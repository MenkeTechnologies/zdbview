//! The interactive terminal application: state, key handling, and rendering.

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
};
use ratatui::{DefaultTerminal, Frame};

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::formats::{self, Decoded, FormatKind};
use crate::hexedit::{self, HexEdit};
use crate::mru::{self, Entry};
use crate::overlay::{HelpCtx, Overlays};
use crate::rkyv_inspect::RkyvStore;
use crate::sqlite::{RowsView, Sort, SqliteStore};
use crate::store::{Kind, Store};
use crate::text::{truncate, truncate_start};
use crate::theme::{Theme, ThemeName};

/// How many rows per SQLite page.
const PAGE: i64 = 500;
/// How long a page fetch may hold the render thread before it is left to finish
/// in the background. Long enough that a page which is already fast arrives
/// before the frame is drawn, short enough to be under one frame either way.
const PAGE_GRACE: std::time::Duration = std::time::Duration::from_millis(30);
/// Minimum length for an extracted rkyv string run.
const MIN_STRING: usize = 4;
/// How many values a column's frequency table shows.
const FREQUENCY_ROWS: i64 = 20;
/// Idle wake-up interval: how often the loop redraws with no input pending, so a
/// toast dismisses itself on time (iftoprs's `event::poll` tick).
const TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// What `.help` prints in the SQL editor. Every line here is a command `run_dot`
/// implements; the completion list in [`crate::sqledit::DOT_COMMANDS`] is the same
/// set.
const DOT_HELP: &str = "\
.tables                 list the tables
.schema [TABLE]         CREATE statements
.indexes [TABLE]        indexes on a table, with their columns
.dump [TABLE]           schema and data as replayable SQL
.databases              attached databases
.attach FILE ALIAS      attach another database
.detach ALIAS           detach it again
.mode MODE              list csv tsv markdown line insert json
.headers on|off         column names in redirected output
.output FILE            send results to a file
.once FILE              send the next result to a file
.timer on|off           report how long a statement took
.eqp on|off             print the query plan before each statement
.import FILE TABLE      load a CSV or TSV
.read FILE              run the statements in a file
.backup FILE            copy the database with VACUUM INTO
.load FILE              load a SQLite extension
.project save|open FILE the session's settings (DB Browser's project file)
.expert                 index advice for the statement just run
.recover [FILE]         salvage rows page by page into a script
.vacuum .analyze .reindex   maintenance
.quit                   leave zdbview";

/// How a result set is rendered — the sqlite3 shell's `.mode`, over the writers in
/// [`crate::export`]. `List` is zdbview's own: the grid the editor draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OutputMode {
    List,
    Csv,
    Tsv,
    Markdown,
    Line,
    Insert,
    Json,
}

impl OutputMode {
    fn parse(name: &str) -> Option<Self> {
        Some(match name.to_lowercase().as_str() {
            "list" | "column" | "box" | "table" => OutputMode::List,
            "csv" => OutputMode::Csv,
            "tsv" | "tabs" => OutputMode::Tsv,
            "markdown" => OutputMode::Markdown,
            "line" => OutputMode::Line,
            "insert" => OutputMode::Insert,
            "json" => OutputMode::Json,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            OutputMode::List => "list",
            OutputMode::Csv => "csv",
            OutputMode::Tsv => "tsv",
            OutputMode::Markdown => "markdown",
            OutputMode::Line => "line",
            OutputMode::Insert => "insert",
            OutputMode::Json => "json",
        }
    }

    /// Render a result set. `table` names the target of `insert` mode, and
    /// `literals` are the same cells as SQL literals — `insert` mode has to use
    /// those, since a display string describes a blob rather than carrying it.
    fn render(
        self,
        columns: &[String],
        rows: &[Vec<String>],
        literals: &[Vec<String>],
        table: &str,
        headers: bool,
    ) -> String {
        use crate::export::*;
        match self {
            OutputMode::Csv => {
                let out = rows_to_csv(columns, rows);
                if headers {
                    out
                } else {
                    out.split_once('\n')
                        .map(|(_, r)| r.to_string())
                        .unwrap_or(out)
                }
            }
            OutputMode::Tsv => {
                let out = rows_to_tsv(columns, rows);
                if headers {
                    out
                } else {
                    out.split_once('\n')
                        .map(|(_, r)| r.to_string())
                        .unwrap_or(out)
                }
            }
            OutputMode::Markdown => rows_to_markdown(columns, rows),
            OutputMode::Line => rows_to_lines(columns, rows),
            OutputMode::Insert => rows_to_inserts_exact(table, columns, literals),
            OutputMode::Json => rows_to_json(columns, rows),
            // The grid has no text form; a redirect falls back to CSV, which is
            // what the shell's default list mode is closest to.
            OutputMode::List => rows_to_csv(columns, rows),
        }
    }
}

/// What the hex editor has open, which decides where `^s` writes.
#[derive(Clone, PartialEq, Eq, Debug)]
enum HexTarget {
    /// A rkyv record's value, written through the archive's re-serialization.
    Record,
    /// A SQLite cell, written back as a blob parameter. Blob cells have no text
    /// form, so the line editor cannot express them. Addressed by key, so a
    /// `WITHOUT ROWID` table's cells are editable too.
    Cell {
        table: String,
        key: crate::sqlite::RowKey,
        column: String,
    },
}

/// Which pane has keyboard focus.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Focus {
    Left,
    Right,
}

/// Modal input state layered over Normal browsing.
enum Mode {
    Normal,
    /// Editing a SQLite cell in place; buffer holds the pending value.
    EditCell(String),
    /// A `/` search prompt; buffer holds the pattern being typed.
    Search(String),
    /// Adding a new rkyv record; buffer holds the key being typed.
    AddRecord(String),
    /// Typing a custom display format for the cursor column, where `%1` stands
    /// for the column — DB Browser's "Custom" display format.
    CustomFormat(String),
    /// Find and replace, step one: what to look for in the cursor column.
    FindText(String),
    /// Find and replace, step two: what to put in its place. The text to find is
    /// in `replace_find`.
    ReplaceText(String),
    /// Naming the view a filter is being saved as.
    ViewName(String),
    /// Renaming a rkyv record's key; buffer holds the new key.
    RenameRecord(String),
    /// Confirm a destructive action (delete row).
    ConfirmDelete,
    /// Confirm dropping a schema object, as `(type, name)`. Dropping a table
    /// takes its rows with it, so it is the one schema edit that asks first.
    ConfirmDrop(String, String),
    /// Leaving the file with changes that have not been written: write them,
    /// revert them, or stay.
    ConfirmClose(Exit),
}

/// Where a confirmed close goes.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Exit {
    Quit,
    /// Back to the file picker, which closes this store just as a quit does.
    Files,
}

/// The views for a rkyv/binary file. `Records` is only available when the
/// archive was recognized and decoded to key/value.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum RkyvView {
    Records,
    Info,
    Strings,
    Hex,
}

/// Where the cursor was when `/` was pressed. An incremental search always looks
/// from here, so the match moves with the pattern instead of walking forward one
/// hop per keystroke, and Esc puts things back.
#[derive(Debug, Clone, Copy)]
struct SearchOrigin {
    table_idx: usize,
    page_offset: i64,
    row_idx: usize,
    record_idx: usize,
    string_idx: usize,
    hex_row: usize,
}

/// Why the app loop ended: the user quit, or asked for the file picker again.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Outcome {
    Quit,
    /// `o` — back to the picker to open another file.
    Reopen,
}

/// Top-level screen. Overlaid modals (`Mode`) and the help overlay sit on top.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Screen {
    Main,
    /// Full-screen detail of one row/record with a scrollable value pane.
    Detail,
    /// Hex editor over one record's value bytes.
    HexEdit,
    /// Live write monitor over every store zdbview knows about.
    Top,
    /// The SQL editor: multi-line input, transcript, completion.
    Sql,
    /// Database facts, integrity check and foreign-key lint (`.dbinfo`, `.intck`,
    /// `.lint fkey-indexes` in the sqlite3 shell).
    DbInfo,
    /// Per-column statistics for the current table, and one column's frequency
    /// table (VisiData's `describe` and frequency sheet, sqlite-utils'
    /// `analyze-tables`).
    Stats,
    /// The write-ahead log, frame by frame, with the rows each frame wrote.
    Frames,
    /// SQLite schema (CREATE statements) view.
    Schema,
    /// The table designer — DB Browser's "Edit Table Definition".
    TableDesign,
    /// The index designer — DB Browser's "Edit Index".
    IndexDesign,
    /// The editable pragmas — DB Browser's "Edit Pragmas" tab.
    Pragmas,
    /// The insert form — DB Browser's "Insert Values".
    InsertRow,
    /// The conditional-format rules for one column — DB Browser's Conditional
    /// Formats manager.
    CondFormat,
}

/// How the value pane renders raw bytes.
#[derive(PartialEq, Eq, Clone, Copy)]
enum ValueRender {
    Auto,
    Hex,
    Text,
    /// Disassemble the value as a fusevm::Chunk (requires the `disasm` feature).
    Disasm,
}

impl ValueRender {
    fn label(self) -> &'static str {
        match self {
            ValueRender::Auto => "auto",
            ValueRender::Hex => "hex",
            ValueRender::Text => "text",
            ValueRender::Disasm => "disasm",
        }
    }
    fn next(self) -> Self {
        match self {
            ValueRender::Auto => ValueRender::Hex,
            ValueRender::Hex => ValueRender::Text,
            ValueRender::Text => ValueRender::Disasm,
            ValueRender::Disasm => ValueRender::Auto,
        }
    }
}

pub struct App {
    store: Store,
    focus: Focus,
    mode: Mode,
    status: String,
    quit: bool,

    // SQLite state
    table_idx: usize,
    rows: Option<RowsView>,
    page_offset: i64,
    row_idx: usize,
    col_idx: usize,

    // rkyv state
    rkyv_view: RkyvView,
    strings: Vec<crate::rkyv_inspect::StringHit>,
    string_idx: usize,
    hex_row: usize,
    /// Decoded key/value records when the archive was recognized.
    decoded: Option<Decoded>,
    record_idx: usize,

    /// True after a lone `g`, awaiting the second `g` of a `gg` motion.
    pending_g: bool,
    /// Active search pattern (empty = no search); `n`/`N` cycle its matches.
    search: String,

    // Screens / overlays
    screen: Screen,
    value_render: ValueRender,
    /// Cached bytes shown in the Detail value pane.
    detail_value: Vec<u8>,
    detail_scroll: usize,
    schema_scroll: usize,
    /// SQLite schema objects `(type, name, sql)`, loaded lazily.
    schema: Vec<(String, String, String)>,
    /// Which schema object the cursor is on, indexing `schema`.
    schema_idx: usize,
    /// Row counts for the schema screen, once they have been asked for — DB
    /// Browser's "Row counts", which is an action rather than a column because
    /// counting every table means scanning every table.
    schema_counts: std::collections::HashMap<String, i64>,
    /// The table designer, while `screen` is `Screen::TableDesign`.
    design: Option<crate::designer::TableDesigner>,
    /// The index designer, while `screen` is `Screen::IndexDesign`.
    index_design: Option<crate::designer::IndexDesigner>,
    /// The pragma form, while `screen` is `Screen::Pragmas`.
    pragmas: Option<crate::pragmas::PragmaEditor>,
    /// Per-table grid settings: hidden columns, frozen columns, the rowid
    /// column, display formats — DB Browser's Browse Data settings.
    browse: crate::browse::Browse,
    /// True while the editor's statement is running, which is when the stop key
    /// is watched. See [`App::install_sql_stop`].
    sql_running: Arc<std::sync::atomic::AtomicBool>,
    /// Set by the stop key, so the outcome can say the statement was stopped
    /// rather than that it failed.
    sql_stopped: Arc<std::sync::atomic::AtomicBool>,
    /// First scrolling column on screen, so a wide table can be walked sideways
    /// without the window jumping back and forth.
    col_scroll: usize,
    /// What find-and-replace is looking for, between its two prompts.
    replace_find: String,
    /// The insert form, while `screen` is `Screen::InsertRow`.
    insert_form: Option<crate::browse::RowForm>,
    /// The conditional-format manager, while `screen` is `Screen::CondFormat`.
    cond_format: Option<crate::browse::RulesEditor>,
    /// Views the user has unlocked for editing this session — DB Browser's
    /// "Unlock view editing".
    unlocked_views: Vec<String>,

    // Mouse hit-testing: the on-screen rect and scroll offset of each clickable
    // list/grid, captured during render so a click maps to the right index.
    click_left: Rect,
    click_right: Rect,
    click_records: Rect,
    off_left: usize,
    off_right: usize,
    off_records: usize,
    /// Byte offset of the text cursor within the active input modal's buffer.
    input_cursor: usize,
    /// The open hex editor, while `screen` is `Screen::HexEdit`.
    hex: Option<HexEdit>,
    /// Rect the hex editor last rendered into, for click hit-testing.
    hex_area: Rect,
    /// The write monitor, while `screen` is `Screen::Top`.
    top: Option<crate::monitor::Monitor>,
    /// Rect the monitor last rendered into, for its header hit test.
    top_area: Rect,
    /// The SQL editor, while `screen` is `Screen::Sql`.
    sql: Option<crate::sqledit::SqlEdit>,
    /// The log walker, while `screen` is `Screen::Frames`.
    walk: Option<crate::frames::FrameView>,
    /// Which screen the hex editor was opened from, so closing it goes back there
    /// rather than always to the grid.
    hex_from: Screen,
    /// What the open hex editor writes back to. A rkyv record by default; a
    /// SQLite blob cell when the grid's cell could not be edited as text.
    hex_target: HexTarget,
    /// A statistics pass running on another thread, with the table it is for.
    stats_pending: Option<(String, std::sync::mpsc::Receiver<StatsResult>)>,
    /// Per-column statistics, built when the statistics screen opens.
    stats: Vec<crate::sqlite::ColumnStat>,
    stats_idx: usize,
    /// The column whose frequency table is shown, with its top values.
    stats_freq: Option<(String, Vec<(String, i64)>)>,
    /// Lines of the database-info screen, built when it opens.
    dbinfo: Vec<(String, String)>,
    /// Integrity-check and lint output, filled on demand (both can be slow).
    dbinfo_checks: Vec<String>,
    dbinfo_scroll: usize,
    /// Furthest the database screen can scroll, measured while rendering it —
    /// the report's length depends on how many checks have been run and on the
    /// height it was given, neither of which the key handler can see.
    dbinfo_max_scroll: usize,
    /// Rect the SQL editor last rendered into, for paging.
    sql_area: Rect,
    /// How the editor renders a result set — the shell's `.mode`.
    sql_mode: OutputMode,
    /// `.output` / `.once`: where results go instead of the transcript, and
    /// whether the redirect ends after the next statement.
    sql_out: Option<(PathBuf, bool)>,
    /// `.headers off` drops the column names from redirected output.
    sql_headers: bool,
    /// `.timer off` stops reporting how long a statement took.
    sql_timer: bool,
    /// `.eqp on` prints each statement's plan before running it.
    sql_eqp: bool,
    /// Set by `o`: leave the app loop and show the file picker again.
    reopen: bool,
    /// A file the monitor asked to open, taking precedence over the picker.
    open_next: Option<PathBuf>,
    /// Position `/` started from, while a search prompt is open.
    search_origin: Option<SearchOrigin>,
    /// Active `/` filter: only matching rows/records/strings are listed. Empty
    /// when nothing is filtered.
    filter: String,
    /// The extracted string list hit its bounds, so it is not exhaustive.
    strings_truncated: bool,
    /// Bytes the string scan covered (the whole file unless it was bounded).
    strings_scanned: usize,
    /// A decode running on another thread, for an archive too big to validate
    /// while the user waits.
    decoding: Option<std::sync::mpsc::Receiver<Option<Decoded>>>,
    /// Rows of the focused scrollable region in the last frame. Paging moves by
    /// this much, so PageUp/PageDown match what is actually on screen instead of
    /// a fixed guess.
    page_rows: usize,
    /// Active row-grid ordering, or `None` for the table's natural `rowid`
    /// order. Reset when another table is selected.
    sort: Option<Sort>,

    /// Themed overlays (help / scheme chooser / palette editor / toast),
    /// shared with the recent-files picker.
    ov: Overlays,

    /// Off-thread page and count queries, for a SQLite store. `None` for rkyv,
    /// which holds its whole archive in memory and needs no query at all.
    engine: Option<crate::query::Engine>,
    /// Generation of the page request the grid is waiting for. A result tagged
    /// with anything older describes a table, filter or page that has since been
    /// replaced.
    page_generation: u64,
    /// `G` was pressed before the exact total was known, so the jump to the last
    /// page is owed as soon as the count lands.
    pending_bottom: bool,
    /// Offset of the page actually loaded, which lags `page_offset` while a fetch
    /// is in flight. Paging by cursor needs to know which page the rows on screen
    /// are.
    loaded_offset: i64,
    /// `(table, filter, rows)` from the last exact count, kept while it still
    /// describes the grid so the scan is not repeated for every page.
    exact_total: Option<(String, String, i64)>,
}

impl App {
    /// Open `store` with an already-resolved scheme. The picker hands its own
    /// scheme over this way, so opening a file cannot re-read prefs and land on a
    /// different one.
    pub fn with_theme(store: Store, theme: Theme) -> Self {
        let mut app = App {
            store,
            focus: Focus::Left,
            mode: Mode::Normal,
            status: String::new(),
            quit: false,
            table_idx: 0,
            rows: None,
            page_offset: 0,
            row_idx: 0,
            col_idx: 0,
            rkyv_view: RkyvView::Info,
            strings: Vec::new(),
            string_idx: 0,
            hex_row: 0,
            decoded: None,
            record_idx: 0,
            pending_g: false,
            search: String::new(),
            screen: Screen::Main,
            value_render: ValueRender::Auto,
            detail_value: Vec::new(),
            detail_scroll: 0,
            schema_scroll: 0,
            schema: Vec::new(),
            schema_idx: 0,
            schema_counts: Default::default(),
            design: None,
            index_design: None,
            pragmas: None,
            browse: Default::default(),
            sql_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sql_stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            col_scroll: 0,
            replace_find: String::new(),
            insert_form: None,
            cond_format: None,
            unlocked_views: Vec::new(),
            click_left: Rect::ZERO,
            click_right: Rect::ZERO,
            click_records: Rect::ZERO,
            off_left: 0,
            off_right: 0,
            off_records: 0,
            input_cursor: 0,
            sort: None,
            hex: None,
            hex_area: Rect::ZERO,
            top: None,
            top_area: Rect::ZERO,
            sql: None,
            walk: None,
            hex_from: Screen::Main,
            hex_target: HexTarget::Record,
            stats_pending: None,
            stats: Vec::new(),
            stats_idx: 0,
            stats_freq: None,
            dbinfo: Vec::new(),
            dbinfo_checks: Vec::new(),
            dbinfo_scroll: 0,
            dbinfo_max_scroll: 0,
            sql_area: Rect::ZERO,
            sql_mode: OutputMode::List,
            sql_out: None,
            sql_headers: true,
            sql_timer: true,
            sql_eqp: false,
            reopen: false,
            open_next: None,
            search_origin: None,
            filter: String::new(),
            strings_truncated: false,
            strings_scanned: 0,
            decoding: None,
            page_rows: 10,
            ov: Overlays::new(theme),
            engine: None,
            page_generation: 0,
            pending_bottom: false,
            loaded_offset: 0,
            exact_total: None,
        };
        // The grid's queries run on their own connections, so a scan the user
        // cannot see never blocks the render thread.
        if let Store::Sqlite(s) = &app.store {
            app.engine = Some(crate::query::Engine::new(&s.path));
        }
        app.init();
        app
    }

    /// Leave the open file and ask for the picker again (`o`, or `Esc` on the
    /// first level).
    fn back_to_files(&mut self) {
        // Closing the store rolls its open savepoint back, so an unwritten
        // change has to be answered for first.
        if self.guard_pending(Exit::Files) {
            return;
        }
        self.reopen = true;
        self.quit = true;
    }

    /// A file the write monitor asked to open next, if any.
    pub fn open_next(&mut self) -> Option<PathBuf> {
        self.open_next.take()
    }

    /// The scheme currently in use, to carry back to the picker.
    pub fn theme(&self) -> Theme {
        self.ov.theme
    }

    /// Screens that take every printable key for themselves, so the global
    /// bindings must stay out of the way: the hex editor (letters are hex digits
    /// and motions) and the SQL editor (letters are the statement).
    fn screen_owns_keys(&self) -> bool {
        matches!(
            self.screen,
            // The designers and the two forms take every printable key: `c`, `h`
            // and `w` are text in a column name — or a rule's condition — as
            // readily as anywhere else.
            Screen::HexEdit
                | Screen::Sql
                | Screen::TableDesign
                | Screen::IndexDesign
                | Screen::InsertRow
                | Screen::CondFormat
        )
    }

    /// Which key sections the help overlay lists for what is on screen.
    fn help_ctx(&self) -> HelpCtx {
        if self.screen == Screen::HexEdit {
            return HelpCtx::HexEdit;
        }
        if self.screen == Screen::Top {
            return HelpCtx::Top;
        }
        if self.screen == Screen::Sql {
            return HelpCtx::Sql;
        }
        if self.screen == Screen::DbInfo {
            return HelpCtx::DbInfo;
        }
        if self.screen == Screen::Stats {
            return HelpCtx::Stats;
        }
        if self.screen == Screen::Frames {
            return HelpCtx::Frames;
        }
        if matches!(
            self.screen,
            Screen::Schema | Screen::TableDesign | Screen::IndexDesign
        ) {
            return HelpCtx::Schema;
        }
        if self.screen == Screen::Pragmas {
            return HelpCtx::Pragmas;
        }
        match &self.store {
            Store::Sqlite(_) => HelpCtx::Sqlite,
            Store::Rkyv(_) => HelpCtx::Rkyv,
        }
    }

    /// Report an action's result: a transient toast over the UI plus the same
    /// text in the status bar, so it stays readable after the toast fades.
    fn notify(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.ov.toast(msg.clone());
        self.status = msg;
    }

    fn init(&mut self) {
        match &self.store {
            Store::Sqlite(s) => {
                if !s.tables.is_empty() {
                    self.load_table();
                }
                self.status = "j/k ←/→ move · Tab focus · / filter · ^f/^b page · e edit · a add · d delete · : SQL · s sort · c scheme · o/Esc files · h help · q quit".into();
            }
            Store::Rkyv(r) => {
                let s = r.strings(MIN_STRING);
                self.strings_truncated = s.truncated;
                self.strings_scanned = s.scanned;
                self.strings = s.hits;
                // Validating a large archive is slow — 25s for a 382MB shard —
                // so it runs on a thread and the structural view opens at once.
                // The Records view appears when the decode lands.
                if r.bytes.len() > DECODE_INLINE_MAX {
                    self.decoding = Some(spawn_decode(r.bytes.clone()));
                    self.rkyv_view = RkyvView::Info;
                    self.status = format!(
                        "decoding {} in the background · 1 Info · 2 Strings · 3 Hex · o/Esc files · q quit",
                        human_size(r.bytes.len() as u64)
                    );
                    return;
                }
                self.decoded = formats::try_decode(&r.bytes);
                if let Some(d) = &self.decoded {
                    self.rkyv_view = RkyvView::Records;
                    self.status = format!(
                        "{} · {} records · Enter detail · a add e hex-edit r rename d delete · / filter · 0/1/2/3 views · c scheme · o/Esc files · h help · q quit",
                        d.format,
                        d.records.len()
                    );
                } else {
                    self.status = "1 Info · 2 Strings · 3 Hex · j/k scroll · / filter · c scheme · o/Esc files · h help · q quit  (rkyv: unrecognized)".into();
                }
            }
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<Outcome> {
        while !self.quit {
            self.poll_decode();
            self.poll_stats();
            self.poll_query();
            if let Some(w) = self.top.as_mut() {
                w.tick();
            }
            terminal.draw(|f| self.render(f))?;
            if !event::poll(TICK)? {
                self.ov.expire_toast();
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
                Event::Mouse(m) => self.on_mouse(m),
                _ => {}
            }
            self.ov.expire_toast();
        }
        Ok(if self.reopen {
            Outcome::Reopen
        } else {
            Outcome::Quit
        })
    }

    // ----- key handling -----------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        let code = key.code;

        // An open overlay owns the key.
        if self.ov.active() {
            self.ov.on_key(code);
            return;
        }

        // Modal input first. Snapshot the buffer into a local so no borrow of
        // `self.mode` is held across the `&mut self` dispatch call.
        enum Modal {
            Edit(String),
            Search(String),
            Add(String),
            Format(String),
            Find(String),
            Replace(String),
            ViewName(String),
            Rename(String),
            Confirm,
            Drop(String, String),
            Close(Exit),
            None,
        }
        let modal = match &self.mode {
            Mode::EditCell(buf) => Modal::Edit(buf.clone()),
            Mode::Search(buf) => Modal::Search(buf.clone()),
            Mode::AddRecord(buf) => Modal::Add(buf.clone()),
            Mode::CustomFormat(buf) => Modal::Format(buf.clone()),
            Mode::FindText(buf) => Modal::Find(buf.clone()),
            Mode::ReplaceText(buf) => Modal::Replace(buf.clone()),
            Mode::ViewName(buf) => Modal::ViewName(buf.clone()),
            Mode::RenameRecord(buf) => Modal::Rename(buf.clone()),
            Mode::ConfirmDelete => Modal::Confirm,
            Mode::ConfirmDrop(ty, name) => Modal::Drop(ty.clone(), name.clone()),
            Mode::ConfirmClose(exit) => Modal::Close(*exit),
            Mode::Normal => Modal::None,
        };
        match modal {
            Modal::Edit(buf) => {
                return self.key_input(key, buf, Mode::EditCell, App::commit_edit_cell)
            }
            Modal::Search(buf) => {
                // The arrows (and paging) move through the filtered list while the
                // prompt stays open — the pattern only takes Left/Right for its
                // own cursor.
                match code {
                    KeyCode::Up => {
                        self.move_selection(-1);
                        return;
                    }
                    KeyCode::Down => {
                        self.move_selection(1);
                        return;
                    }
                    KeyCode::PageUp => {
                        let step = self.page_step() as isize;
                        self.move_selection(-step);
                        return;
                    }
                    KeyCode::PageDown => {
                        let step = self.page_step() as isize;
                        self.move_selection(step);
                        return;
                    }
                    _ => {}
                }
                return self.key_input(key, buf, Mode::Search, App::commit_search);
            }
            Modal::Add(buf) => {
                return self.key_input(key, buf, Mode::AddRecord, App::commit_add_record)
            }
            Modal::Format(buf) => {
                return self.key_input(key, buf, Mode::CustomFormat, App::commit_custom_format)
            }
            Modal::Find(buf) => return self.key_input(key, buf, Mode::FindText, App::commit_find),
            Modal::Replace(buf) => {
                return self.key_input(key, buf, Mode::ReplaceText, App::commit_replace)
            }
            Modal::ViewName(buf) => {
                return self.key_input(key, buf, Mode::ViewName, App::commit_view_name)
            }
            Modal::Rename(buf) => {
                return self.key_input(key, buf, Mode::RenameRecord, App::commit_rename_record)
            }
            Modal::Confirm => return self.key_confirm_delete(code),
            Modal::Drop(ty, name) => return self.key_confirm_drop(code, &ty, &name),
            Modal::Close(exit) => return self.key_confirm_close(code, exit),
            Modal::None => {}
        }

        // The overlay openers (`h`/`?` help, `c` chooser, `C` editor) work from
        // every screen, exactly as on the recent-files picker — except on the
        // screens that own their keys: the hex editor's motions and the SQL
        // editor's text.
        if !self.screen_owns_keys() && self.ov.on_key(code) {
            return;
        }

        // `w` opens the write monitor from any screen that does not own its keys,
        // and not from the monitor itself, where it closes.
        if code == KeyCode::Char('w') && !self.screen_owns_keys() && self.screen != Screen::Top {
            self.open_top();
            return;
        }

        // `o` goes back to the file picker from any screen but the hex editor,
        // where it inserts a byte.
        if code == KeyCode::Char('o') && !self.screen_owns_keys() {
            self.back_to_files();
            return;
        }

        match self.screen {
            // The hex editor is modal: it owns every key, including `h` and `c`,
            // because those are its own motions and data.
            Screen::HexEdit => return self.key_hex(key),
            Screen::Top => return self.key_top(code),
            Screen::Sql => return self.key_sql(key),
            Screen::DbInfo => return self.key_dbinfo(code),
            Screen::Stats => return self.key_stats(code),
            Screen::Frames => return self.key_frames(code),
            Screen::Detail => return self.key_detail(code),
            Screen::Schema => return self.key_schema(code),
            Screen::TableDesign => return self.key_table_design(key),
            Screen::IndexDesign => return self.key_index_design(key),
            Screen::Pragmas => return self.key_pragmas(key),
            Screen::InsertRow => return self.key_insert_row(key),
            Screen::CondFormat => return self.key_cond_format(key),
            Screen::Main => {}
        }

        match &self.store {
            Store::Sqlite(_) => self.key_sqlite(key),
            Store::Rkyv(_) => self.key_rkyv(key),
        }
    }

    // ----- mouse (ported from iftoprs `handle_mouse`) -----------------------

    fn on_mouse(&mut self, m: MouseEvent) {
        // An open overlay owns the event (wheel drives it, a click confirms).
        if self.ov.on_mouse(m) {
            return;
        }
        // The write monitor takes the wheel and clicks (its header sorts).
        if self.screen == Screen::Top {
            let area = self.top_area;
            if let Some(mon) = self.top.as_mut() {
                mon.on_mouse(m, area);
                if let Some(note) = mon.note.take() {
                    self.notify(note);
                }
            }
            return;
        }
        // In the hex editor the wheel scrolls the dump and a click places the
        // cursor on the byte under it.
        if self.screen == Screen::HexEdit {
            let area = self.hex_area;
            if let Some(ed) = self.hex.as_mut() {
                ed.on_mouse(m, area);
            }
            return;
        }
        // Scroll wheel reuses the existing up/down navigation for the active
        // screen/view (rows, records, hex, strings, detail, schema).
        match m.kind {
            MouseEventKind::ScrollDown => self.scroll_select(true),
            MouseEventKind::ScrollUp => self.scroll_select(false),
            MouseEventKind::Down(MouseButton::Left) => self.click_at(m.column, m.row, false),
            MouseEventKind::Down(MouseButton::Right) => self.click_at(m.column, m.row, true),
            _ => {}
        }
    }

    fn scroll_select(&mut self, down: bool) {
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        let code = if down { KeyCode::Down } else { KeyCode::Up };
        self.on_key(KeyEvent::new(code, KeyModifiers::empty()));
    }

    /// Left/right click at `(col,row)`: select the item under the cursor. Right
    /// click additionally opens the detail screen for it (like iftoprs's
    /// right-click-shows-details). The clickable rects and their scroll offsets
    /// are captured during render, so the mapping is correct even when scrolled.
    fn click_at(&mut self, col: u16, row: u16, right: bool) {
        if !matches!(self.mode, Mode::Normal) || self.screen != Screen::Main {
            return;
        }
        match &self.store {
            Store::Sqlite(_) => {
                if hit(self.click_left, col, row) {
                    self.focus = Focus::Left;
                    let idx = self.off_left + row.saturating_sub(self.click_left.y + 1) as usize;
                    self.select_table(idx);
                } else if hit(self.click_right, col, row) {
                    self.focus = Focus::Right;
                    // +2 for the top border and the header row.
                    let idx = self.off_right + row.saturating_sub(self.click_right.y + 2) as usize;
                    let n = self.rows.as_ref().map(|r| r.rows.len()).unwrap_or(0);
                    if idx < n {
                        self.row_idx = idx;
                        if right {
                            self.enter_detail();
                        }
                    }
                }
            }
            Store::Rkyv(_) => {
                if self.rkyv_view == RkyvView::Records && hit(self.click_records, col, row) {
                    let idx =
                        self.off_records + row.saturating_sub(self.click_records.y + 1) as usize;
                    let n = self.decoded.as_ref().map(|d| d.records.len()).unwrap_or(0);
                    if idx < n {
                        self.record_idx = idx;
                        if right {
                            self.enter_detail();
                        }
                    }
                }
            }
        }
    }

    // ----- Detail / Schema / export / clipboard screens ---------------------

    fn key_detail(&mut self, code: KeyCode) {
        let max_scroll = self.detail_value.len() / 16;
        match code {
            KeyCode::Char('q') => {
                if !self.guard_pending(Exit::Quit) {
                    self.quit = true;
                }
            }
            KeyCode::Esc | KeyCode::Enter => {
                self.screen = Screen::Main;
                self.detail_scroll = 0;
            }
            KeyCode::Char('v') => self.value_render = self.value_render.next(),
            KeyCode::Char('y') => self.copy_detail_value(),
            // The detail screen shows the whole value, which is where you want to
            // change it — `e` edits the highlighted field without going back, and
            // the arrows pick which field that is.
            KeyCode::Char('e') => self.edit_current_value(),
            KeyCode::Char('E') => self.edit_cell_as_bytes(),
            KeyCode::Right => {
                let n = self
                    .rows
                    .as_ref()
                    .map(|r| r.columns.len())
                    .unwrap_or(0)
                    .saturating_sub(1);
                if self.col_idx < n {
                    self.col_idx += 1;
                    self.detail_scroll = 0;
                    self.load_detail_value(false);
                }
            }
            KeyCode::Left => {
                if self.col_idx > 0 {
                    self.col_idx -= 1;
                    self.detail_scroll = 0;
                    self.load_detail_value(false);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.detail_scroll < max_scroll {
                    self.detail_scroll += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1)
            }
            KeyCode::Char('g') => self.detail_scroll = 0,
            KeyCode::Char('G') => self.detail_scroll = max_scroll,
            KeyCode::PageDown => {
                self.detail_scroll = (self.detail_scroll + self.page_step()).min(max_scroll)
            }
            KeyCode::PageUp => {
                self.detail_scroll = self.detail_scroll.saturating_sub(self.page_step())
            }
            _ => {}
        }
    }

    /// The schema screen is DB Browser's Database Structure tab: the objects,
    /// their statements, and the edits that act on the object under the cursor.
    fn key_schema(&mut self, code: KeyCode) {
        let last = self.schema.len().saturating_sub(1);
        match code {
            KeyCode::Char('q') => {
                if !self.guard_pending(Exit::Quit) {
                    self.quit = true;
                }
            }
            KeyCode::Esc | KeyCode::Char('S') => {
                self.screen = Screen::Main;
                self.schema_scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => self.schema_idx = (self.schema_idx + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.schema_idx = self.schema_idx.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => self.schema_idx = 0,
            KeyCode::Char('G') | KeyCode::End => self.schema_idx = last,
            KeyCode::PageDown => self.schema_idx = (self.schema_idx + self.page_step()).min(last),
            KeyCode::PageUp => self.schema_idx = self.schema_idx.saturating_sub(self.page_step()),
            KeyCode::Enter | KeyCode::Char('e') => self.open_designer(),
            KeyCode::Char('a') => self.open_new_table(),
            KeyCode::Char('i') => self.open_new_index(),
            KeyCode::Char('d') => {
                if let Some((ty, name, _)) = self.schema.get(self.schema_idx) {
                    self.mode = Mode::ConfirmDrop(ty.clone(), name.clone());
                }
            }
            KeyCode::Char('y') => self.copy_create_statement(),
            KeyCode::Char('R') => self.count_schema_rows(),
            _ => {}
        }
    }

    /// `R` on the schema screen: how many rows each table and view holds — DB
    /// Browser's "Row counts". Pressing it again drops them, since a count is a
    /// scan and the numbers age the moment anything writes.
    fn count_schema_rows(&mut self) {
        if !self.schema_counts.is_empty() {
            self.schema_counts.clear();
            self.notify("row counts cleared");
            return;
        }
        let names: Vec<String> = self
            .schema
            .iter()
            .filter(|(ty, _, _)| ty == "table" || ty == "view")
            .map(|(_, name, _)| name.clone())
            .collect();
        let counts: Vec<(String, i64)> = match self.sqlite() {
            Some(s) => names
                .into_iter()
                .filter_map(|n| s.count_exact(&n, "").ok().map(|c| (n, c)))
                .collect(),
            None => return,
        };
        let n = counts.len();
        self.schema_counts = counts.into_iter().collect();
        self.notify(format!(
            "counted {n} object{}",
            if n == 1 { "" } else { "s" }
        ));
    }

    /// `Enter`/`e` on the schema screen: open whichever designer fits the object.
    fn open_designer(&mut self) {
        let (ty, name) = match self.schema.get(self.schema_idx) {
            Some((ty, name, _)) => (ty.clone(), name.clone()),
            None => return,
        };
        match ty.as_str() {
            "table" => match self.sqlite().map(|s| s.table_def(&name)) {
                Some(Ok(def)) => {
                    self.design = Some(crate::designer::TableDesigner::edit(def));
                    self.screen = Screen::TableDesign;
                }
                Some(Err(e)) => self.notify(e.to_string()),
                None => {}
            },
            "index" => {
                let store = match self.sqlite() {
                    Some(s) => s,
                    None => return,
                };
                match store.index_def(&name) {
                    Ok(def) => {
                        let cols = store.columns(&def.table).unwrap_or_default();
                        self.index_design = Some(crate::designer::IndexDesigner::edit(def, cols));
                        self.screen = Screen::IndexDesign;
                    }
                    Err(e) => self.notify(e.to_string()),
                }
            }
            other => self.notify(format!(
                "{other}s have no designer — edit one as SQL with `:` (DROP then CREATE)"
            )),
        }
    }

    fn open_new_table(&mut self) {
        if self.sqlite().is_none() {
            return;
        }
        self.design = Some(crate::designer::TableDesigner::create());
        self.screen = Screen::TableDesign;
    }

    /// `i` on the schema screen: a new index on the table under the cursor, or on
    /// the table the object under the cursor belongs to.
    fn open_new_index(&mut self) {
        let table = match self.schema.get(self.schema_idx) {
            Some((ty, name, _)) if ty == "table" => name.clone(),
            Some((ty, name, _)) if ty == "index" => self
                .sqlite()
                .and_then(|s| s.index_def(name).ok())
                .map(|d| d.table)
                .unwrap_or_default(),
            _ => self.current_table().unwrap_or_default(),
        };
        if table.is_empty() {
            self.notify("select a table to index");
            return;
        }
        let cols = self
            .sqlite()
            .and_then(|s| s.columns(&table).ok())
            .unwrap_or_default();
        self.index_design = Some(crate::designer::IndexDesigner::create(&table, cols));
        self.screen = Screen::IndexDesign;
    }

    /// `y` on the schema screen — DB Browser's "Copy Create Statement".
    fn copy_create_statement(&mut self) {
        let sql = match self.schema.get(self.schema_idx) {
            Some((_, _, sql)) if !sql.is_empty() => sql.clone(),
            Some((ty, name, _)) => {
                self.notify(format!("{ty} {name} has no statement — SQLite created it"));
                return;
            }
            None => return,
        };
        let ok = crate::clipboard::copy(&sql);
        self.notify(if ok {
            format!("copied {} bytes to clipboard", sql.len())
        } else {
            "clipboard unavailable (no tty)".into()
        });
    }

    fn key_confirm_drop(&mut self, code: KeyCode, ty: &str, name: &str) {
        self.mode = Mode::Normal;
        if !matches!(code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            return;
        }
        let sql = match crate::ddl::drop_sql(ty, name) {
            Some(s) => s,
            None => return,
        };
        let plan = crate::ddl::AlterPlan {
            statements: vec![sql],
            rebuild: false,
        };
        match self.sqlite().map(|s| s.apply_ddl(&plan)) {
            Some(Ok(())) => {
                self.after_schema_edit(format!("dropped {ty} {name}"));
            }
            Some(Err(e)) => self.notify(format!("drop failed: {e}")),
            None => {}
        }
    }

    fn key_table_design(&mut self, key: KeyEvent) {
        let action = match self.design.as_mut() {
            Some(d) => d.on_key(key),
            None => {
                self.screen = Screen::Schema;
                return;
            }
        };
        match action {
            crate::designer::Action::None => {}
            crate::designer::Action::Note(msg) => self.notify(msg),
            crate::designer::Action::Cancel => {
                self.design = None;
                self.screen = Screen::Schema;
            }
            crate::designer::Action::Apply => self.apply_table_design(),
        }
    }

    fn apply_table_design(&mut self) {
        let designer = match self.design.as_ref() {
            Some(d) => d,
            None => return,
        };
        // A rebuild has to put the table's indexes, triggers and views back, so
        // they are read before anything is dropped.
        let aux = match (&designer.original, self.sqlite()) {
            (Some(old), Some(store)) => store.dependents(&old.name).unwrap_or_default(),
            _ => Vec::new(),
        };
        let plan = match designer.plan(&aux) {
            Ok(p) => p,
            Err(e) => return self.notify(e),
        };
        let name = designer.def.name.clone();
        let created = designer.original.is_none();
        match self.sqlite().map(|s| s.apply_ddl(&plan)) {
            Some(Ok(())) => {
                self.design = None;
                self.screen = Screen::Schema;
                let what = if created { "created" } else { "wrote" };
                let how = if plan.rebuild { " (rebuilt)" } else { "" };
                self.after_schema_edit(format!("{what} table {name}{how}"));
            }
            Some(Err(e)) => self.notify(format!("write failed: {e}")),
            None => {}
        }
    }

    fn key_index_design(&mut self, key: KeyEvent) {
        let action = match self.index_design.as_mut() {
            Some(d) => d.on_key(key),
            None => {
                self.screen = Screen::Schema;
                return;
            }
        };
        match action {
            crate::designer::Action::None => {}
            crate::designer::Action::Note(msg) => self.notify(msg),
            crate::designer::Action::Cancel => {
                self.index_design = None;
                self.screen = Screen::Schema;
            }
            crate::designer::Action::Apply => self.apply_index_design(),
        }
    }

    fn apply_index_design(&mut self) {
        let designer = match self.index_design.as_ref() {
            Some(d) => d,
            None => return,
        };
        let plan = match designer.plan() {
            Ok(p) => p,
            Err(e) => return self.notify(e),
        };
        let name = designer.def.name.clone();
        match self.sqlite().map(|s| s.apply_ddl(&plan)) {
            Some(Ok(())) => {
                self.index_design = None;
                self.screen = Screen::Schema;
                self.after_schema_edit(format!("wrote index {name}"));
            }
            Some(Err(e)) => self.notify(format!("write failed: {e}")),
            None => {}
        }
    }

    /// After any schema edit: every cached fact about the database is stale, the
    /// object list has changed, and the grid may be looking at a table that no
    /// longer has the columns it is displaying.
    fn after_schema_edit(&mut self, note: String) {
        self.schema_changed();
        if let Some(s) = self.sqlite() {
            self.schema = s.schema().unwrap_or_default();
        }
        self.schema_idx = self.schema_idx.min(self.schema.len().saturating_sub(1));
        // The grid may be on a table that has just been renamed, dropped or
        // reshaped, so it is re-selected rather than left showing stale columns.
        self.select_table(self.table_idx);
        self.notify(note);
    }

    /// Enter the detail screen for the current SQLite row or rkyv record.
    fn enter_detail(&mut self) {
        self.detail_scroll = 0;
        self.load_detail_value(true);
    }

    /// Read the selected cell / record value into `detail_value`. `enter` also
    /// switches to the screen; a reload after an edit leaves the screen alone.
    fn load_detail_value(&mut self, enter: bool) {
        match &self.store {
            Store::Sqlite(_) => {
                let bytes = match (self.current_table(), self.current_rowid()) {
                    (Some(t), Some(rid)) => {
                        let col = self
                            .rows
                            .as_ref()
                            .and_then(|r| r.columns.get(self.col_idx).cloned())
                            .unwrap_or_default();
                        self.sqlite()
                            .and_then(|s| {
                                s.cell_bytes_keyed(&t, &crate::sqlite::RowKey::Rowid(rid), &col)
                                    .ok()
                            })
                            .unwrap_or_default()
                    }
                    _ => self
                        .rows
                        .as_ref()
                        .and_then(|r| r.rows.get(self.row_idx))
                        .and_then(|row| row.get(self.col_idx))
                        .map(|s| s.clone().into_bytes())
                        .unwrap_or_default(),
                };
                self.detail_value = bytes;
                if enter {
                    self.screen = Screen::Detail;
                }
            }
            Store::Rkyv(_) => {
                if let Some(rec) = self
                    .decoded
                    .as_ref()
                    .and_then(|d| d.records.get(self.record_idx))
                {
                    self.detail_value = rec.value.clone();
                    if enter {
                        self.screen = Screen::Detail;
                    }
                }
            }
        }
    }

    fn open_schema(&mut self) {
        if let Some(s) = self.sqlite() {
            self.schema = s.schema().unwrap_or_default();
            self.schema_scroll = 0;
            self.screen = Screen::Schema;
        }
    }

    fn copy_detail_value(&mut self) {
        let text = match self.value_render {
            ValueRender::Text | ValueRender::Auto if looks_textual(&self.detail_value) => {
                String::from_utf8_lossy(&self.detail_value).into_owned()
            }
            _ => hex_string(&self.detail_value),
        };
        let ok = crate::clipboard::copy(&text);
        self.notify(if ok {
            format!("copied {} bytes to clipboard", self.detail_value.len())
        } else {
            "clipboard unavailable (no tty)".into()
        });
    }

    /// Export the current view to a file in the working directory.
    fn export_current(&mut self) {
        match &self.store {
            Store::Sqlite(_) => self.export_sqlite(),
            Store::Rkyv(_) => self.export_rkyv(),
        }
    }

    fn export_sqlite(&mut self) {
        let table = match self.current_table() {
            Some(t) => t,
            None => return,
        };
        // Whatever the grid is showing, in the order and under the filter it is
        // showing it: the counted total when there is one, else the page's bound.
        let total = self
            .known_total()
            .or_else(|| self.rows.as_ref().map(|r| r.total))
            .unwrap_or(0);
        let view = match self.sqlite().unwrap().rows(&crate::sqlite::PageQuery {
            table: &table,
            limit: total.max(1),
            offset: 0,
            sort: self.sort.as_ref(),
            filter: &self.filter,
            hint: None,
            known_total: None,
            formats: &crate::sqlite::NO_FORMATS,
        }) {
            Ok(v) => v,
            Err(e) => {
                self.notify(format!("export failed: {}", e));
                return;
            }
        };
        let csv = crate::export::rows_to_csv(&view.columns, &view.rows);
        let path = format!("{}.csv", sanitize(&table));
        match std::fs::write(&path, csv) {
            Ok(()) => self.status = format!("exported {} rows → {}", view.rows.len(), path),
            Err(e) => self.status = format!("write failed: {}", e),
        }
    }

    fn export_rkyv(&mut self) {
        let d = match &self.decoded {
            Some(d) => d,
            None => {
                self.notify("nothing to export (unrecognized archive)");
                return;
            }
        };
        let recs: Vec<crate::export::RecordExport> = d
            .records
            .iter()
            .map(|r| crate::export::RecordExport {
                key: &r.key,
                fields: &r.fields,
                value: &r.value,
            })
            .collect();
        let json = crate::export::records_to_json(&recs);
        let base = match &self.store {
            Store::Rkyv(r) => r
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("records")
                .to_string(),
            _ => "records".into(),
        };
        let path = format!("{}.records.json", sanitize(&base));
        match std::fs::write(&path, json) {
            Ok(()) => self.status = format!("exported {} records → {}", d.records.len(), path),
            Err(e) => self.status = format!("write failed: {}", e),
        }
    }

    fn key_sqlite(&mut self, key: KeyEvent) {
        let code = key.code;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Ctrl-f / Ctrl-b page forward / back (vim page motions); Ctrl-s writes
        // the pending changes, which is the chord DB Browser uses for it.
        if ctrl {
            match code {
                KeyCode::Char('f') => self.page_sqlite(true),
                KeyCode::Char('b') => self.page_sqlite(false),
                KeyCode::Char('s') => self.write_changes(),
                KeyCode::Char('y') => self.copy_column_name(),
                KeyCode::Char('p') => self.print_table(),
                // The rest of DB Browser's cell menu.
                KeyCode::Char('n') => self.set_cell_null(),
                KeyCode::Char('r') => self.refresh(),
                KeyCode::Char('h') => self.copy_cell_hex(),
                KeyCode::Char('e') => self.open_cell_externally(),
                _ => {}
            }
            return;
        }

        // `gg` motion: a lone `g` arms, the next `g` fires. Any other key
        // disarms.
        if code == KeyCode::Char('g') {
            if self.pending_g {
                self.pending_g = false;
                self.goto_top();
            } else {
                self.pending_g = true;
            }
            return;
        }
        self.pending_g = false;

        match code {
            KeyCode::Char('G') => self.goto_bottom(),
            KeyCode::Char('/') => self.open_modal(Mode::Search(String::new())),
            KeyCode::Char('n') => self.search_next(true),
            KeyCode::Char('N') => self.search_next(false),
            KeyCode::Char('q') => {
                if !self.guard_pending(Exit::Quit) {
                    self.quit = true;
                }
            }
            // First level: Esc backs out to the file list, as it does from any
            // nested screen; `q` is the one that quits.
            KeyCode::Esc => self.back_to_files(),
            KeyCode::Tab => {
                self.focus = if self.focus == Focus::Left {
                    Focus::Right
                } else {
                    Focus::Left
                };
            }
            KeyCode::Up | KeyCode::Char('k') => match self.focus {
                Focus::Left => {
                    let visible = self.visible_tables();
                    if let Some(i) = Self::step_visible(&visible, self.table_idx, -1) {
                        self.select_table(i);
                    }
                }
                Focus::Right => self.row_idx = self.row_idx.saturating_sub(1),
            },
            KeyCode::Down | KeyCode::Char('j') => match self.focus {
                Focus::Left => {
                    let visible = self.visible_tables();
                    if let Some(i) = Self::step_visible(&visible, self.table_idx, 1) {
                        self.select_table(i);
                    }
                }
                Focus::Right => {
                    if let Some(r) = &self.rows {
                        if self.row_idx + 1 < r.rows.len() {
                            self.row_idx += 1;
                        }
                    }
                }
            },
            // Columns move with the arrows only: `h` is the help overlay (as in
            // iftoprs) and `l` is left free so the pair stays consistent.
            KeyCode::Left => {
                if self.focus == Focus::Right {
                    self.step_column(false);
                }
            }
            KeyCode::Right => {
                if self.focus == Focus::Right {
                    self.step_column(true);
                }
            }
            KeyCode::Enter => match self.focus {
                Focus::Left => self.focus = Focus::Right,
                Focus::Right => self.enter_detail(),
            },
            KeyCode::PageDown => self.page_sqlite(true),
            KeyCode::PageUp => self.page_sqlite(false),
            KeyCode::Char('e') => self.begin_edit_cell(),
            // `E` edits any cell as bytes, which is the only way to put binary
            // into one that does not already hold a blob.
            KeyCode::Char('E') => self.edit_cell_as_bytes(),
            KeyCode::Char('a') => self.insert_row(),
            KeyCode::Char('d') => {
                if self.focus == Focus::Right && self.current_rowid().is_some() {
                    self.mode = Mode::ConfirmDelete;
                }
            }
            KeyCode::Char('S') => self.open_schema(),
            // `P` for the pragmas that decide how the file is written.
            KeyCode::Char('P') => self.open_pragmas(),
            // `D` for the database's own facts, the shell's `.dbinfo`.
            KeyCode::Char('D') => self.open_dbinfo(),
            // `A` analyzes the table's columns, `Y` copies the row as an INSERT.
            KeyCode::Char('A') => self.open_stats(),
            KeyCode::Char('Y') => self.copy_row_as_insert(),
            // `F` follows the foreign key under the cursor to the row it points
            // at, which is the one navigation a schema gives you for free.
            KeyCode::Char('F') => self.follow_foreign_key(),
            // Sorting: `s` toggles the cursor column (asc → desc → off), and
            // `<`/`>` walk the sort across columns keeping the direction.
            KeyCode::Char('s') => self.sort_by_current_column(),
            KeyCode::Char('<') => self.sort_shift_column(false),
            KeyCode::Char('>') => self.sort_shift_column(true),
            KeyCode::Char('x') => self.export_current(),
            KeyCode::Char('y') => self.copy_sqlite_cell(),
            // `:` opens the SQL editor (multi-line, completion, transcript).
            KeyCode::Char(':') => self.open_sql(),
            // DB Browser's Write Changes / Revert Changes.
            KeyCode::Char('W') => self.write_changes(),
            KeyCode::Char('R') => self.revert_changes(),
            // DB Browser's Browse Data column settings.
            KeyCode::Char('H') => self.toggle_hidden_column(),
            KeyCode::Char('U') => self.show_all_columns(),
            KeyCode::Char('f') => self.freeze_to_cursor(),
            KeyCode::Char('#') => self.toggle_rowid_column(),
            KeyCode::Char('m') => self.cycle_column_format(true),
            KeyCode::Char('M') => self.cycle_column_format(false),
            KeyCode::Char('%') => self.begin_custom_format(),
            // DB Browser's find and replace, insert values, save filter as view,
            // and the two clears.
            KeyCode::Char('r') => self.begin_find_replace(),
            KeyCode::Char('i') => self.open_insert_form(),
            KeyCode::Char('V') => self.begin_save_view(),
            KeyCode::Char('z') => self.clear_sorting(),
            KeyCode::Char('Z') => self.clear_filter(),
            KeyCode::Char('L') => self.unlock_view_editing(),
            KeyCode::Char('!') => self.open_cond_format(),
            // F5 is the refresh every GUI has, including DB Browser's.
            KeyCode::F(5) => self.refresh(),
            _ => {}
        }
    }

    fn copy_rkyv_key(&mut self) {
        let key = self
            .decoded
            .as_ref()
            .and_then(|d| d.records.get(self.record_idx))
            .map(|r| r.key.clone());
        if let Some(k) = key {
            let ok = crate::clipboard::copy(&k);
            self.status = if ok {
                "copied key to clipboard".into()
            } else {
                "clipboard unavailable (no tty)".into()
            };
        }
    }

    fn copy_sqlite_cell(&mut self) {
        let cell = self
            .rows
            .as_ref()
            .and_then(|r| r.rows.get(self.row_idx))
            .and_then(|row| row.get(self.col_idx))
            .cloned()
            .unwrap_or_default();
        let ok = crate::clipboard::copy(&cell);
        self.notify(if ok {
            "copied cell to clipboard"
        } else {
            "clipboard unavailable (no tty)"
        });
    }

    fn key_rkyv(&mut self, key: KeyEvent) {
        let code = key.code;
        if code == KeyCode::Char('g') {
            if self.pending_g {
                self.pending_g = false;
                self.rkyv_goto_top();
            } else {
                self.pending_g = true;
            }
            return;
        }
        self.pending_g = false;

        match code {
            KeyCode::Char('G') => self.rkyv_goto_bottom(),
            KeyCode::Char('/') => self.open_modal(Mode::Search(String::new())),
            KeyCode::Char('n') => self.search_next(true),
            KeyCode::Char('N') => self.search_next(false),
            KeyCode::Char('q') => {
                if !self.guard_pending(Exit::Quit) {
                    self.quit = true;
                }
            }
            // First level: Esc backs out to the file list, as it does from any
            // nested screen; `q` is the one that quits.
            KeyCode::Esc => self.back_to_files(),
            KeyCode::Char('0') => {
                if self.decoded.is_some() {
                    self.rkyv_view = RkyvView::Records;
                } else if self.decoding.is_some() {
                    self.notify("still decoding — Records will open when it lands");
                }
            }
            KeyCode::Char('1') => self.rkyv_view = RkyvView::Info,
            KeyCode::Char('2') => self.rkyv_view = RkyvView::Strings,
            KeyCode::Char('3') => self.rkyv_view = RkyvView::Hex,
            KeyCode::Up | KeyCode::Char('k') => self.move_rkyv(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_rkyv(1),
            KeyCode::PageDown => self.page_rkyv(true),
            KeyCode::PageUp => self.page_rkyv(false),
            KeyCode::Enter => {
                if self.rkyv_view == RkyvView::Records {
                    self.enter_detail();
                }
            }
            KeyCode::Char('d') => {
                if self.rkyv_view == RkyvView::Records && self.has_current_record() {
                    self.mode = Mode::ConfirmDelete;
                }
            }
            KeyCode::Char('a') => {
                if self.rkyv_view == RkyvView::Records && self.decoded.is_some() {
                    self.open_modal(Mode::AddRecord(String::new()));
                }
            }
            KeyCode::Char('e') => {
                if self.rkyv_view == RkyvView::Records && self.has_current_record() {
                    self.open_hex_editor();
                }
            }
            KeyCode::Char('r') => {
                let renamable = matches!(
                    self.decoded.as_ref().map(|d| d.kind),
                    Some(
                        FormatKind::Script
                            | FormatKind::Stryke
                            | FormatKind::Autoload
                            | FormatKind::Elisp
                    )
                );
                if self.rkyv_view == RkyvView::Records && self.has_current_record() && renamable {
                    let key = self
                        .decoded
                        .as_ref()
                        .and_then(|d| d.records.get(self.record_idx))
                        .map(|r| r.key.clone())
                        .unwrap_or_default();
                    self.open_modal(Mode::RenameRecord(key));
                }
            }
            KeyCode::Char('x') => self.export_current(),
            KeyCode::Char('y') => self.copy_rkyv_key(),
            _ => {}
        }
    }

    /// Cursor-aware text-input handler shared by every input modal. The cursor
    /// model (UTF-8-safe left/right/word-nav/home/end/kill) is ported from
    /// iftoprs's `FilterState`. `mk` rebuilds the mode from the edited buffer,
    /// `commit` runs on Enter.
    fn key_input(
        &mut self,
        key: KeyEvent,
        mut buf: String,
        mk: fn(String) -> Mode,
        commit: fn(&mut App, &str),
    ) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let mut cur = self.input_cursor.min(buf.len());
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                // A cancelled `/` drops the filter and puts the cursor back.
                if let Some(o) = self.search_origin.take() {
                    self.set_filter(String::new());
                    self.restore_position(o);
                    self.status.clear();
                }
                return;
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                // Enter keeps the filter the typing already applied.
                if self.search_origin.take().is_some() {
                    return;
                }
                commit(self, &buf);
                return;
            }
            KeyCode::Left => cur = crate::input::left(&buf, cur),
            KeyCode::Right => cur = crate::input::right(&buf, cur),
            KeyCode::Home => cur = 0,
            KeyCode::End => cur = buf.len(),
            KeyCode::Char('a') if ctrl => cur = 0,
            KeyCode::Char('e') if ctrl => cur = buf.len(),
            KeyCode::Char('b') if ctrl => cur = crate::input::left(&buf, cur),
            KeyCode::Char('f') if ctrl => cur = crate::input::right(&buf, cur),
            KeyCode::Char('w') if ctrl => cur = crate::input::delete_word(&mut buf, cur),
            KeyCode::Char('u') if ctrl => {
                buf.drain(..cur);
                cur = 0;
            }
            KeyCode::Char('k') if ctrl => buf.truncate(cur),
            KeyCode::Backspace => {
                if cur > 0 {
                    let p = crate::input::left(&buf, cur);
                    buf.drain(p..cur);
                    cur = p;
                }
            }
            KeyCode::Delete => {
                if cur < buf.len() {
                    let n = crate::input::right(&buf, cur);
                    buf.drain(cur..n);
                }
            }
            KeyCode::Char(c) => {
                buf.insert(cur, c);
                cur += c.len_utf8();
            }
            _ => {}
        }
        self.input_cursor = cur;
        self.mode = mk(buf);
        // A `/` prompt filters the list as it is typed.
        if let Mode::Search(pattern) = &self.mode {
            let pattern = pattern.clone();
            self.filter_preview(&pattern);
        }
    }

    fn snapshot_position(&self) -> SearchOrigin {
        SearchOrigin {
            table_idx: self.table_idx,
            page_offset: self.page_offset,
            row_idx: self.row_idx,
            record_idx: self.record_idx,
            string_idx: self.string_idx,
            hex_row: self.hex_row,
        }
    }

    fn restore_position(&mut self, o: SearchOrigin) {
        self.table_idx = o.table_idx;
        self.record_idx = o.record_idx;
        self.string_idx = o.string_idx;
        self.hex_row = o.hex_row;
        self.row_idx = o.row_idx;
        // Only reload when the page actually moved: a query per keystroke would
        // make typing feel heavy on a large table.
        if self.page_offset != o.page_offset {
            self.page_offset = o.page_offset;
            self.load_table();
            self.row_idx = o.row_idx;
        }
    }

    /// Apply `pattern` as the list filter, on every keystroke. Only matching
    /// rows/records/strings stay listed — the same model as iftoprs's `/`, rather
    /// than hopping between matches.
    ///
    /// For SQLite the filter is a `WHERE` over every column, so it covers the
    /// whole table and not just the loaded page.
    fn filter_preview(&mut self, pattern: &str) {
        self.set_filter(pattern.to_string());
    }

    /// Set the active filter and rebuild whatever the current view lists.
    fn set_filter(&mut self, pattern: String) {
        if self.filter == pattern {
            return;
        }
        self.filter = pattern;
        match &self.store {
            Store::Sqlite(_) => {
                // With the table list focused, follow the filter onto a listed
                // table; otherwise the grid keeps showing a hidden one.
                if self.focus == Focus::Left {
                    let visible = self.visible_tables();
                    if !visible.contains(&self.table_idx) {
                        if let Some(&first) = visible.first() {
                            self.table_idx = first;
                        }
                    }
                }
                // The row grid is filtered in SQL, so the page restarts.
                self.page_offset = 0;
                self.row_idx = 0;
                self.load_table();
            }
            Store::Rkyv(_) => {
                // Keep the selection on a listed row.
                self.record_idx = self.first_visible_record().unwrap_or(0);
                self.string_idx = self.first_visible_string().unwrap_or(0);
            }
        }
        self.status = if self.filter.is_empty() {
            String::new()
        } else {
            let n = self.visible_count();
            format!(
                "/{}  ({} match{})",
                self.filter,
                n,
                if n == 1 { "" } else { "es" }
            )
        };
    }

    /// Does `hay` pass the active filter? An empty filter passes everything.
    fn passes(&self, hay: &str) -> bool {
        self.filter.is_empty() || hay.to_lowercase().contains(&self.filter.to_lowercase())
    }

    /// Indices of the records the filter leaves listed.
    fn visible_records(&self) -> Vec<usize> {
        match &self.decoded {
            Some(d) => d
                .records
                .iter()
                .enumerate()
                .filter(|(_, r)| self.passes(&r.key))
                .map(|(i, _)| i)
                .collect(),
            None => Vec::new(),
        }
    }

    /// Indices of the extracted strings the filter leaves listed.
    fn visible_strings(&self) -> Vec<usize> {
        self.strings
            .iter()
            .enumerate()
            .filter(|(_, s)| self.passes(&s.text))
            .map(|(i, _)| i)
            .collect()
    }

    /// Table names the filter leaves listed (the left pane).
    fn visible_tables(&self) -> Vec<usize> {
        let tables = self.sqlite().map(|s| s.tables.clone()).unwrap_or_default();
        tables
            .iter()
            .enumerate()
            .filter(|(_, t)| self.passes(t))
            .map(|(i, _)| i)
            .collect()
    }

    fn first_visible_record(&self) -> Option<usize> {
        self.visible_records().first().copied()
    }

    fn first_visible_string(&self) -> Option<usize> {
        self.visible_strings().first().copied()
    }

    /// How many rows the current view lists under the filter, for the status line.
    fn visible_count(&self) -> usize {
        match &self.store {
            Store::Sqlite(_) => match self.focus {
                Focus::Left => self.visible_tables().len(),
                Focus::Right => self.rows.as_ref().map(|r| r.total as usize).unwrap_or(0),
            },
            Store::Rkyv(_) => match self.rkyv_view {
                RkyvView::Records => self.visible_records().len(),
                RkyvView::Strings => self.visible_strings().len(),
                _ => 0,
            },
        }
    }

    /// Open a text-input modal, placing the cursor at the end of its buffer.
    fn open_modal(&mut self, mode: Mode) {
        if matches!(mode, Mode::Search(_)) {
            self.search_origin = Some(self.snapshot_position());
        }
        self.input_cursor = match &mode {
            Mode::EditCell(s) | Mode::Search(s) | Mode::AddRecord(s) | Mode::RenameRecord(s) => {
                s.len()
            }
            _ => 0,
        };
        self.mode = mode;
    }

    fn commit_search(&mut self, pattern: &str) {
        self.search = pattern.to_string();
        self.search_next(true);
    }

    // ----- what the grid shows (DB Browser's Browse Data settings) ----------

    /// The column under the cursor, by name.
    fn cursor_column(&self) -> Option<String> {
        self.rows
            .as_ref()
            .and_then(|r| r.columns.get(self.col_idx))
            .cloned()
    }

    /// `H`: hide the cursor column, or show it again if it is already hidden.
    fn toggle_hidden_column(&mut self) {
        let (table, column) = match (self.current_table(), self.cursor_column()) {
            (Some(t), Some(c)) => (t, c),
            _ => return,
        };
        let total = self.rows.as_ref().map(|r| r.columns.len()).unwrap_or(0);
        let done = self.browse.view_mut(&table).toggle_hidden(&column, total);
        match done {
            Ok(hidden) => {
                if hidden {
                    // The cursor cannot sit on a column that is not drawn.
                    self.move_to_visible_column(1);
                }
                self.notify(format!(
                    "{column} {}",
                    if hidden { "hidden" } else { "shown" }
                ));
            }
            Err(e) => self.notify(e),
        }
    }

    /// `U`: DB Browser's "Show all columns".
    fn show_all_columns(&mut self) {
        let table = match self.current_table() {
            Some(t) => t,
            None => return,
        };
        let n = self.browse.view(&table).hidden.len();
        self.browse.view_mut(&table).hidden.clear();
        self.notify(if n == 0 {
            "no columns were hidden".to_string()
        } else {
            format!("showing {n} hidden column{}", if n == 1 { "" } else { "s" })
        });
    }

    /// `f`: freeze the columns up to and including the cursor, so they stay at
    /// the left edge while the rest scroll. Pressing it on a column already
    /// inside the frozen span unfreezes everything.
    fn freeze_to_cursor(&mut self) {
        let table = match self.current_table() {
            Some(t) => t,
            None => return,
        };
        let columns = self
            .rows
            .as_ref()
            .map(|r| r.columns.clone())
            .unwrap_or_default();
        let view = self.browse.view_mut(&table);
        let visible = view.visible(&columns);
        let at = visible.iter().position(|&i| i == self.col_idx);
        let want = match at {
            Some(at) if at < view.frozen => 0,
            Some(at) => at + 1,
            None => return,
        };
        view.frozen = want;
        self.col_scroll = 0;
        self.notify(if want == 0 {
            "columns unfrozen".to_string()
        } else {
            format!("{want} column{} frozen", if want == 1 { "" } else { "s" })
        });
    }

    /// `#`: DB Browser's "Show rowid column".
    fn toggle_rowid_column(&mut self) {
        let table = match self.current_table() {
            Some(t) => t,
            None => return,
        };
        let has_rowid = self
            .rows
            .as_ref()
            .is_some_and(|r| r.rowids.iter().any(Option::is_some));
        if !has_rowid {
            self.notify("a WITHOUT ROWID table has no rowid to show");
            return;
        }
        let view = self.browse.view_mut(&table);
        view.show_rowid = !view.show_rowid;
        let on = view.show_rowid;
        self.notify(if on {
            "rowid column shown"
        } else {
            "rowid column hidden"
        });
    }

    /// `m` / `M`: step the cursor column's display format forwards or back —
    /// DB Browser's Column Display Format, applied in the `SELECT`.
    fn cycle_column_format(&mut self, forward: bool) {
        let (table, column) = match (self.current_table(), self.cursor_column()) {
            (Some(t), Some(c)) => (t, c),
            _ => return,
        };
        let next = self.browse.view(&table).format(&column).next(!forward);
        self.browse
            .view_mut(&table)
            .set_format(&column, next.clone());
        // The format is part of the query, so the page has to be fetched again.
        self.load_table();
        self.notify(format!("{column}: {}", next.label()));
    }

    /// One column left or right, over the columns that are drawn — a hidden
    /// column is stepped over rather than selected invisibly.
    fn step_column(&mut self, forward: bool) {
        let (table, columns) = match (
            self.current_table(),
            self.rows.as_ref().map(|r| r.columns.clone()),
        ) {
            (Some(t), Some(c)) => (t, c),
            _ => return,
        };
        let visible = self.browse.view(&table).visible(&columns);
        let at = visible.iter().position(|&i| i == self.col_idx);
        let next = match (at, forward) {
            (Some(i), true) => visible.get(i + 1).copied(),
            (Some(i), false) => i.checked_sub(1).and_then(|p| visible.get(p).copied()),
            // The cursor is on a hidden column, so the step lands on a shown one.
            (None, _) => visible.first().copied(),
        };
        if let Some(c) = next {
            self.col_idx = c;
        }
    }

    /// `%`: type a display format of your own — DB Browser's "Custom" entry,
    /// where `%1` stands for the column.
    fn begin_custom_format(&mut self) {
        let (table, column) = match (self.current_table(), self.cursor_column()) {
            (Some(t), Some(c)) => (t, c),
            _ => return,
        };
        // Seeded with whatever the column is already formatted by, written out as
        // an expression — a cycled format is then editable rather than a dead end.
        let seed = match self.browse.view(&table).format(&column) {
            crate::browse::Format::Custom(expr) => expr,
            crate::browse::Format::Default => String::new(),
            other => other.expression("%1"),
        };
        self.input_cursor = seed.len();
        self.mode = Mode::CustomFormat(seed);
    }

    fn commit_custom_format(&mut self, expr: &str) {
        let (table, column) = match (self.current_table(), self.cursor_column()) {
            (Some(t), Some(c)) => (t, c),
            _ => return,
        };
        // An empty expression clears the format rather than being rejected: it is
        // the obvious way to ask for the column back.
        if expr.trim().is_empty() {
            self.browse
                .view_mut(&table)
                .set_format(&column, crate::browse::Format::Default);
            self.load_table();
            self.notify(format!("{column}: default"));
            return;
        }
        if let Err(e) = crate::browse::Format::validate_custom(expr) {
            self.notify(e);
            return;
        }
        let f = crate::browse::Format::Custom(expr.trim().to_string());
        self.browse.view_mut(&table).set_format(&column, f.clone());
        self.load_table();
        // A bad expression is SQLite's to reject, and the failed page says so.
        self.notify(format!("{column}: {}", f.label()));
    }

    /// Move the cursor to the nearest column that is actually drawn, stepping in
    /// `dir`. Used after hiding the one it was on.
    fn move_to_visible_column(&mut self, dir: isize) {
        let (table, columns) = match (
            self.current_table(),
            self.rows.as_ref().map(|r| r.columns.clone()),
        ) {
            (Some(t), Some(c)) => (t, c),
            _ => return,
        };
        let visible = self.browse.view(&table).visible(&columns);
        if visible.is_empty() {
            return;
        }
        if visible.contains(&self.col_idx) {
            return;
        }
        // The nearest visible column in the direction of travel, else the other
        // way — hiding the last column has to land somewhere.
        let next = if dir >= 0 {
            visible
                .iter()
                .find(|&&i| i > self.col_idx)
                .or_else(|| visible.last())
        } else {
            visible
                .iter()
                .rev()
                .find(|&&i| i < self.col_idx)
                .or_else(|| visible.first())
        };
        self.col_idx = next.copied().unwrap_or(0);
    }

    // ----- Browse Data operations (DB Browser's data tab) -------------------

    /// `r`: find and replace in the cursor column, asked in two steps because a
    /// terminal has one prompt line.
    fn begin_find_replace(&mut self) {
        if self.focus != Focus::Right || self.cursor_column().is_none() {
            return;
        }
        if !self.writable_here() {
            return;
        }
        self.input_cursor = 0;
        self.mode = Mode::FindText(String::new());
    }

    fn commit_find(&mut self, find: &str) {
        if find.is_empty() {
            self.notify("nothing to find");
            return;
        }
        let (table, column) = match (self.current_table(), self.cursor_column()) {
            (Some(t), Some(c)) => (t, c),
            _ => return,
        };
        // How many rows it would touch, before asking what to put there. A find
        // that matches nothing is worth knowing before typing a replacement.
        let filter = self.filter.clone();
        let n = self
            .sqlite()
            .and_then(|s| s.count_matches(&table, &column, find, &filter).ok())
            .unwrap_or(0);
        if n == 0 {
            self.notify(format!("{find:?} is not in {column}"));
            return;
        }
        self.notify(format!(
            "{n} row{} of {column} contain {find:?}",
            if n == 1 { "" } else { "s" }
        ));
        self.replace_find = find.to_string();
        self.input_cursor = 0;
        self.mode = Mode::ReplaceText(String::new());
    }

    fn commit_replace(&mut self, to: &str) {
        let (table, column) = match (self.current_table(), self.cursor_column()) {
            (Some(t), Some(c)) => (t, c),
            _ => return,
        };
        let find = self.replace_find.clone();
        let filter = self.filter.clone();
        let done = match self.sqlite() {
            Some(s) => s.replace_in_column(&table, &column, &find, to, &filter),
            None => return,
        };
        match done {
            Ok(0) => self.notify(format!("{find:?} is not in {column}")),
            Ok(n) => {
                self.rows_changed();
                self.load_table();
                self.notify(format!(
                    "replaced {find:?} with {to:?} in {n} row{} of {column} — R reverts",
                    if n == 1 { "" } else { "s" }
                ));
            }
            Err(e) => self.notify(e.to_string()),
        }
    }

    /// `i`: DB Browser's "Insert Values" — a row typed column by column, rather
    /// than the blank row `a` inserts.
    fn open_insert_form(&mut self) {
        let table = match self.current_table() {
            Some(t) => t,
            None => return,
        };
        if !self.writable_here() {
            return;
        }
        let columns = match self.sqlite().and_then(|s| s.columns(&table).ok()) {
            Some(c) if !c.is_empty() => c,
            _ => return,
        };
        self.insert_form = Some(crate::browse::RowForm::new(&table, columns));
        self.screen = Screen::InsertRow;
    }

    fn key_insert_row(&mut self, key: KeyEvent) {
        let action = match self.insert_form.as_mut() {
            Some(f) => f.on_key(key),
            None => {
                self.screen = Screen::Main;
                return;
            }
        };
        match action {
            crate::browse::FormAction::None => {}
            crate::browse::FormAction::Note(msg) => self.notify(msg),
            crate::browse::FormAction::Cancel => {
                self.insert_form = None;
                self.screen = Screen::Main;
            }
            crate::browse::FormAction::Insert => {
                let (table, values) = match self.insert_form.as_ref() {
                    Some(f) => (f.table.clone(), f.pairs()),
                    None => return,
                };
                match self.sqlite().map(|s| s.insert_values(&table, &values)) {
                    Some(Ok(n)) => {
                        self.insert_form = None;
                        self.screen = Screen::Main;
                        self.rows_changed();
                        self.load_table();
                        self.notify(format!("inserted {n} row — W writes it, R reverts"));
                    }
                    Some(Err(e)) => self.notify(e.to_string()),
                    None => {}
                }
            }
        }
    }

    /// `V`: DB Browser's "Save filter as view".
    fn begin_save_view(&mut self) {
        if self.current_table().is_none() {
            return;
        }
        let seed = match self.current_table() {
            Some(t) => format!("{t}_view"),
            None => String::new(),
        };
        self.input_cursor = seed.len();
        self.mode = Mode::ViewName(seed);
    }

    fn commit_view_name(&mut self, name: &str) {
        let table = match self.current_table() {
            Some(t) => t,
            None => return,
        };
        let filter = self.filter.clone();
        let done = match self.sqlite() {
            Some(s) => s.create_view_from_filter(name, &table, &filter),
            None => return,
        };
        match done {
            Ok(sql) => {
                self.after_schema_edit(format!("created view {name}: {sql}"));
            }
            Err(e) => self.notify(e.to_string()),
        }
    }

    /// `z` / `Z`: DB Browser's "Clear sorting" and "Clear all filters".
    fn clear_sorting(&mut self) {
        if self.sort.is_none() {
            self.notify("nothing is sorted");
            return;
        }
        self.sort = None;
        self.page_offset = 0;
        self.row_idx = 0;
        self.load_table();
        self.notify("sort cleared (rowid order)");
    }

    fn clear_filter(&mut self) {
        if self.filter.is_empty() {
            self.notify("nothing is filtered");
            return;
        }
        self.set_filter(String::new());
        self.notify("filter cleared");
    }

    /// `!`: the conditional formats for the cursor column — DB Browser's
    /// Conditional Formats manager.
    fn open_cond_format(&mut self) {
        let (table, column) = match (self.current_table(), self.cursor_column()) {
            (Some(t), Some(c)) => (t, c),
            _ => return,
        };
        let rules = self
            .browse
            .view(&table)
            .rules
            .get(&column)
            .cloned()
            .unwrap_or_default();
        self.cond_format = Some(crate::browse::RulesEditor::new(&column, rules));
        self.screen = Screen::CondFormat;
    }

    fn key_cond_format(&mut self, key: KeyEvent) {
        let action = match self.cond_format.as_mut() {
            Some(r) => r.on_key(key),
            None => {
                self.screen = Screen::Main;
                return;
            }
        };
        match action {
            crate::browse::RulesAction::None => {}
            crate::browse::RulesAction::Note(msg) => self.notify(msg),
            crate::browse::RulesAction::Cancel => {
                self.store_rules();
                self.cond_format = None;
                self.screen = Screen::Main;
            }
            crate::browse::RulesAction::Changed => self.store_rules(),
        }
    }

    /// Put the manager's rules back on the table's view. Done on every change so
    /// the grid behind is already right when the screen closes.
    fn store_rules(&mut self) {
        let (table, column, rules) = match (self.current_table(), self.cond_format.as_ref()) {
            (Some(t), Some(r)) => (t, r.column.clone(), r.rules.clone()),
            _ => return,
        };
        let view = self.browse.view_mut(&table);
        if rules.is_empty() {
            view.rules.remove(&column);
        } else {
            view.rules.insert(column, rules);
        }
    }

    /// `Ctrl-n`: put NULL in the cell — DB Browser's "Set to NULL", which is not
    /// the same as clearing it to an empty string.
    fn set_cell_null(&mut self) {
        let (table, key, column) = match self.cell_target() {
            Some(t) => t,
            None => return,
        };
        if !self.writable_here() {
            return;
        }
        // The keyed update binds text, which cannot express NULL, so the store
        // has a statement of its own for it.
        let done = self
            .sqlite()
            .map(|s| s.update_cell_null(&table, &key, &column));
        match done {
            Some(Ok(_)) => {
                self.load_table();
                self.notify(format!("{column} = NULL — W writes it, R reverts"));
            }
            Some(Err(e)) => self.notify(format!("cannot set {column} to NULL: {e}")),
            None => {}
        }
    }

    /// The table, row key and column the cursor is on.
    fn cell_target(&self) -> Option<(String, crate::sqlite::RowKey, String)> {
        let table = self.current_table()?;
        let key = self.current_key()?;
        let column = self.cursor_column()?;
        Some((table, key, column))
    }

    /// `Ctrl-r` / `F5`: read the database again — DB Browser's Refresh. Every
    /// cached fact is dropped, including the reader threads', since another
    /// process may have written to the file since the page was fetched.
    fn refresh(&mut self) {
        self.schema_changed();
        if self.screen == Screen::Schema {
            if let Some(s) = self.sqlite() {
                self.schema = s.schema().unwrap_or_default();
            }
        }
        self.load_table();
        self.notify("refreshed");
    }

    /// `Ctrl-h`: the cell as a hex + ASCII dump on the clipboard — DB Browser's
    /// "Copy with hex/ASCII".
    fn copy_cell_hex(&mut self) {
        let (table, key, column) = match self.cell_target() {
            Some(t) => t,
            None => return,
        };
        let bytes = match self
            .sqlite()
            .map(|s| s.cell_bytes_keyed(&table, &key, &column))
        {
            Some(Ok(b)) => b,
            Some(Err(e)) => return self.notify(e.to_string()),
            None => return,
        };
        let dump: String = bytes
            .chunks(16)
            .enumerate()
            .map(|(i, chunk)| crate::hexedit::hex_dump_line(i * 16, chunk))
            .collect::<Vec<_>>()
            .join("\n");
        let ok = crate::clipboard::copy(&dump);
        self.notify(if ok {
            format!("copied {} bytes of {column} as hex", bytes.len())
        } else {
            "clipboard unavailable (no tty)".into()
        });
    }

    /// `Ctrl-e`: hand the cell's bytes to whatever opens that kind of file —
    /// DB Browser's "Open in external application". The bytes go to a temporary
    /// file, since an external viewer takes a path, not a stream.
    fn open_cell_externally(&mut self) {
        let (table, key, column) = match self.cell_target() {
            Some(t) => t,
            None => return,
        };
        let bytes = match self
            .sqlite()
            .map(|s| s.cell_bytes_keyed(&table, &key, &column))
        {
            Some(Ok(b)) => b,
            Some(Err(e)) => return self.notify(e.to_string()),
            None => return,
        };
        let path = std::env::temp_dir().join(format!(
            "zdbview-{}-{}-{}",
            std::process::id(),
            table.replace(|c: char| !c.is_alphanumeric(), "_"),
            column.replace(|c: char| !c.is_alphanumeric(), "_")
        ));
        if let Err(e) = std::fs::write(&path, &bytes) {
            return self.notify(format!("cannot write {}: {e}", path.display()));
        }
        match open_externally(&path) {
            Ok(()) => self.notify(format!("opened {} ({} bytes)", path.display(), bytes.len())),
            Err(e) => self.notify(format!("cannot open it: {e}")),
        }
    }

    /// `Ctrl-p`: send the table to the printer — DB Browser's "Print". The text
    /// goes to `lpr`, which is what a terminal program has instead of a print
    /// dialog; the printer is whichever one `lpr` would use.
    fn print_table(&mut self) {
        let table = match self.current_table() {
            Some(t) => t,
            None => return,
        };
        let filter = self.filter.clone();
        let view = match self.sqlite().map(|s| {
            s.rows(&crate::sqlite::PageQuery {
                table: &table,
                limit: PRINT_ROWS,
                offset: 0,
                sort: self.sort.as_ref(),
                filter: &filter,
                hint: None,
                known_total: None,
                formats: &crate::sqlite::NO_FORMATS,
            })
        }) {
            Some(Ok(v)) => v,
            Some(Err(e)) => return self.notify(format!("cannot read {table}: {e}")),
            None => return,
        };
        let mut text = format!("{table}\n\n");
        text.push_str(&crate::export::rows_to_csv(&view.columns, &view.rows));
        let rows = view.rows.len();
        match print_via_lpr(&text) {
            Ok(()) => self.notify(format!("sent {rows} row(s) of {table} to lpr")),
            Err(e) => self.notify(format!("cannot print: {e}")),
        }
    }

    /// `.project save FILE`: everything about the session that is not in the
    /// database — what each grid is set to show, what is filtered and sorted, and
    /// the statements left in the editor.
    fn save_project(&mut self, file: &str) -> Vec<String> {
        let (path, tables) = match self.sqlite() {
            Some(s) => (s.path.clone(), s.tables.clone()),
            None => return vec!["projects are for SQLite databases".into()],
        };
        let current = self.current_table();
        let sort = self.sort.as_ref().map(|s| (s.column.as_str(), s.desc));
        let statements = self
            .sql
            .as_ref()
            .map(|e| e.all_statements())
            .unwrap_or_default();
        let project = crate::project::Project::capture(
            &path,
            &self.browse,
            &tables,
            current.as_deref().map(|t| (t, self.filter.as_str(), sort)),
            statements,
        );
        match std::fs::write(file, project.to_text()) {
            Ok(()) => vec![format!("wrote the project to {file}")],
            Err(e) => vec![format!("cannot write {file}: {e}")],
        }
    }

    /// `.project open FILE`: put the settings back.
    fn load_project(&mut self, file: &str) -> Vec<String> {
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => return vec![format!("cannot read {file}: {e}")],
        };
        let project = match crate::project::Project::parse(&text) {
            Some(p) => p,
            None => return vec![format!("{file} is not a zdbview project")],
        };
        let mine = self.sqlite().map(|s| s.path.clone());
        let mut out = Vec::new();
        if mine.as_deref() != Some(project.database.as_path()) {
            out.push(format!(
                "note: the project is for {} — applying its settings anyway",
                project.database.display()
            ));
        }
        self.apply_project(&project);
        out.push(format!(
            "applied {} table setting{}",
            project.tables.len(),
            if project.tables.len() == 1 { "" } else { "s" }
        ));
        out
    }

    /// Put a project's settings on the grids, and its statements in the editor.
    pub fn apply_project(&mut self, project: &crate::project::Project) {
        for t in &project.tables {
            let view = self.browse.view_mut(&t.name);
            view.hidden = t.hidden.clone();
            view.frozen = t.frozen;
            view.show_rowid = t.show_rowid;
            view.formats = t.formats.iter().cloned().collect();
            view.rules.clear();
            for (column, rule) in &t.rules {
                view.rules
                    .entry(column.clone())
                    .or_default()
                    .push(rule.clone());
            }
        }
        // The table in front takes its filter and sort as well.
        if let Some(current) = self.current_table() {
            if let Some(t) = project.table(&current) {
                self.set_filter(t.filter.clone());
                self.sort = t.sort.as_ref().map(|(column, desc)| Sort {
                    column: column.clone(),
                    desc: *desc,
                });
            }
        }
        if !project.statements.is_empty() {
            let schema = self.sqlite().map(|s| s.schema_names()).unwrap_or_default();
            let editor = self
                .sql
                .get_or_insert_with(|| crate::sqledit::SqlEdit::new(schema));
            editor.set_statements(&project.statements);
        }
        self.load_table();
    }

    /// `Ctrl-y`: the column's name on the clipboard — DB Browser's "Copy column
    /// name".
    fn copy_column_name(&mut self) {
        let column = match self.cursor_column() {
            Some(c) => c,
            None => return,
        };
        let ok = crate::clipboard::copy(&column);
        self.notify(if ok {
            format!("copied {column} to clipboard")
        } else {
            "clipboard unavailable (no tty)".into()
        });
    }

    /// `L`: DB Browser's "Unlock view editing". A view can only be written to
    /// when it carries `INSTEAD OF` triggers, so this reports what SQLite would
    /// do rather than pretending the lock is zdbview's to lift.
    fn unlock_view_editing(&mut self) {
        let table = match self.current_table() {
            Some(t) => t,
            None => return,
        };
        let store = match self.sqlite() {
            Some(s) => s,
            None => return,
        };
        if !store.is_view(&table) {
            self.notify(format!("{table} is a table — it is already editable"));
            return;
        }
        match store.view_is_writable(&table) {
            Ok(true) => {
                if let Some(i) = self.unlocked_views.iter().position(|v| *v == table) {
                    self.unlocked_views.remove(i);
                    self.notify(format!("{table} locked again"));
                } else {
                    self.unlocked_views.push(table.clone());
                    self.notify(format!(
                        "{table} unlocked — its INSTEAD OF triggers take the write"
                    ));
                }
            }
            Ok(false) => self.notify(format!(
                "{table} has no INSTEAD OF triggers, so SQLite cannot write to it"
            )),
            Err(e) => self.notify(e.to_string()),
        }
    }

    /// Whether the object the grid is showing can be written to, with the reason
    /// reported when it cannot.
    fn writable_here(&mut self) -> bool {
        let table = match self.current_table() {
            Some(t) => t,
            None => return false,
        };
        if self.sqlite().is_some_and(|s| s.is_readonly()) {
            self.notify("this database was opened read-only (--readonly)");
            return false;
        }
        let is_view = self.sqlite().is_some_and(|s| s.is_view(&table));
        if !is_view {
            return true;
        }
        if self.unlocked_views.contains(&table) {
            return true;
        }
        self.notify(format!(
            "{table} is a view — L unlocks it if it has INSTEAD OF triggers"
        ));
        false
    }

    // ----- editable pragmas (DB Browser's Edit Pragmas) ---------------------

    /// `P`: the settings that decide how the database is written, editable. The
    /// values are read when the screen opens, and re-read from the database after
    /// every change rather than assumed.
    fn open_pragmas(&mut self) {
        let values = match self.sqlite() {
            Some(s) => crate::pragmas::EDITABLE
                .iter()
                .map(|spec| s.pragma(spec.name).unwrap_or_default())
                .collect(),
            None => return,
        };
        self.pragmas = Some(crate::pragmas::PragmaEditor::new(values));
        self.screen = Screen::Pragmas;
    }

    fn key_pragmas(&mut self, key: KeyEvent) {
        let action = match self.pragmas.as_mut() {
            Some(p) => p.on_key(key),
            None => {
                self.screen = Screen::Main;
                return;
            }
        };
        match action {
            crate::pragmas::Action::None => {}
            crate::pragmas::Action::Note(msg) => self.notify(msg),
            crate::pragmas::Action::Cancel => {
                self.pragmas = None;
                self.screen = Screen::Main;
            }
            crate::pragmas::Action::Set(name, value) => self.set_pragma(name, &value),
        }
    }

    fn set_pragma(&mut self, name: &'static str, value: &str) {
        let spec = crate::pragmas::EDITABLE.iter().find(|s| s.name == name);
        let before = self
            .pragmas
            .as_ref()
            .and_then(|p| p.value(name))
            .unwrap_or_default()
            .to_string();
        let result = match self.sqlite() {
            Some(s) => s.set_pragma(name, value),
            None => return,
        };
        match result {
            Ok(read_back) => {
                // A pragma with no query form reports nothing, so what this
                // session asked for is the only value there is to show.
                let effective = read_back.clone().unwrap_or_else(|| value.to_string());
                if let Some(p) = self.pragmas.as_mut() {
                    p.update(name, effective.clone());
                }
                // Reading the value back is what catches a change SQLite took but
                // did not apply — the interesting half of this screen.
                let note = if effective.trim().eq_ignore_ascii_case(value) {
                    match spec {
                        Some(s) if s.needs_vacuum => {
                            format!(
                                "{name}: {before} → {effective} — takes effect on the next VACUUM"
                            )
                        }
                        _ => format!("{name}: {before} → {effective}"),
                    }
                } else {
                    format!("{name} stayed {effective}: SQLite would not take {value}")
                };
                self.notify(note);
                // page_size and auto_vacuum change how the file is laid out, so
                // nothing cached about it still holds.
                self.schema_changed();
            }
            Err(e) => self.notify(e.to_string()),
        }
    }

    // ----- the edit buffer (DB Browser's Write / Revert Changes) ------------

    /// Whether the store is holding changes that have not reached the file.
    fn has_pending(&self) -> bool {
        self.sqlite().is_some_and(|s| s.has_pending())
    }

    /// `W` / `Ctrl-s`: commit everything edited since the last write.
    fn write_changes(&mut self) {
        let done = match self.sqlite() {
            Some(s) => s.write_changes(),
            None => return,
        };
        match done {
            Ok(true) => {
                // The file has changed, so every reader thread's snapshot is old.
                self.schema_changed();
                self.load_table();
                self.notify("changes written");
            }
            Ok(false) => self.notify("nothing to write"),
            Err(e) => self.notify(format!("write failed: {e}")),
        }
    }

    /// `R`: throw the unwritten changes away.
    fn revert_changes(&mut self) {
        let done = match &mut self.store {
            Store::Sqlite(s) => s.revert_changes(),
            Store::Rkyv(_) => return,
        };
        match done {
            Ok(true) => {
                self.schema_changed();
                if let Some(s) = self.sqlite() {
                    self.schema = s.schema().unwrap_or_default();
                }
                self.select_table(self.table_idx);
                self.notify("changes reverted");
            }
            Ok(false) => self.notify("nothing to revert"),
            Err(e) => self.notify(format!("revert failed: {e}")),
        }
    }

    /// Leaving the file with unwritten changes would drop them on the floor when
    /// the connection closes, so it asks first — the one question DB Browser also
    /// asks. Returns true when the caller should stop and let the prompt answer.
    fn guard_pending(&mut self, exit: Exit) -> bool {
        if !self.has_pending() {
            return false;
        }
        self.mode = Mode::ConfirmClose(exit);
        true
    }

    fn key_confirm_close(&mut self, code: KeyCode, exit: Exit) {
        match code {
            KeyCode::Char('w') | KeyCode::Char('W') => {
                self.mode = Mode::Normal;
                self.write_changes();
                self.leave(exit);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.mode = Mode::Normal;
                self.revert_changes();
                self.leave(exit);
            }
            _ => self.mode = Mode::Normal,
        }
    }

    fn leave(&mut self, exit: Exit) {
        match exit {
            Exit::Quit => self.quit = true,
            Exit::Files => {
                self.reopen = true;
                self.quit = true;
            }
        }
    }

    fn key_confirm_delete(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                match &self.store {
                    Store::Sqlite(_) => self.delete_current_row(),
                    Store::Rkyv(_) => self.delete_current_record(),
                }
                self.mode = Mode::Normal;
            }
            _ => self.mode = Mode::Normal,
        }
    }

    // ----- rkyv record CRUD -------------------------------------------------

    fn has_current_record(&self) -> bool {
        self.decoded
            .as_ref()
            .is_some_and(|d| self.record_idx < d.records.len())
    }

    /// (kind, shard bytes) for the current rkyv archive, if decoded. The bytes are
    /// shared, not copied — see [`RkyvStore::bytes`].
    fn rkyv_kind_bytes(&self) -> Option<(FormatKind, std::sync::Arc<[u8]>)> {
        let kind = self.decoded.as_ref().map(|d| d.kind)?;
        match &self.store {
            Store::Rkyv(r) => Some((kind, r.bytes.clone())),
            _ => None,
        }
    }

    /// (display key, del_key, kind, shard bytes) for the selected record.
    fn rkyv_ctx(&self) -> Option<(String, String, FormatKind, std::sync::Arc<[u8]>)> {
        let (key, del_key, kind) = self.decoded.as_ref().and_then(|d| {
            d.records
                .get(self.record_idx)
                .map(|r| (r.key.clone(), r.del_key.clone(), d.kind))
        })?;
        match &self.store {
            Store::Rkyv(r) => Some((key, del_key, kind, r.bytes.clone())),
            _ => None,
        }
    }

    /// Apply a shard edit: write the new bytes back atomically and reload.
    fn rkyv_apply(&mut self, result: Result<Vec<u8>, String>, ok_msg: String) {
        let new_bytes = match result {
            Ok(b) => b,
            Err(e) => {
                self.notify(format!("failed: {}", e));
                return;
            }
        };
        let path = match &self.store {
            Store::Rkyv(r) => r.path.clone(),
            _ => return,
        };
        let tmp = path.with_extension("zdbview.tmp");
        let write = std::fs::write(&tmp, &new_bytes).and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            self.notify(format!("write failed: {}", e));
            return;
        }
        if let Store::Rkyv(r) = &mut self.store {
            r.bytes = new_bytes.into();
        }
        self.reload_rkyv();
        self.notify(ok_msg);
    }

    /// Delete the selected rkyv record and write the shard back.
    fn delete_current_record(&mut self) {
        let (key, del_key, kind, bytes) = match self.rkyv_ctx() {
            Some(v) => v,
            None => return,
        };
        self.rkyv_apply(
            crate::formats::delete_record(&bytes, kind, &del_key),
            format!("deleted: {}", key),
        );
    }

    fn commit_add_record(&mut self, key: &str) {
        if key.is_empty() {
            self.notify("add cancelled: empty key");
            return;
        }
        let (kind, bytes) = match self.rkyv_kind_bytes() {
            Some(v) => v,
            None => return,
        };
        self.rkyv_apply(
            crate::formats::add_record(&bytes, kind, key, Vec::new()),
            format!("added: {}", key),
        );
    }

    /// Open the hex editor on the selected record's value, pre-filled with its
    /// current bytes (ported editor; see `crate::hexedit`).
    fn open_hex_editor(&mut self) {
        let (key, value) = match self
            .decoded
            .as_ref()
            .and_then(|d| d.records.get(self.record_idx))
        {
            Some(rec) => (rec.key.clone(), rec.value.clone()),
            None => return,
        };
        self.hex = Some(HexEdit::new(key, value));
        self.hex_from = self.screen;
        self.hex_target = HexTarget::Record;
        self.screen = Screen::HexEdit;
    }

    fn key_hex(&mut self, key: KeyEvent) {
        let action = match self.hex.as_mut() {
            Some(ed) => ed.on_key(key),
            None => {
                self.screen = Screen::Main;
                return;
            }
        };
        match action {
            hexedit::Action::None => {}
            hexedit::Action::Save => self.commit_hex_value(),
            hexedit::Action::Close => {
                self.hex = None;
                self.screen = self.hex_from;
                if self.screen == Screen::Detail {
                    // The value it was editing may have changed under it.
                    self.load_detail_value(false);
                }
            }
        }
    }

    /// Write the edited bytes back into the archive (same write-back path the
    /// other record edits use), leaving the editor open on the saved value.
    fn commit_hex_value(&mut self) {
        let value = match self.hex.as_ref() {
            Some(ed) => ed.bytes.clone(),
            None => return,
        };
        if let HexTarget::Cell { table, key, column } = self.hex_target.clone() {
            let n = value.len();
            let written = match self.sqlite() {
                Some(s) => s.update_cell_blob_keyed(&table, &key, &column, &value),
                None => return,
            };
            match written {
                Ok(0) => self.notify("the row that cell belonged to is gone".to_string()),
                Ok(_) => {
                    self.load_table();
                    self.notify(format!("{column} set ({n} bytes)"));
                    if let Some(ed) = self.hex.as_mut() {
                        ed.mark_saved();
                    }
                }
                Err(e) => self.notify(format!("write failed: {e}")),
            }
            return;
        }
        let (_, del_key, kind, bytes) = match self.rkyv_ctx() {
            Some(v) => v,
            None => return,
        };
        let n = value.len();
        self.rkyv_apply(
            crate::formats::set_value(&bytes, kind, &del_key, value),
            format!("value set ({} bytes)", n),
        );
        if let Some(ed) = self.hex.as_mut() {
            ed.mark_saved();
        }
    }

    fn commit_rename_record(&mut self, new_key: &str) {
        if new_key.is_empty() {
            self.notify("rename cancelled");
            return;
        }
        let (_, del_key, kind, bytes) = match self.rkyv_ctx() {
            Some(v) => v,
            None => return,
        };
        self.rkyv_apply(
            crate::formats::rename_record(&bytes, kind, &del_key, new_key),
            format!("renamed → {}", new_key),
        );
    }

    /// Recompute rkyv-derived state (strings + decoded records) after a write.
    fn reload_rkyv(&mut self) {
        let (strings, decoded) = match &self.store {
            Store::Rkyv(r) => (r.strings(MIN_STRING), crate::formats::try_decode(&r.bytes)),
            _ => return,
        };
        self.strings_truncated = strings.truncated;
        self.strings_scanned = strings.scanned;
        self.strings = strings.hits;
        self.decoded = decoded;
        let n = self.decoded.as_ref().map(|d| d.records.len()).unwrap_or(0);
        if self.record_idx >= n {
            self.record_idx = n.saturating_sub(1);
        }
        if n == 0 {
            self.rkyv_view = RkyvView::Info;
        }
    }

    // ----- SQLite operations ------------------------------------------------

    fn sqlite(&self) -> Option<&SqliteStore> {
        match &self.store {
            Store::Sqlite(s) => Some(s),
            _ => None,
        }
    }

    fn current_table(&self) -> Option<String> {
        self.sqlite()
            .and_then(|s| s.tables.get(self.table_idx).cloned())
    }

    fn current_rowid(&self) -> Option<i64> {
        self.rows
            .as_ref()
            .and_then(|r| r.rowids.get(self.row_idx).copied().flatten())
    }

    /// How to address the selected row for a write: its rowid where the table has
    /// one, else its primary key read from the row on screen. `None` when neither
    /// exists — a `WITHOUT ROWID` table with no primary key cannot be edited in
    /// place at all, and neither can one whose key column holds a blob, since the
    /// key is matched as text.
    fn current_key(&self) -> Option<crate::sqlite::RowKey> {
        if let Some(id) = self.current_rowid() {
            return Some(crate::sqlite::RowKey::Rowid(id));
        }
        let view = self.rows.as_ref()?;
        if view.primary_key.is_empty() {
            return None;
        }
        let row = view.rows.get(self.row_idx)?;
        let mut pairs = Vec::with_capacity(view.primary_key.len());
        for name in &view.primary_key {
            let i = view.columns.iter().position(|c| c == name)?;
            let value = row.get(i)?;
            if value.starts_with("<blob ") {
                return None;
            }
            pairs.push((name.clone(), value.clone()));
        }
        Some(crate::sqlite::RowKey::Primary(pairs))
    }

    /// Why the selected row cannot be written to, for a message that says which of
    /// the two reasons it is.
    fn no_key_reason(&self) -> &'static str {
        match self.rows.as_ref() {
            Some(v) if v.primary_key.is_empty() => {
                "row has no rowid and the table has no primary key — cannot edit in place"
            }
            _ => "the primary key of this row holds a blob — cannot address it as text",
        }
    }

    fn select_table(&mut self, idx: usize) {
        let n = self.sqlite().map(|s| s.tables.len()).unwrap_or(0);
        if n == 0 {
            return;
        }
        self.table_idx = idx.min(n - 1);
        self.page_offset = 0;
        self.row_idx = 0;
        self.col_idx = 0;
        // The sort column belongs to the table that was open.
        self.sort = None;
        self.load_table();
    }

    /// Sort the row grid by the column under the cursor. Pressing it again on
    /// the same column flips the direction; a third press clears the sort and
    /// returns to the table's natural `rowid` order.
    fn sort_by_current_column(&mut self) {
        let col = match self
            .rows
            .as_ref()
            .and_then(|r| r.columns.get(self.col_idx))
            .cloned()
        {
            Some(c) => c,
            None => return,
        };
        self.sort = match self.sort.take() {
            Some(s) if s.column == col && !s.desc => Some(Sort {
                column: col.clone(),
                desc: true,
            }),
            Some(s) if s.column == col => None,
            _ => Some(Sort {
                column: col.clone(),
                desc: false,
            }),
        };
        // A different ordering means a different first page.
        self.page_offset = 0;
        self.row_idx = 0;
        self.load_table();
        match &self.sort {
            Some(s) => {
                let dir = if s.desc { "descending" } else { "ascending" };
                self.notify(format!("sorted by {} {}", s.column, dir))
            }
            None => self.notify("sort cleared (rowid order)"),
        }
    }

    /// Move the sort to the next / previous column, keeping the direction.
    fn sort_shift_column(&mut self, forward: bool) {
        let columns = match self.rows.as_ref().map(|r| r.columns.clone()) {
            Some(c) if !c.is_empty() => c,
            _ => return,
        };
        let desc = self.sort.as_ref().is_some_and(|s| s.desc);
        let cur = self
            .sort
            .as_ref()
            .and_then(|s| columns.iter().position(|c| *c == s.column));
        let next = match cur {
            Some(i) if forward => (i + 1) % columns.len(),
            Some(i) => (i + columns.len() - 1) % columns.len(),
            // No sort yet: start from the column the cursor is on.
            None => self.col_idx.min(columns.len() - 1),
        };
        self.col_idx = next;
        self.sort = Some(Sort {
            column: columns[next].clone(),
            desc,
        });
        self.page_offset = 0;
        self.row_idx = 0;
        self.load_table();
        let dir = if desc { "descending" } else { "ascending" };
        self.notify(format!("sorted by {} {}", columns[next], dir));
    }

    /// Ask for the page the current position describes. Returns immediately; the
    /// rows arrive through [`Self::poll_query`].
    fn load_table(&mut self) {
        let Some(table) = self.current_table() else {
            return;
        };
        // The page on screen is what lets the fetch step by cursor instead of by
        // offset — the difference between one index seek and re-walking every
        // matching row that comes before this page. Its offset is
        // `loaded_offset`, not `page_offset`: callers move `page_offset` to the
        // page they want before asking for it.
        let loaded_offset = self.loaded_offset;
        let hint = self.rows.as_ref().and_then(|r| {
            Some(crate::sqlite::PageHint {
                offset: loaded_offset,
                first: r.rowids.first().copied().flatten()?,
                last: r.rowids.last().copied().flatten()?,
                len: r.rows.len() as i64,
            })
        });
        let page = crate::query::PageReq {
            table: table.clone(),
            limit: PAGE,
            offset: self.page_offset,
            sort: self.sort.clone(),
            filter: self.filter.clone(),
            hint,
            known_total: self.known_total(),
            formats: self.browse.expressions(&table),
        };
        // The exact total is only worth a scan once per table+filter; while it
        // still describes the grid it is kept.
        let count = match self.exact_total {
            Some((ref t, ref f, _)) if *t == table && *f == self.filter => None,
            _ => Some(crate::query::CountReq {
                table,
                filter: self.filter.clone(),
            }),
        };
        // An unwritten change lives in this store's own open savepoint, and no
        // other connection can see another's open transaction — so while
        // anything is pending the page comes from the store itself rather than
        // from the reader threads, which would still show the old cell.
        if self.sqlite().is_some_and(|s| s.has_pending()) {
            let result = self
                .sqlite()
                .unwrap()
                .rows(&page.query())
                .map_err(|e| e.to_string());
            self.page_generation += 1;
            let generation = self.page_generation;
            self.install_page(crate::query::PageDone { generation, result });
            return;
        }
        if let Some(engine) = self.engine.as_mut() {
            self.page_generation = engine.request(page, count);
            // A cheap page — the common case now that the count is bounded and
            // paging uses a cursor — arrives inside this grace period, so the
            // grid is drawn once with its rows instead of blank and then filled.
            if let Some(done) = engine.wait_page(PAGE_GRACE) {
                self.install_page(done);
            }
        }
    }

    /// Put a finished page on screen.
    fn install_page(&mut self, done: crate::query::PageDone) {
        match done.result {
            Ok(v) => {
                self.loaded_offset = self.page_offset;
                self.rows = Some(v);
                let loaded = self.rows.as_ref().map(|r| r.rows.len()).unwrap_or(0);
                if self.row_idx >= loaded {
                    self.row_idx = loaded.saturating_sub(1);
                }
                // A total already counted for this table and filter is better
                // than the page's bound.
                if let (Some(n), Some(rows)) = (self.known_total(), self.rows.as_mut()) {
                    rows.total = n;
                    rows.total_exact = true;
                }
            }
            // A cancelled query is the normal case while typing, and the
            // generation check already dropped its result; what reaches here is a
            // real failure.
            Err(e) => self.status = format!("load: {e}"),
        }
    }

    /// Install whatever the query threads have finished.
    fn poll_query(&mut self) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        let page = engine.poll_page();
        let count = engine.poll_count();
        let found = engine.poll_search();
        if let Some(done) = page {
            self.install_page(done);
        }
        if let Some(done) = found {
            self.install_search(done);
        }
        if let Some(done) = count {
            if let Ok(n) = done.result {
                self.exact_total = Some((done.table.clone(), done.filter.clone(), n));
                // The count is what `G` was waiting for.
                if self.pending_bottom {
                    self.pending_bottom = false;
                    self.goto_bottom_at(n);
                }
                if let Some(rows) = self.rows.as_mut() {
                    rows.total = n;
                    rows.total_exact = true;
                }
            }
        }
    }

    /// The database changed shape, so every cached fact about it is suspect —
    /// including the ones held by the query threads on their own connections,
    /// which is why they are replaced rather than notified.
    fn schema_changed(&mut self) {
        let path = match &mut self.store {
            Store::Sqlite(s) => {
                s.invalidate();
                Some(s.path.clone())
            }
            Store::Rkyv(_) => None,
        };
        if let Some(path) = path {
            self.engine = Some(crate::query::Engine::new(&path));
        }
        self.exact_total = None;
    }

    /// Rows were added or removed, so a counted total no longer holds.
    fn rows_changed(&mut self) {
        self.exact_total = None;
    }

    /// An exact count is still running, so the total on screen is a lower bound.
    fn counting(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.count_inflight())
    }

    /// A page fetch is still running, so the rows on screen are the previous
    /// page.
    fn loading(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.page_inflight())
    }

    /// A whole-table search is still scanning.
    fn searching(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.searching())
    }

    /// The exact total for the table and filter on screen, if it has arrived.
    fn known_total(&self) -> Option<i64> {
        let table = self.current_table()?;
        match &self.exact_total {
            Some((t, f, n)) if *t == table && *f == self.filter => Some(*n),
            _ => None,
        }
    }

    /// Step `cur` by `delta` positions through `visible`, clamped to its ends.
    /// Navigation has to walk the filtered list, or j/k would land on rows the
    /// filter has hidden.
    fn step_visible(visible: &[usize], cur: usize, delta: isize) -> Option<usize> {
        if visible.is_empty() {
            return None;
        }
        let pos = visible.iter().position(|&i| i == cur).unwrap_or(0) as isize;
        let next = (pos + delta).clamp(0, visible.len() as isize - 1) as usize;
        Some(visible[next])
    }

    /// Install a background decode's result once it arrives.
    fn poll_decode(&mut self) {
        let result = match self.decoding.as_ref() {
            Some(rx) => match rx.try_recv() {
                Ok(d) => d,
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                // The thread died without sending: treat it as undecodable.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
            },
            None => return,
        };
        self.decoding = None;
        self.decoded = result;
        match &self.decoded {
            Some(d) => {
                let (format, records) = (d.format.clone(), d.records.len());
                self.rkyv_view = RkyvView::Records;
                self.notify(format!("{} · {} records", format, records));
            }
            None => self.notify("unrecognized archive — structural view"),
        }
    }

    /// A screenful, never zero.
    fn page_step(&self) -> usize {
        self.page_rows.max(1)
    }

    /// PageUp/PageDown (and `^F`/`^B`) for the SQLite panes: move the selection by
    /// a screenful, stepping to the next/previous SQL page at the edges.
    fn page_sqlite(&mut self, down: bool) {
        let step = self.page_step();
        match self.focus {
            Focus::Left => {
                let visible = self.visible_tables();
                let delta = if down {
                    step as isize
                } else {
                    -(step as isize)
                };
                if let Some(i) = Self::step_visible(&visible, self.table_idx, delta) {
                    self.select_table(i);
                }
            }
            Focus::Right => {
                let (loaded, total) = match &self.rows {
                    Some(r) => (r.rows.len(), r.total),
                    None => return,
                };
                if loaded == 0 {
                    return;
                }
                if down {
                    if self.row_idx + step < loaded {
                        self.row_idx += step;
                    } else if self.page_offset + PAGE < total {
                        self.page(PAGE);
                    } else {
                        self.row_idx = loaded - 1;
                    }
                } else if self.row_idx >= step {
                    self.row_idx -= step;
                } else if self.page_offset > 0 {
                    self.page(-PAGE);
                    // Land at the bottom of the page we just came back to.
                    self.row_idx = self.rows.as_ref().map(|r| r.rows.len()).unwrap_or(1) - 1;
                } else {
                    self.row_idx = 0;
                }
            }
        }
    }

    /// Move the selection by `delta` listed rows, whichever screen is showing.
    /// Used by the filter prompt, where the list stays navigable while typing.
    fn move_selection(&mut self, delta: isize) {
        match &self.store {
            Store::Sqlite(_) => match self.focus {
                Focus::Left => {
                    let visible = self.visible_tables();
                    if let Some(i) = Self::step_visible(&visible, self.table_idx, delta) {
                        self.select_table(i);
                    }
                }
                Focus::Right => {
                    let loaded = self.rows.as_ref().map(|r| r.rows.len()).unwrap_or(0);
                    if loaded == 0 {
                        return;
                    }
                    let next = (self.row_idx as isize + delta).clamp(0, loaded as isize - 1);
                    self.row_idx = next as usize;
                }
            },
            Store::Rkyv(_) => self.move_rkyv(delta),
        }
    }

    /// Move the rkyv selection by `delta` listed rows (the Hex view scrolls
    /// instead, since bytes are not filtered).
    fn move_rkyv(&mut self, delta: isize) {
        match self.rkyv_view {
            RkyvView::Records => {
                let visible = self.visible_records();
                if let Some(i) = Self::step_visible(&visible, self.record_idx, delta) {
                    self.record_idx = i;
                }
            }
            RkyvView::Strings => {
                let visible = self.visible_strings();
                if let Some(i) = Self::step_visible(&visible, self.string_idx, delta) {
                    self.string_idx = i;
                }
            }
            RkyvView::Hex => {
                let rows = match &self.store {
                    Store::Rkyv(r) => r.len().div_ceil(16),
                    _ => 0,
                };
                let next = (self.hex_row as isize + delta).max(0) as usize;
                self.hex_row = next.min(rows.saturating_sub(1));
            }
            RkyvView::Info => {}
        }
    }

    /// The same for the rkyv views, a screenful at a time.
    fn page_rkyv(&mut self, down: bool) {
        let step = self.page_step() as isize;
        self.move_rkyv(if down { step } else { -step });
    }

    fn page(&mut self, delta: i64) {
        if self.focus != Focus::Right {
            return;
        }
        let total = self.rows.as_ref().map(|r| r.total).unwrap_or(0);
        let next = (self.page_offset + delta).max(0);
        if next < total {
            self.page_offset = next;
            self.row_idx = 0;
            self.load_table();
        }
    }

    fn begin_edit_cell(&mut self) {
        if self.focus != Focus::Right {
            return;
        }
        self.edit_current_value()
    }

    /// `E`: open the selected cell in the hex editor whatever it holds. `e` only
    /// does that for a cell that is already a blob, so without this there is no way
    /// to turn a NULL or a string into bytes.
    fn edit_cell_as_bytes(&mut self) {
        if matches!(self.store, Store::Rkyv(_)) {
            return self.open_hex_editor();
        }
        let column = self
            .rows
            .as_ref()
            .and_then(|r| r.columns.get(self.col_idx).cloned());
        let (table, column, key) = match (self.current_table(), column, self.current_key()) {
            (Some(t), Some(c), Some(k)) => (t, c, k),
            _ => {
                let why = self.no_key_reason();
                self.notify(why);
                return;
            }
        };
        // Whatever the cell holds becomes the starting bytes: a string's own bytes,
        // a number's digits, nothing at all for NULL.
        let bytes = match self.sqlite() {
            Some(s) => s
                .cell_bytes_keyed(&table, &key, &column)
                .unwrap_or_default(),
            None => return,
        };
        let n = bytes.len();
        self.hex = Some(HexEdit::new(format!("{table}.{column}"), bytes));
        self.hex_from = self.screen;
        self.hex_target = HexTarget::Cell {
            table,
            key,
            column: column.clone(),
        };
        self.screen = Screen::HexEdit;
        self.notify(format!(
            "{column} as bytes — {n} to start, ^s writes it back as a blob"
        ));
    }

    /// Edit whatever is selected: a SQLite cell (in the hex editor when it holds a
    /// blob, inline otherwise) or a rkyv record's value. Reached from the grid via
    /// `e` and from the detail screen, which has no pane focus of its own.
    fn edit_current_value(&mut self) {
        if matches!(self.store, Store::Rkyv(_)) {
            self.open_hex_editor();
            return;
        }
        let cur = self
            .rows
            .as_ref()
            .and_then(|r| r.rows.get(self.row_idx))
            .and_then(|row| row.get(self.col_idx))
            .cloned()
            .unwrap_or_default();
        let key = match self.current_key() {
            Some(k) => k,
            None => {
                let why = self.no_key_reason();
                self.notify(why);
                return;
            }
        };
        // A blob has no text form: editing it as a string would replace the bytes
        // with their own description. Those cells open in the hex editor instead,
        // which is what DB Browser's binary cell editor does.
        let column = self
            .rows
            .as_ref()
            .and_then(|r| r.columns.get(self.col_idx).cloned());
        if let (Some(table), Some(column), Some(store)) =
            (self.current_table(), column, self.sqlite())
        {
            if store
                .cell_is_blob_keyed(&table, &key, &column)
                .unwrap_or(false)
            {
                match store.cell_bytes_keyed(&table, &key, &column) {
                    Ok(bytes) => {
                        let n = bytes.len();
                        self.hex = Some(HexEdit::new(format!("{table}.{column}"), bytes));
                        self.hex_from = self.screen;
                        self.hex_target = HexTarget::Cell { table, key, column };
                        self.screen = Screen::HexEdit;
                        self.notify(format!("blob cell — {n} bytes in the hex editor"));
                        return;
                    }
                    Err(e) => {
                        self.notify(format!("cannot read the blob: {e}"));
                        return;
                    }
                }
            }
        }
        self.open_modal(Mode::EditCell(cur));
    }

    fn commit_edit_cell(&mut self, val: &str) {
        let (table, key, col) = match (
            self.current_table(),
            self.current_key(),
            self.rows
                .as_ref()
                .and_then(|r| r.columns.get(self.col_idx).cloned()),
        ) {
            (Some(t), Some(rid), Some(c)) => (t, rid, c),
            _ => return,
        };
        let res = self
            .sqlite()
            .unwrap()
            .update_cell_keyed(&table, &key, &col, val);
        match res {
            Ok(0) => self.notify("that row is no longer there".to_string()),
            Ok(_) => {
                self.notify(format!("updated {}.{}", table, col));
                self.load_table();
                // Edited from the detail screen: it is still showing the old bytes.
                if self.screen == Screen::Detail {
                    self.load_detail_value(false);
                }
            }
            Err(e) => self.status = format!("update failed: {}", e),
        }
    }

    fn insert_row(&mut self) {
        let table = match self.current_table() {
            Some(t) => t,
            None => return,
        };
        match self.sqlite().unwrap().insert_blank(&table) {
            Ok(()) => {
                self.notify(format!("inserted default row into {}", table));
                self.rows_changed();
                self.load_table();
            }
            Err(e) => self.status = format!("insert failed: {}", e),
        }
    }

    fn delete_current_row(&mut self) {
        let (table, key) = match (self.current_table(), self.current_key()) {
            (Some(t), Some(k)) => (t, k),
            _ => {
                let why = self.no_key_reason();
                self.notify(why);
                return;
            }
        };
        let what = match &key {
            crate::sqlite::RowKey::Rowid(id) => format!("row {id}"),
            crate::sqlite::RowKey::Primary(pairs) => pairs
                .iter()
                .map(|(c, v)| format!("{c}={v}"))
                .collect::<Vec<_>>()
                .join(", "),
        };
        match self.sqlite().unwrap().delete_row_keyed(&table, &key) {
            Ok(0) => self.notify("that row is no longer there".to_string()),
            Ok(_) => {
                self.notify(format!("deleted {what} from {table}"));
                self.row_idx = self.row_idx.saturating_sub(1);
                self.rows_changed();
                self.load_table();
            }
            Err(e) => self.status = format!("delete failed: {}", e),
        }
    }

    /// `gg` — jump to the first table (left) or first row of the first page
    /// (right).
    fn goto_top(&mut self) {
        match self.focus {
            Focus::Left => self.select_table(0),
            Focus::Right => {
                self.page_offset = 0;
                self.row_idx = 0;
                self.load_table();
            }
        }
    }

    /// `G` — jump to the last table (left) or the last row of the last page
    /// (right).
    fn goto_bottom(&mut self) {
        match self.focus {
            Focus::Left => {
                let n = self.sqlite().map(|s| s.tables.len()).unwrap_or(0);
                if n > 0 {
                    self.select_table(n - 1);
                }
            }
            Focus::Right => match self.known_total() {
                Some(total) => self.goto_bottom_at(total),
                // Only an exact count says which page the last one is, and that
                // is a full scan on a filtered grid. Ask for it and jump when it
                // lands, rather than freezing here.
                None => {
                    self.pending_bottom = true;
                    if let (Some(table), Some(engine)) =
                        (self.current_table(), self.engine.as_mut())
                    {
                        engine.request_count(crate::query::CountReq {
                            table,
                            filter: self.filter.clone(),
                        });
                    }
                    self.status = "counting rows for the last page…".into();
                }
            },
        }
    }

    /// Jump to the last page of a grid known to hold `total` rows.
    fn goto_bottom_at(&mut self, total: i64) {
        if total <= 0 {
            return;
        }
        self.page_offset = ((total - 1) / PAGE) * PAGE;
        self.row_idx = ((total - 1) % PAGE) as usize;
        self.load_table();
    }

    // ----- rkyv navigation --------------------------------------------------

    fn rkyv_goto_top(&mut self) {
        match self.rkyv_view {
            RkyvView::Records => self.record_idx = 0,
            RkyvView::Strings => self.string_idx = 0,
            RkyvView::Hex => self.hex_row = 0,
            RkyvView::Info => {}
        }
    }

    fn rkyv_goto_bottom(&mut self) {
        match self.rkyv_view {
            RkyvView::Records => {
                self.record_idx = self
                    .decoded
                    .as_ref()
                    .map(|d| d.records.len().saturating_sub(1))
                    .unwrap_or(0);
            }
            RkyvView::Strings => self.string_idx = self.strings.len().saturating_sub(1),
            RkyvView::Hex => {
                let len = match &self.store {
                    Store::Rkyv(r) => r.len(),
                    _ => 0,
                };
                self.hex_row = len.saturating_sub(1) / 16;
            }
            RkyvView::Info => {}
        }
    }

    // ----- search (`/`, `n`, `N`) -------------------------------------------

    /// Move to the next (`forward`) or previous match of `self.search`.
    /// SQLite search scans the loaded page across all columns; rkyv search
    /// scans the string list or the raw bytes depending on the active view.
    fn search_next(&mut self, forward: bool) {
        if self.search.is_empty() {
            return;
        }
        match &self.store {
            Store::Sqlite(_) => self.search_sqlite(forward),
            Store::Rkyv(_) => self.search_rkyv(forward),
        }
    }

    fn search_sqlite(&mut self, forward: bool) {
        let term = self.search.to_lowercase();
        match self.focus {
            Focus::Left => {
                let tables = self.sqlite().map(|s| s.tables.clone()).unwrap_or_default();
                // Only what the filter leaves listed: jumping to a hidden table
                // would leave the pane showing a selection nobody can see.
                let visible = self.visible_tables();
                match find_next_visible(&visible, self.table_idx, forward, |i| {
                    tables[i].to_lowercase().contains(&term)
                }) {
                    Some(i) => self.select_table(i),
                    None => self.status = format!("not found: {}", self.search),
                }
            }
            Focus::Right => self.search_sqlite_table(forward),
        }
    }

    /// Whole-table SQLite search (SQL-backed, not limited to the loaded page).
    /// Wraps around from the opposite edge when nothing is found ahead.
    fn search_sqlite_table(&mut self, forward: bool) {
        let (table, columns) = match (
            self.current_table(),
            self.rows.as_ref().map(|r| r.columns.clone()),
        ) {
            (Some(t), Some(c)) => (t, c),
            _ => return,
        };
        // A search scans until it matches and then counts to find out which page
        // that is, so on a large table it is two full scans in the worst case —
        // both on the query thread, from where it cannot freeze the grid.
        let req = crate::query::SearchReq {
            table,
            columns,
            term: self.search.clone(),
            sort: self.sort.clone(),
            filter: self.filter.clone(),
            // From the selected row, else from the edge the scan comes in from.
            from: self.current_rowid(),
            forward,
        };
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        engine.request_search(req);
        self.status = format!("searching for {}…", self.search);
    }

    /// Move to what a finished search found.
    fn install_search(&mut self, done: crate::query::SearchDone) {
        match done.result {
            Ok(Some((_rowid, ordinal))) => {
                let idx0 = (ordinal - 1).max(0);
                self.page_offset = (idx0 / PAGE) * PAGE;
                self.load_table();
                self.row_idx = (idx0 - self.page_offset) as usize;
                let total = self
                    .known_total()
                    .or_else(|| self.rows.as_ref().map(|r| r.total))
                    .unwrap_or(0);
                self.notify(format!("/{}  (row {} of {})", self.search, ordinal, total));
            }
            Ok(None) => self.status = format!("not found: {}", self.search),
            Err(e) => self.status = format!("search error: {}", e),
        }
    }

    fn search_rkyv(&mut self, forward: bool) {
        let term = self.search.to_lowercase();
        match self.rkyv_view {
            RkyvView::Records => {
                let keys: Vec<String> = self
                    .decoded
                    .as_ref()
                    .map(|d| d.records.iter().map(|r| r.key.to_lowercase()).collect())
                    .unwrap_or_default();
                let visible = self.visible_records();
                match find_next_visible(&visible, self.record_idx, forward, |i| {
                    keys[i].contains(&term)
                }) {
                    Some(i) => self.record_idx = i,
                    None => self.status = format!("not found: {}", self.search),
                }
            }
            RkyvView::Strings => {
                let visible = self.visible_strings();
                match find_next_visible(&visible, self.string_idx, forward, |i| {
                    self.strings[i].text.to_lowercase().contains(&term)
                }) {
                    Some(i) => self.string_idx = i,
                    None => self.status = format!("not found: {}", self.search),
                }
            }
            RkyvView::Hex => {
                let bytes = match &self.store {
                    Store::Rkyv(r) => r.bytes.clone(),
                    _ => return,
                };
                let cur = self.hex_row * 16;
                match find_bytes(&bytes, self.search.as_bytes(), cur, forward) {
                    Some(off) => {
                        self.hex_row = off / 16;
                        self.notify(format!("/{}  (offset {:#x})", self.search, off));
                    }
                    None => self.status = format!("not found: {}", self.search),
                }
            }
            RkyvView::Info => {}
        }
    }

    // ----- rendering --------------------------------------------------------

    fn render(&mut self, f: &mut Frame) {
        let outer = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
        // A screenful for paging: the body minus borders, and minus the header
        // row of whichever screen has one. Recomputed every frame so a resize is
        // picked up without any extra plumbing.
        let body = outer[0].height as usize;
        self.page_rows = match self.screen {
            // The detail screen's value pane sits under a 9-row field list.
            Screen::Detail => body.saturating_sub(11),
            // The row grid has a header row on top of its borders.
            Screen::Main if matches!(self.store, Store::Sqlite(_)) => body.saturating_sub(3),
            _ => body.saturating_sub(2),
        }
        .max(1);

        match self.screen {
            Screen::Detail => self.render_detail(f, outer[0]),
            Screen::HexEdit => self.render_hex(f, outer[0]),
            Screen::Top => {
                let t = self.ov.theme;
                self.top_area = outer[0];
                if let Some(m) = self.top.as_ref() {
                    m.render(f, outer[0], &t);
                }
            }
            Screen::Sql => {
                let t = self.ov.theme;
                self.sql_area = outer[0];
                if let Some(e) = self.sql.as_mut() {
                    e.render(f, outer[0], &t);
                }
            }
            Screen::DbInfo => self.render_dbinfo(f, outer[0]),
            Screen::Stats => self.render_stats(f, outer[0]),
            Screen::Frames => {
                let t = self.ov.theme;
                if let Some(w) = self.walk.as_mut() {
                    w.render(f, outer[0], &t);
                }
            }
            Screen::Schema => self.render_schema(f, outer[0]),
            Screen::TableDesign => {
                let t = self.ov.theme;
                if let Some(d) = self.design.as_mut() {
                    d.render(f, outer[0], &t);
                }
            }
            Screen::IndexDesign => {
                let t = self.ov.theme;
                if let Some(d) = self.index_design.as_mut() {
                    d.render(f, outer[0], &t);
                }
            }
            Screen::Pragmas => {
                let t = self.ov.theme;
                if let Some(p) = self.pragmas.as_mut() {
                    p.render(f, outer[0], &t);
                }
            }
            Screen::InsertRow => {
                let t = self.ov.theme;
                if let Some(form) = self.insert_form.as_mut() {
                    form.render(f, outer[0], &t);
                }
            }
            Screen::CondFormat => {
                let t = self.ov.theme;
                if let Some(rules) = self.cond_format.as_mut() {
                    rules.render(f, outer[0], &t);
                }
            }
            Screen::Main => match &self.store {
                Store::Sqlite(_) => self.render_sqlite(f, outer[0]),
                Store::Rkyv(_) => self.render_rkyv(f, outer[0]),
            },
        }
        self.render_status(f, outer[1]);

        // Modal overlays.
        match &self.mode {
            Mode::EditCell(buf) => self.render_input(f, "edit cell (Enter=save, Esc=cancel)", buf),
            Mode::Search(buf) => self.render_input(f, "search / (Enter, Esc)", buf),
            Mode::AddRecord(buf) => self.render_input(f, "new record key (Enter=add, Esc)", buf),
            Mode::CustomFormat(buf) => {
                self.render_input(f, "custom display format — %1 is the column", buf)
            }
            Mode::FindText(buf) => self.render_input(
                f,
                &format!(
                    "find in {} (Enter, Esc)",
                    self.cursor_column().unwrap_or_default()
                ),
                buf,
            ),
            Mode::ReplaceText(buf) => self.render_input(
                f,
                &format!("replace {:?} with (Enter, Esc)", self.replace_find),
                buf,
            ),
            Mode::ViewName(buf) => {
                self.render_input(f, "save this filter as view (Enter, Esc)", buf)
            }
            Mode::RenameRecord(buf) => self.render_input(f, "rename key to (Enter, Esc)", buf),
            Mode::ConfirmDelete => {
                let what = match self.store {
                    Store::Sqlite(_) => "row",
                    Store::Rkyv(_) => "record (rewrites the cache file)",
                };
                self.render_input(f, &format!("delete this {}? (y = yes, any = no)", what), "")
            }
            Mode::ConfirmClose(_) => self.render_input(
                f,
                "unwritten changes: w = write, r = revert, any = stay",
                "",
            ),
            Mode::ConfirmDrop(ty, name) => {
                let extra = if ty == "table" { ", with its rows" } else { "" };
                let title = format!(
                    "DROP {} {}{}? (y = yes, any = no)",
                    ty.to_uppercase(),
                    name,
                    extra
                );
                self.render_input(f, &title, "")
            }
            Mode::Normal => {}
        }

        self.ov.render(f, self.help_ctx());
    }

    fn render_detail(&self, f: &mut Frame, area: Rect) {
        let rows = Layout::vertical([Constraint::Length(9), Constraint::Min(3)]).split(area);

        // Top: field list for the current row/record.
        let mut fields: Vec<Line> = Vec::new();
        let title;
        match &self.store {
            Store::Sqlite(_) => {
                title = " row detail ".to_string();
                if let Some(rv) = &self.rows {
                    if let Some(row) = rv.rows.get(self.row_idx) {
                        for (i, col) in rv.columns.iter().enumerate() {
                            let sel = i == self.col_idx;
                            fields.push(Line::from(vec![
                                Span::styled(
                                    format!("{:<20}", truncate(col, 20)),
                                    Style::default().fg(if sel {
                                        self.ov.theme.accent
                                    } else {
                                        self.ov.theme.dim
                                    }),
                                ),
                                Span::raw(truncate(
                                    row.get(i).map(|s| s.as_str()).unwrap_or(""),
                                    80,
                                )),
                            ]));
                        }
                    }
                }
            }
            Store::Rkyv(_) => {
                title = " record detail ".to_string();
                if let Some(rec) = self
                    .decoded
                    .as_ref()
                    .and_then(|d| d.records.get(self.record_idx))
                {
                    fields.push(Line::from(vec![
                        Span::styled(
                            format!("{:<20}", "key"),
                            Style::default().fg(self.ov.theme.accent),
                        ),
                        Span::raw(truncate(&rec.key, 80)),
                    ]));
                    for (name, val) in &rec.fields {
                        fields.push(Line::from(vec![
                            Span::styled(
                                format!("{:<20}", truncate(name, 20)),
                                Style::default().fg(self.ov.theme.dim),
                            ),
                            Span::raw(val.clone()),
                        ]));
                    }
                }
            }
        }
        f.render_widget(
            Paragraph::new(fields).block(Block::default().borders(Borders::ALL).title(title)),
            rows[0],
        );

        // Bottom: value pane.
        let height = rows[1].height.saturating_sub(2) as usize;
        let lines = value_lines(
            &self.detail_value,
            self.value_render,
            self.detail_scroll,
            height,
        );
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!(
                " value — {} bytes — render: {} (v to cycle · y copy · Esc back) ",
                self.detail_value.len(),
                self.value_render.label()
            ))),
            rows[1],
        );
    }

    /// Open the write monitor over everything zdbview knows about, with the open
    /// file first since that is the one being edited.
    fn open_top(&mut self) {
        let mut targets = vec![match &self.store {
            Store::Sqlite(s) => (s.path.clone(), Kind::Sqlite),
            Store::Rkyv(r) => (r.path.clone(), Kind::Rkyv),
        }];
        targets.extend(watch_targets());
        let m = crate::monitor::Monitor::new(targets);
        if m.is_empty() {
            self.notify("nothing to watch yet");
            return;
        }
        let n = m.len();
        self.top = Some(m);
        self.screen = Screen::Top;
        self.notify(format!("watching {} files for writes", n));
    }

    /// Open the statistics screen for the current table: one row per column with
    /// its nulls, distinct values, extremes and mean — VisiData's `describe`,
    /// sqlite-utils' `analyze-tables`. Every column costs one pass over the table,
    /// so this runs on request rather than alongside the grid.
    fn open_stats(&mut self) {
        let table = match self.current_table() {
            Some(t) => t,
            None => return,
        };
        let path = match self.sqlite() {
            Some(s) => s.path.clone(),
            None => return,
        };
        self.stats.clear();
        self.stats_freq = None;
        self.stats_idx = 0;
        self.screen = Screen::Stats;
        // Each column costs a pass over the table, and on a real database a wide
        // blob column costs more than a second of it — so the pass runs on its own
        // connection on another thread and the screen fills in when it lands.
        self.stats_pending = Some((table.clone(), spawn_stats(path, table.clone())));
        self.status = format!("{table} · analyzing columns …");
    }

    /// Install a finished statistics pass. Called from the event loop, like the
    /// archive decode.
    fn poll_stats(&mut self) {
        let (table, result) = match self.stats_pending.as_ref() {
            Some((t, rx)) => match rx.try_recv() {
                Ok(r) => (t.clone(), r),
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    (t.clone(), Err("the analysis thread stopped".into()))
                }
            },
            None => return,
        };
        self.stats_pending = None;
        match result {
            Ok(v) => {
                let n = v.len();
                self.stats = v;
                self.stats_idx = self.col_idx.min(n.saturating_sub(1));
                self.status =
                    format!("{table} · {n} columns · Enter frequency · j/k move · Esc back");
            }
            Err(e) => {
                self.screen = Screen::Main;
                self.notify(format!("stats failed: {e}"));
            }
        }
    }

    fn key_stats(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => {
                if !self.guard_pending(Exit::Quit) {
                    self.quit = true;
                }
            }
            KeyCode::Esc | KeyCode::Char('A') => {
                // The frequency table is a step deeper, so Esc closes that first.
                if self.stats_freq.is_some() {
                    self.stats_freq = None;
                } else {
                    self.screen = Screen::Main;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.stats_idx = (self.stats_idx + 1).min(self.stats.len().saturating_sub(1));
                self.stats_freq = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.stats_idx = self.stats_idx.saturating_sub(1);
                self.stats_freq = None;
            }
            KeyCode::Char('g') => {
                self.stats_idx = 0;
                self.stats_freq = None;
            }
            KeyCode::Char('G') => {
                self.stats_idx = self.stats.len().saturating_sub(1);
                self.stats_freq = None;
            }
            // A frequency table needs the column list the pass is still building.
            KeyCode::Enter | KeyCode::Char('f') if self.stats_pending.is_none() => {
                self.open_frequency()
            }
            _ => {}
        }
    }

    /// The selected column's most common values with their counts.
    fn open_frequency(&mut self) {
        let (table, column) = match (
            self.current_table(),
            self.stats.get(self.stats_idx).map(|c| c.name.clone()),
        ) {
            (Some(t), Some(c)) => (t, c),
            _ => return,
        };
        let rows = match self.sqlite() {
            Some(s) => s.frequency(&table, &column, FREQUENCY_ROWS),
            None => return,
        };
        match rows {
            Ok(v) => self.stats_freq = Some((column, v)),
            Err(e) => self.notify(format!("frequency failed: {e}")),
        }
    }

    fn render_stats(&mut self, f: &mut Frame, area: Rect) {
        let t = self.ov.theme;
        let mut lines: Vec<Line> = Vec::new();
        let head = format!(
            "  {:<16} {:<8} {:>7} {:>6} {:>8} {:>7} {:>7} {:>9} {:>9} {:>9}",
            "column",
            "type",
            "rows",
            "nulls",
            "distinct",
            "numeric",
            "longest",
            "min",
            "max",
            "mean"
        );
        lines.push(Line::from(Span::styled(
            head,
            Style::default().fg(t.primary).add_modifier(Modifier::BOLD),
        )));
        for (i, c) in self.stats.iter().enumerate() {
            // `numeric` is how many cells SQLite actually stores as a number,
            // which is the only way to see that a column declared INTEGER is
            // holding text — the declared type is an affinity hint, not a rule.
            let mean = match c.avg {
                Some(a) => format!("{a:.3}"),
                None => "—".to_string(),
            };
            let text = format!(
                "  {:<16} {:<8} {:>7} {:>6} {:>8} {:>7} {:>7} {:>9} {:>9} {:>9}",
                truncate(&c.name, 16),
                truncate(&c.declared, 8),
                c.rows,
                c.nulls,
                c.distinct,
                c.numeric,
                c.longest,
                truncate(&c.min, 9),
                truncate(&c.max, 9),
                truncate(&mean, 9),
            );
            // Same selection styling as every other list in the app.
            let style = if i == self.stats_idx {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(t.primary)
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
        if let Some((col, freq)) = &self.stats_freq {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  {col} — most common values"),
                Style::default().fg(t.primary).add_modifier(Modifier::BOLD),
            )));
            let widest = freq.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1);
            for (v, n) in freq {
                // The bar is the point: a glance says whether a column is skewed.
                let bar = "#".repeat(((*n as f64 / widest as f64) * 24.0).round() as usize);
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {:<32} {:>9}  ", truncate(v, 32), n),
                        Style::default().fg(t.primary),
                    ),
                    Span::styled(bar, Style::default().fg(t.accent)),
                ]));
            }
        }
        let height = area.height.saturating_sub(2) as usize;
        // Keep the cursor row on screen without a separate scroll offset: the
        // header plus the selected row is what must be visible.
        let first = (self.stats_idx + 2).saturating_sub(height);
        let shown: Vec<Line> = lines.into_iter().skip(first).take(height).collect();
        let title = match (&self.stats_pending, self.current_table()) {
            (Some((t, _)), _) => format!(" columns of {t} — analyzing … "),
            (None, Some(t)) => format!(" columns of {t} — Enter frequency · Esc back "),
            (None, None) => " columns ".to_string(),
        };
        f.render_widget(
            Paragraph::new(shown).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.accent))
                    .title(title),
            ),
            area,
        );
    }

    /// `F`: follow the foreign key on the cursor column to the row it references,
    /// the way DB Browser and Datasette link a child row to its parent. Jumps to
    /// the parent table, selects the row, and leaves the column on the key.
    fn follow_foreign_key(&mut self) {
        let (table, columns, value) = match (self.current_table(), self.rows.as_ref()) {
            (Some(t), Some(v)) => {
                let value = match v.rows.get(self.row_idx).and_then(|r| r.get(self.col_idx)) {
                    Some(v) => v.clone(),
                    None => return,
                };
                (t, v.columns.clone(), value)
            }
            _ => return,
        };
        let column = match columns.get(self.col_idx) {
            Some(c) => c.clone(),
            None => return,
        };
        let keys = match self.sqlite() {
            Some(s) => s.foreign_keys(&table).unwrap_or_default(),
            None => return,
        };
        let (parent, parent_col) = match keys
            .iter()
            .find(|(child, _, _)| child.eq_ignore_ascii_case(&column))
        {
            Some((_, parent, parent_col)) => (parent.clone(), parent_col.clone()),
            None => {
                self.notify(format!("{column} is not a foreign key"));
                return;
            }
        };
        if value == "NULL" {
            self.notify(format!("{column} is NULL — nothing to follow"));
            return;
        }
        // `rowid` as the parent column means the key targets the parent's implicit
        // rowid, which is already what positions a row.
        let found = match self.sqlite() {
            Some(s) => {
                if parent_col == "rowid" {
                    value.parse::<i64>().ok()
                } else {
                    s.rowid_where(&parent, &parent_col, &value).unwrap_or(None)
                }
            }
            None => return,
        };
        let rowid = match found {
            Some(r) => r,
            None => {
                self.notify(format!("no {parent} row with {parent_col} = {value}"));
                return;
            }
        };
        let idx = match self
            .sqlite()
            .and_then(|s| s.tables.iter().position(|t| *t == parent))
        {
            Some(i) => i,
            None => {
                self.notify(format!("{parent} is not listed"));
                return;
            }
        };
        // The filter belongs to the table being left, and the parent row has to be
        // reachable, so clear it before jumping.
        self.filter.clear();
        self.select_table(idx);
        self.focus = Focus::Right;
        self.goto_rowid(rowid, &parent_col);
        self.notify(format!(
            "{table}.{column} → {parent}.{parent_col} = {value}"
        ));
    }

    /// Put the cursor on `rowid`, paging to wherever it sits in the display order,
    /// and on `column` if the table has one by that name.
    fn goto_rowid(&mut self, rowid: i64, column: &str) {
        let ord = match (self.current_table(), self.sqlite()) {
            (Some(t), Some(s)) => s
                .rowid_ordinal(&t, rowid, self.sort.as_ref(), &self.filter)
                .unwrap_or(1),
            _ => return,
        };
        let idx0 = (ord - 1).max(0);
        self.page_offset = (idx0 / PAGE) * PAGE;
        self.load_table();
        self.row_idx = (idx0 - self.page_offset) as usize;
        if let Some(i) = self
            .rows
            .as_ref()
            .and_then(|r| r.columns.iter().position(|c| c == column))
        {
            self.col_idx = i;
        }
    }

    /// `Y`: the selected row as an `INSERT`, on the clipboard — DB Browser's
    /// "Copy as INSERT", which is how a row moves into a bug report or a fixture.
    fn copy_row_as_insert(&mut self) {
        let (table, view) = match (self.current_table(), self.rows.as_ref()) {
            (Some(t), Some(v)) => (t, v),
            _ => return,
        };
        let row = match view.rows.get(self.row_idx) {
            Some(r) => r.clone(),
            None => return,
        };
        let sql = crate::export::insert_statement(&table, &view.columns, &row);
        let ok = crate::clipboard::copy(&sql);
        self.notify(if ok {
            format!("copied INSERT ({} chars)", sql.len())
        } else {
            "clipboard unavailable (no tty)".to_string()
        });
    }

    /// Open the database-info screen: the pragmas the sqlite3 shell prints for
    /// `.dbinfo`, with the integrity check and foreign-key lint a keypress away
    /// since both can take a while on a large file.
    fn open_dbinfo(&mut self) {
        let info = match self.sqlite() {
            Some(s) => s.db_info(),
            None => return,
        };
        self.dbinfo = info;
        self.dbinfo_checks.clear();
        self.dbinfo_scroll = 0;
        self.screen = Screen::DbInfo;
        self.status = "database · i integrity · Q quick · f fk lint · v vacuum · z analyze \
                       · r reindex · Esc back"
            .into();
    }

    fn key_dbinfo(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Char('D') => {
                self.screen = Screen::Main;
                self.dbinfo_scroll = 0;
            }
            // `q` still quits, as on every other screen.
            KeyCode::Char('q') => {
                if !self.guard_pending(Exit::Quit) {
                    self.quit = true;
                }
            }
            // `O` is DB Browser's "Optimize": PRAGMA optimize, which runs
            // whatever SQLite thinks is worth doing and says what that was.
            KeyCode::Char('O') => {
                self.dbinfo_checks = match self.sqlite().map(|s| s.optimize()) {
                    Some(Ok(done)) if done.is_empty() => {
                        vec!["PRAGMA optimize: nothing needed doing".into()]
                    }
                    Some(Ok(done)) => {
                        let mut out = vec!["PRAGMA optimize ran:".to_string()];
                        out.extend(done);
                        out
                    }
                    Some(Err(e)) => vec![format!("optimize failed: {e}")],
                    None => return,
                };
            }
            // `i` and `Q` are the shell's two checks; `f` is `.lint fkey-indexes`.
            KeyCode::Char('i') | KeyCode::Char('Q') => {
                let quick = code == KeyCode::Char('Q');
                let out = match self.sqlite() {
                    Some(s) => s.integrity_check(quick),
                    None => return,
                };
                self.dbinfo_checks = match out {
                    Ok(lines) if lines == ["ok".to_string()] => {
                        vec![format!(
                            "{} check: ok",
                            if quick { "quick" } else { "integrity" }
                        )]
                    }
                    Ok(lines) => lines,
                    Err(e) => vec![format!("check failed: {e}")],
                };
            }
            // The maintenance statements DB Browser calls "Compact Database" and
            // sqlite-utils exposes as subcommands. Each rewrites the file, so they
            // report what changed.
            KeyCode::Char('v') => self.maintain(crate::sqlite::Maintenance::Vacuum),
            KeyCode::Char('z') => self.maintain(crate::sqlite::Maintenance::Analyze),
            KeyCode::Char('r') => self.maintain(crate::sqlite::Maintenance::Reindex),
            KeyCode::Char('f') => {
                let out = match self.sqlite() {
                    Some(s) => s.missing_fk_indexes(),
                    None => return,
                };
                self.dbinfo_checks = match out {
                    Ok(v) if v.is_empty() => {
                        vec!["foreign-key lint: every foreign key has an index".into()]
                    }
                    Ok(v) => v,
                    Err(e) => vec![format!("lint failed: {e}")],
                };
            }
            KeyCode::Down | KeyCode::Char('j') => self.dbinfo_scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => {
                self.dbinfo_scroll = self.dbinfo_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => self.dbinfo_scroll += self.page_step(),
            KeyCode::PageUp => {
                self.dbinfo_scroll = self.dbinfo_scroll.saturating_sub(self.page_step())
            }
            KeyCode::Char('g') | KeyCode::Home => self.dbinfo_scroll = 0,
            // `G` is the bottom on every other scrolling screen; the report is
            // long enough to want it once the checks have printed.
            KeyCode::Char('G') | KeyCode::End => self.dbinfo_scroll = self.dbinfo_max_scroll,
            _ => {}
        }
    }

    /// Run `VACUUM`, `ANALYZE` or `REINDEX` and re-read the pragmas, since all
    /// three change what the screen is showing.
    fn maintain(&mut self, op: crate::sqlite::Maintenance) {
        let result = match self.sqlite() {
            Some(s) => s.maintain(op),
            None => return,
        };
        let line = match result {
            Ok(delta) => {
                let size = if delta == 0 {
                    "no change in size".to_string()
                } else if delta < 0 {
                    format!("{} smaller", human_size((-delta) as u64))
                } else {
                    format!("{} larger", human_size(delta as u64))
                };
                format!("{}: done, {size}", op.label())
            }
            Err(e) => format!("{} failed: {e}", op.label()),
        };
        if let Some(s) = self.sqlite() {
            self.dbinfo = s.db_info();
        }
        self.dbinfo_checks = vec![line.clone()];
        self.notify(line);
    }

    fn render_dbinfo(&mut self, f: &mut Frame, area: Rect) {
        let t = self.ov.theme;
        let mut lines: Vec<Line> = Vec::new();
        let width = self
            .dbinfo
            .iter()
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(0);
        for (k, v) in &self.dbinfo {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:>width$}  ", k, width = width),
                    Style::default().fg(t.dim),
                ),
                Span::styled(v.clone(), Style::default().fg(t.primary)),
            ]));
        }
        if !self.dbinfo_checks.is_empty() {
            lines.push(Line::from(""));
            for c in &self.dbinfo_checks {
                // A clean result reads as a fact; anything else is a finding.
                let ok =
                    c.ends_with("ok") || c.contains("every foreign key") || c.contains(": done");
                lines.push(Line::from(Span::styled(
                    format!("  {c}"),
                    Style::default().fg(if ok { t.label } else { t.alt }),
                )));
            }
        }
        let height = area.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(height);
        self.dbinfo_max_scroll = max_scroll;
        self.dbinfo_scroll = self.dbinfo_scroll.min(max_scroll);
        let shown: Vec<Line> = lines
            .into_iter()
            .skip(self.dbinfo_scroll)
            .take(height)
            .collect();
        f.render_widget(
            Paragraph::new(shown).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.accent))
                    .title(" database — i/Q check · f fk lint · v/z/r maintain · Esc back "),
            ),
            area,
        );
    }

    /// Open the SQL editor, seeded with the schema so Tab can complete against
    /// the real tables and columns.
    fn open_sql(&mut self) {
        let schema = match self.sqlite() {
            Some(s) => s.schema_names(),
            None => return,
        };
        if self.sql.is_none() {
            self.sql = Some(crate::sqledit::SqlEdit::new(schema));
            self.install_sql_stop();
        }
        self.screen = Screen::Sql;
        let (tab, tabs) = self
            .sql
            .as_ref()
            .map(|e| (e.active_tab() + 1, e.tab_count()))
            .unwrap_or((1, 1));
        self.status = format!(
            "SQL editor · tab {tab}/{tabs} · Tab completes · ^j newline · Enter runs · \
             M-t new tab · Esc back"
        );
    }

    /// Let a running statement be stopped — DB Browser's "Stop the execution".
    ///
    /// A statement runs on this thread, so nothing else can watch the keyboard
    /// while it does. SQLite's progress handler is the one place that gets
    /// control back periodically, so that is where the key is read: it polls
    /// without blocking, and Esc (or Ctrl-c) aborts the statement, which SQLite
    /// reports as an interrupt.
    fn install_sql_stop(&mut self) {
        let running = Arc::clone(&self.sql_running);
        let stopped = Arc::clone(&self.sql_stopped);
        let cancel: crate::sqlite::Cancel = Arc::new(move || {
            if !running.load(std::sync::atomic::Ordering::Relaxed) {
                return false;
            }
            // Poll rather than read: a statement that finishes quickly must not
            // wait for a keypress that is not coming.
            while event::poll(std::time::Duration::ZERO).unwrap_or(false) {
                match event::read() {
                    Ok(Event::Key(k))
                        if k.kind == KeyEventKind::Press
                            && (k.code == KeyCode::Esc
                                || (k.code == KeyCode::Char('c')
                                    && k.modifiers.contains(KeyModifiers::CONTROL))) =>
                    {
                        stopped.store(true, std::sync::atomic::Ordering::Relaxed);
                        return true;
                    }
                    Ok(_) => {}
                    Err(_) => return false,
                }
            }
            false
        });
        if let Store::Sqlite(s) = &mut self.store {
            s.set_cancel(SQL_STOP_CHECK_OPS, cancel);
        }
    }

    fn key_sql(&mut self, key: KeyEvent) {
        let page = crate::sqledit::SqlEdit::page_rows(self.sql_area);
        let action = match self.sql.as_mut() {
            Some(e) => e.on_key(key, page),
            None => {
                self.screen = Screen::Main;
                return;
            }
        };
        match action {
            crate::sqledit::Action::None => {}
            // The editor stays alive behind the main screen, so its transcript and
            // history survive a trip back to the data.
            crate::sqledit::Action::Close => self.screen = Screen::Main,
            crate::sqledit::Action::Execute(sql) => self.execute_sql(&sql),
            crate::sqledit::Action::Explain(sql) => self.explain_sql(&sql),
            crate::sqledit::Action::Note(msg) => self.notify(msg),
            crate::sqledit::Action::OpenFile(path) => self.open_sql_file(&path),
            crate::sqledit::Action::SaveFile(path) => self.save_sql_file(&path),
            crate::sqledit::Action::ExportResult(path) => self.export_sql_result(&path),
            crate::sqledit::Action::SaveView(name) => self.save_result_as_view(&name),
        }
    }

    /// `Alt-o` in the editor: read a `.sql` file into the tab, which then
    /// remembers where it came from.
    fn open_sql_file(&mut self, path: &str) {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                if let Some(e) = self.sql.as_mut() {
                    e.load_text(path, text.trim_end());
                }
                self.notify(format!("loaded {path}"));
            }
            Err(e) => self.notify(format!("cannot read {path}: {e}")),
        }
    }

    /// `Alt-s`: write the tab's statement out.
    fn save_sql_file(&mut self, path: &str) {
        let text = match self.sql.as_ref() {
            Some(e) => e.text(),
            None => return,
        };
        match std::fs::write(path, format!("{}\n", text.trim_end())) {
            Ok(()) => {
                if let Some(e) = self.sql.as_mut() {
                    e.bind_file(path);
                }
                self.notify(format!("wrote {} bytes to {path}", text.len() + 1));
            }
            Err(e) => self.notify(format!("cannot write {path}: {e}")),
        }
    }

    /// `Alt-x`: the last result set to a file, CSV or JSON by extension — DB
    /// Browser's "Export to CSV" / "Export to JSON" for a query's results.
    fn export_sql_result(&mut self, path: &str) {
        let (columns, rows) = match self.sql.as_ref().and_then(|e| e.last_result()) {
            Some(r) => r,
            None => {
                self.notify("no result set to export — run a SELECT first");
                return;
            }
        };
        let json = std::path::Path::new(path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"));
        let text = if json {
            crate::export::rows_to_json(&columns, &rows)
        } else {
            crate::export::rows_to_csv(&columns, &rows)
        };
        match std::fs::write(path, text.as_bytes()) {
            Ok(()) => self.notify(format!(
                "wrote {} row{} to {path} as {}",
                rows.len(),
                if rows.len() == 1 { "" } else { "s" },
                if json { "JSON" } else { "CSV" }
            )),
            Err(e) => self.notify(format!("cannot write {path}: {e}")),
        }
    }

    /// `Alt-v`: DB Browser's "Save results as view" — the statement that
    /// produced them becomes a view, since a result set itself cannot be one.
    fn save_result_as_view(&mut self, name: &str) {
        let sql = match self.sql.as_ref().and_then(|e| e.last_statement()) {
            Some(s) => s,
            None => {
                self.notify("no statement to save — run a SELECT first");
                return;
            }
        };
        let statement = format!(
            "CREATE VIEW {} AS {}",
            crate::ddl::quote(name),
            sql.trim().trim_end_matches(';')
        );
        let plan = crate::ddl::AlterPlan {
            statements: vec![statement],
            rebuild: false,
        };
        match self.sqlite().map(|s| s.apply_ddl(&plan)) {
            Some(Ok(())) => self.after_schema_edit(format!("created view {name}")),
            Some(Err(e)) => self.notify(format!("cannot create the view: {e}")),
            None => {}
        }
    }

    /// `^e` in the editor: the query plan, as the sqlite3 shell's `.eqp` shows it.
    fn explain_sql(&mut self, sql: &str) {
        use crate::sqledit::Entry;
        let plan = match self.sqlite() {
            Some(s) => s.explain_plan(sql),
            None => return,
        };
        let entry = match plan {
            Ok(steps) if steps.is_empty() => Entry::Plan(vec!["(no plan)".into()]),
            Ok(steps) => Entry::Plan(steps),
            Err(e) => Entry::Error(e.to_string()),
        };
        if let Some(e) = self.sql.as_mut() {
            e.push(entry);
        }
    }

    /// Run a statement from the editor and hand the outcome back to it.
    fn execute_sql(&mut self, sql: &str) {
        use crate::sqledit::{Entry, RESULT_ROWS};
        use crate::sqlite::Outcome;
        // A line starting with `.` is a dot-command, as in the shell.
        if sql.trim_start().starts_with('.') {
            return self.run_dot(sql.trim());
        }
        if self.sql_eqp {
            // `.eqp on`: the plan first, then the statement.
            self.explain_sql(sql);
        }
        let started = std::time::Instant::now();
        // The stop key is only watched while this runs, or every scan the grid
        // makes would eat the keyboard.
        self.sql_stopped
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.sql_running
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let outcome = match self.sqlite() {
            Some(s) => s.run(sql, RESULT_ROWS),
            None => return,
        };
        self.sql_running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let took = started.elapsed();
        if self.sql_stopped.load(std::sync::atomic::Ordering::Relaxed) {
            if let Some(e) = self.sql.as_mut() {
                e.push(crate::sqledit::Entry::Error(format!(
                    "stopped after {:.3}s",
                    took.as_secs_f64()
                )));
            }
            return;
        }
        let entry = match outcome {
            Ok(Outcome::Rows {
                columns,
                rows,
                literals,
                truncated,
            }) => {
                // `.output` / `.once` send the result to a file instead, and
                // `.mode` decides how anything but the grid is written.
                match self.write_result(&columns, &rows, &literals) {
                    Some(note) => Entry::Note(vec![note]),
                    None if self.sql_mode == OutputMode::List => Entry::Rows {
                        columns,
                        rows,
                        truncated,
                    },
                    None => {
                        let text = self.sql_mode.render(
                            &columns,
                            &rows,
                            &literals,
                            &self.current_table().unwrap_or_else(|| "table".into()),
                            self.sql_headers,
                        );
                        Entry::Note(text.lines().map(str::to_string).collect())
                    }
                }
            }
            Ok(Outcome::Changed(n)) => {
                // The statement may have been DDL, so the cached column lists and
                // row counts no longer describe the database.
                self.schema_changed();
                // A write may have changed what the grid is showing.
                self.load_table();
                Entry::Changed(n)
            }
            Err(e) => Entry::Error(e.to_string()),
        };
        let timer = self.sql_timer;
        if let Some(e) = self.sql.as_mut() {
            e.push(entry);
            // The shell prints the duration after the result; so does this, unless
            // `.timer off`.
            if timer {
                e.push(crate::sqledit::Entry::Timing(took));
            }
        }
    }

    /// Carry out a dot-command, the way the sqlite3 shell does. The editor holds no
    /// database access of its own, so this is the host's job — which also keeps
    /// every file path and every write in one place.
    fn run_dot(&mut self, line: &str) {
        use crate::sqledit::Entry;
        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap_or("").to_lowercase();
        let args: Vec<&str> = parts.collect();
        let arg = |i: usize| args.get(i).copied();
        let on_off = |v: Option<&str>, current: bool| match v {
            Some("on") | Some("yes") | Some("1") => true,
            Some("off") | Some("no") | Some("0") => false,
            _ => !current,
        };

        let note: Vec<String> = match cmd.as_str() {
            ".help" => DOT_HELP.lines().map(str::to_string).collect(),
            ".tables" => match self.sqlite() {
                Some(s) => s.tables.clone(),
                None => return,
            },
            ".schema" => {
                let only = arg(0);
                match self.sqlite().map(|s| s.schema()) {
                    Some(Ok(objects)) => objects
                        .into_iter()
                        .filter(|(_, name, _)| only.is_none_or(|o| o.eq_ignore_ascii_case(name)))
                        .flat_map(|(_, _, sql)| {
                            let mut lines: Vec<String> = sql.lines().map(str::to_string).collect();
                            lines.push(";".into());
                            lines
                        })
                        .collect(),
                    Some(Err(e)) => vec![format!("{e}")],
                    None => return,
                }
            }
            ".indexes" => {
                let table = match arg(0).map(str::to_string).or_else(|| self.current_table()) {
                    Some(t) => t,
                    None => return,
                };
                match self.sqlite().map(|s| s.indexes(&table)) {
                    Some(Ok(list)) if list.is_empty() => vec![format!("{table}: no indexes")],
                    Some(Ok(list)) => list
                        .into_iter()
                        .map(|(name, cols)| format!("{name}  ({})", cols.join(", ")))
                        .collect(),
                    Some(Err(e)) => vec![format!("{e}")],
                    None => return,
                }
            }
            ".dump" => match self.sqlite().map(|s| s.dump(arg(0))) {
                Some(Ok(text)) => text.lines().map(str::to_string).collect(),
                Some(Err(e)) => vec![format!("{e}")],
                None => return,
            },
            ".databases" => match self.sqlite().map(|s| s.databases()) {
                Some(Ok(list)) => list
                    .into_iter()
                    .map(|(alias, file)| format!("{alias:<10} {file}"))
                    .collect(),
                Some(Err(e)) => vec![format!("{e}")],
                None => return,
            },
            ".attach" => match (arg(0), arg(1)) {
                (Some(file), Some(alias)) => {
                    match self
                        .sqlite()
                        .map(|s| s.attach(std::path::Path::new(file), alias))
                    {
                        Some(Ok(())) => vec![format!("attached {file} as {alias}")],
                        Some(Err(e)) => vec![format!("{e}")],
                        None => return,
                    }
                }
                _ => vec!["usage: .attach FILE ALIAS".into()],
            },
            ".detach" => match (arg(0), self.sqlite()) {
                (Some(alias), Some(s)) => match s.detach(alias) {
                    Ok(()) => vec![format!("detached {alias}")],
                    Err(e) => vec![format!("{e}")],
                },
                (None, _) => vec!["usage: .detach ALIAS".into()],
                _ => return,
            },
            ".mode" => match arg(0) {
                Some(name) => match OutputMode::parse(name) {
                    Some(m) => {
                        self.sql_mode = m;
                        vec![format!("mode: {}", m.label())]
                    }
                    None => vec![format!(
                        "unknown mode {name:?} — list csv tsv markdown line insert json"
                    )],
                },
                None => vec![format!("mode: {}", self.sql_mode.label())],
            },
            ".headers" => {
                self.sql_headers = on_off(arg(0), self.sql_headers);
                vec![format!(
                    "headers: {}",
                    if self.sql_headers { "on" } else { "off" }
                )]
            }
            ".timer" => {
                self.sql_timer = on_off(arg(0), self.sql_timer);
                vec![format!(
                    "timer: {}",
                    if self.sql_timer { "on" } else { "off" }
                )]
            }
            ".eqp" => {
                self.sql_eqp = on_off(arg(0), self.sql_eqp);
                vec![format!("eqp: {}", if self.sql_eqp { "on" } else { "off" })]
            }
            ".output" | ".once" => match arg(0) {
                Some(file) => {
                    let path = PathBuf::from(file);
                    // The shell opens the file fresh and then collects into it, so
                    // truncate here and append per result.
                    match std::fs::write(&path, b"") {
                        Ok(()) => {
                            self.sql_out = Some((path, cmd == ".once"));
                            vec![format!(
                                "{} results to {file}",
                                if cmd == ".once" { "next" } else { "all" }
                            )]
                        }
                        Err(e) => vec![format!("cannot write {file}: {e}")],
                    }
                }
                None => {
                    self.sql_out = None;
                    vec!["output: the transcript".into()]
                }
            },
            ".import" => match (arg(0), arg(1)) {
                (Some(file), Some(table)) => self.import_file(file, table),
                _ => vec!["usage: .import FILE TABLE".into()],
            },
            ".read" => match arg(0) {
                Some(file) => match std::fs::read_to_string(file) {
                    Ok(text) => {
                        // Each statement runs as if typed, so a script's results and
                        // errors land in the transcript in order.
                        let statements: Vec<String> = text
                            .split(';')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .collect();
                        let n = statements.len();
                        for stmt in statements {
                            self.execute_sql(&stmt);
                        }
                        vec![format!(
                            "ran {n} statement{} from {file}",
                            if n == 1 { "" } else { "s" }
                        )]
                    }
                    Err(e) => vec![format!("cannot read {file}: {e}")],
                },
                None => vec!["usage: .read FILE".into()],
            },
            ".backup" => match (arg(0), self.sqlite()) {
                (Some(file), Some(s)) => {
                    let dest = std::path::Path::new(file);
                    if dest.exists() {
                        vec![format!("{file} already exists")]
                    } else {
                        match s.backup_to(dest) {
                            Ok(()) => vec![format!("wrote {file}")],
                            Err(e) => vec![format!("{e}")],
                        }
                    }
                }
                (None, _) => vec!["usage: .backup FILE".into()],
                _ => return,
            },
            ".recover" => {
                // The script is far too long to read in a transcript, so it goes
                // to a file and the transcript gets the tally.
                let path = match self.sqlite() {
                    Some(s) => s.path.clone(),
                    None => return,
                };
                match crate::recover::recover(&path) {
                    Ok(found) => {
                        let sql = crate::recover::to_sql(&found);
                        let dest = arg(0)
                            .map(PathBuf::from)
                            .unwrap_or_else(|| PathBuf::from("recovered.sql"));
                        let mut lines = vec![format!(
                            "{} rows across {} tables, {} in lost_and_found",
                            found.rows.len(),
                            found.tables.len(),
                            found.orphans()
                        )];
                        lines.extend(found.notes.clone());
                        match std::fs::write(&dest, sql.as_bytes()) {
                            Ok(()) => lines.push(format!("script written to {}", dest.display())),
                            Err(e) => lines.push(format!("cannot write {}: {e}", dest.display())),
                        }
                        lines
                    }
                    Err(e) => vec![format!("recover failed: {e}")],
                }
            }
            ".expert" => {
                // The advice is about the statement above, which is the one the
                // transcript last ran.
                let last = self
                    .sql
                    .as_ref()
                    .and_then(|e| e.last_statement())
                    .unwrap_or_default();
                if last.is_empty() {
                    vec![".expert: run a statement first, then ask about it".into()]
                } else {
                    match self.sqlite().map(|s| s.index_advice(&last)) {
                        Some(Ok(advice)) if advice.is_empty() => {
                            vec![format!("{last}: the planner scans nothing in full")]
                        }
                        Some(Ok(advice)) => advice,
                        Some(Err(e)) => vec![format!("{e}")],
                        None => return,
                    }
                }
            }
            // `.load FILE` is the shell's own extension loader, and DB
            // Browser's "Load Extension".
            // `.project` is DB Browser's Save Project / Open Project: the
            // session's settings, not the data.
            ".project" => match (args.first().map(|s| s.to_lowercase()), args.get(1)) {
                (Some(verb), Some(file)) if verb == "save" => self.save_project(file),
                (Some(verb), Some(file)) if verb == "open" => self.load_project(file),
                _ => vec!["usage: .project save|open FILE".into()],
            },
            ".load" => match args.first() {
                Some(path) => match self.sqlite().map(|s| s.load_extension(Path::new(path))) {
                    Some(Ok(())) => vec![format!("loaded {path}")],
                    Some(Err(e)) => vec![format!("{e:#}")],
                    None => Vec::new(),
                },
                None => vec!["usage: .load FILE".into()],
            },
            ".vacuum" | ".analyze" | ".reindex" => {
                let op = match cmd.as_str() {
                    ".vacuum" => crate::sqlite::Maintenance::Vacuum,
                    ".analyze" => crate::sqlite::Maintenance::Analyze,
                    _ => crate::sqlite::Maintenance::Reindex,
                };
                match self.sqlite().map(|s| s.maintain(op)) {
                    Some(Ok(delta)) => vec![format!(
                        "{}: done, {}",
                        op.label(),
                        if delta == 0 {
                            "no change in size".to_string()
                        } else if delta < 0 {
                            format!("{} smaller", human_size((-delta) as u64))
                        } else {
                            format!("{} larger", human_size(delta as u64))
                        }
                    )],
                    Some(Err(e)) => vec![format!("{e}")],
                    None => return,
                }
            }
            ".quit" | ".exit" => {
                self.quit = true;
                return;
            }
            other => vec![format!("unknown command {other:?} — .help lists them")],
        };
        if let Some(e) = self.sql.as_mut() {
            e.push(Entry::Note(note));
        }
    }

    /// `.import FILE TABLE`, sharing the reader and the insert path with the
    /// `--import` flag.
    fn import_file(&mut self, file: &str, table: &str) -> Vec<String> {
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => return vec![format!("cannot read {file}: {e}")],
        };
        let sep = match std::path::Path::new(file)
            .extension()
            .and_then(|e| e.to_str())
        {
            Some("tsv") | Some("tab") => '\t',
            _ => ',',
        };
        let csv = match crate::import::parse(&text, sep) {
            Ok(c) => c,
            Err(e) => return vec![format!("{file}: {e}")],
        };
        let done = match self.sqlite() {
            Some(s) => s.import_rows(table, &csv.header, &csv.rows),
            None => return Vec::new(),
        };
        match done {
            Ok(n) => {
                self.load_table();
                vec![format!("imported {n} rows into {table}")]
            }
            Err(e) => vec![format!("{e}")],
        }
    }

    /// Write a result set to wherever `.output` / `.once` points, returning what to
    /// report. `None` when no redirect is active, which leaves the transcript to
    /// show the result itself.
    fn write_result(
        &mut self,
        columns: &[String],
        rows: &[Vec<String>],
        literals: &[Vec<String>],
    ) -> Option<String> {
        let (path, once) = self.sql_out.clone()?;
        let text = self.sql_mode.render(
            columns,
            rows,
            literals,
            &self.current_table().unwrap_or_else(|| "table".into()),
            self.sql_headers,
        );
        // A redirect collects every result until it is reset, as the shell's
        // `.output` does — so the second statement must not erase the first.
        let note = match append_to(&path, text.as_bytes()) {
            Ok(()) => format!(
                "wrote {} row{} to {} as {}",
                rows.len(),
                if rows.len() == 1 { "" } else { "s" },
                path.display(),
                self.sql_mode.label()
            ),
            Err(e) => format!("cannot write {}: {e}", path.display()),
        };
        if once {
            self.sql_out = None;
        }
        Some(note)
    }

    fn key_top(&mut self, code: KeyCode) {
        let action = match self.top.as_mut() {
            Some(m) => m.on_key(code, self.page_rows.max(1)),
            None => {
                self.screen = Screen::Main;
                return;
            }
        };
        if let Some(note) = self.top.as_mut().and_then(|m| m.note.take()) {
            self.notify(note);
        }
        match action {
            crate::monitor::Action::None => {}
            crate::monitor::Action::Back => {
                self.top = None;
                self.screen = Screen::Main;
            }
            crate::monitor::Action::Quit => {
                if !self.guard_pending(Exit::Quit) {
                    self.quit = true;
                }
            }
            // Opening from the monitor leaves the app the way `o` does, with the
            // chosen file carried out.
            crate::monitor::Action::Open(path) => {
                self.open_next = Some(path);
                self.back_to_files();
            }
            crate::monitor::Action::Frames(path) => self.open_frames(&path),
        }
    }

    /// Open the log walker for `path`. Column names come from the open database
    /// when it is the same file, so a decoded row can be labelled.
    fn open_frames(&mut self, path: &std::path::Path) {
        let columns: std::collections::HashMap<String, Vec<String>> = match self.sqlite() {
            Some(s) if s.path == path => s.schema_names().into_iter().collect(),
            _ => match crate::sqlite::SqliteStore::open(path) {
                Ok(s) => s.schema_names().into_iter().collect(),
                Err(_) => Default::default(),
            },
        };
        match crate::frames::FrameView::open(path, columns) {
            Some(view) => {
                let n = view.frames.len();
                self.walk = Some(view);
                self.screen = Screen::Frames;
                self.status =
                    format!("{n} frames · j/k step · [ ] commits · Esc back to the monitor");
            }
            None => self.notify("no write-ahead log to walk (journal_mode is not WAL)"),
        }
    }

    fn key_frames(&mut self, code: KeyCode) {
        let page = self.page_rows.max(1);
        let action = match self.walk.as_mut() {
            Some(w) => w.on_key(code, page),
            None => {
                self.screen = Screen::Top;
                return;
            }
        };
        match action {
            crate::frames::Action::None => {}
            crate::frames::Action::Back => {
                self.walk = None;
                // Back to the monitor it was opened from, which is still there.
                self.screen = Screen::Top;
            }
            crate::frames::Action::Quit => {
                if !self.guard_pending(Exit::Quit) {
                    self.quit = true;
                }
            }
        }
    }

    fn render_hex(&mut self, f: &mut Frame, area: Rect) {
        let theme = self.ov.theme;
        self.hex_area = area;
        if let Some(ed) = self.hex.as_mut() {
            ed.render(f, area, &theme);
        }
    }

    fn render_schema(&mut self, f: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        // Where each object's header line lands, so the scroll can follow the
        // selection rather than counting statements by hand.
        let mut anchor: Vec<usize> = Vec::new();
        for (i, (ty, name, sql)) in self.schema.iter().enumerate() {
            anchor.push(lines.len());
            let selected = i == self.schema_idx;
            let mut head = Style::default().add_modifier(Modifier::BOLD);
            if selected {
                head = head.add_modifier(Modifier::REVERSED);
            }
            let mut spans = vec![
                Span::styled(
                    format!("{} {:<6} ", if selected { "▸" } else { " " }, ty),
                    Style::default().fg(self.ov.theme.alt),
                ),
                Span::styled(name.clone(), head),
            ];
            if let Some(n) = self.schema_counts.get(name) {
                spans.push(Span::styled(
                    format!("  {n} row{}", if *n == 1 { "" } else { "s" }),
                    Style::default().fg(self.ov.theme.dim),
                ));
            }
            lines.push(Line::from(spans));
            for l in sql.lines() {
                lines.push(Line::from(Span::styled(
                    format!("    {}", l),
                    Style::default().fg(self.ov.theme.dim),
                )));
            }
            lines.push(Line::from(""));
        }
        let height = (area.height.saturating_sub(2) as usize).max(1);
        // Keep the selected object's header on screen, and as much of its
        // statement under it as fits.
        if let Some(&at) = anchor.get(self.schema_idx) {
            if at < self.schema_scroll {
                self.schema_scroll = at;
            } else {
                let end = anchor
                    .get(self.schema_idx + 1)
                    .copied()
                    .unwrap_or(lines.len());
                let want = end.min(at + height);
                if want > self.schema_scroll + height {
                    self.schema_scroll = want - height;
                }
            }
        }
        let visible: Vec<Line> = lines
            .into_iter()
            .skip(self.schema_scroll)
            .take(height)
            .collect();
        f.render_widget(
            Paragraph::new(visible).block(Block::default().borders(Borders::ALL).title(format!(
                " schema — {} objects (j/k select · Enter edit · a table · i index · \
                 d drop · y copy · R counts · Esc back) ",
                self.schema.len()
            ))),
            area,
        );
    }

    fn render_sqlite(&mut self, f: &mut Frame, area: Rect) {
        let cols = Layout::horizontal([Constraint::Length(24), Constraint::Min(10)]).split(area);
        let (rect_left, rect_right) = (cols[0], cols[1]);

        let s = self.sqlite().unwrap();
        // Left: the tables the filter leaves listed.
        let visible: Vec<usize> = s
            .tables
            .iter()
            .enumerate()
            .filter(|(_, t)| filter_passes(&self.filter, t))
            .map(|(i, _)| i)
            .collect();
        // A table whose virtual-table module this binary does not have — an FTS
        // index built with a custom tokenizer, say — cannot be read by anyone, so
        // it is marked rather than left to fail on selection.
        let items: Vec<ListItem> = visible
            .iter()
            .map(|&i| {
                let name = &s.tables[i];
                ListItem::new(match s.unreadable_reason(name) {
                    Some(_) => format!("{name}  ⃰"),
                    None => name.clone(),
                })
            })
            .collect();
        let mut lstate = ListState::default();
        lstate.select(
            visible
                .iter()
                .position(|&i| i == self.table_idx)
                .or(Some(0)),
        );
        let left_border = self.pane_style(Focus::Left);
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(left_border)
                    .title(if self.filter.is_empty() {
                        format!(
                            " {} — tables ({}) ",
                            s.path.file_name().and_then(|n| n.to_str()).unwrap_or("db"),
                            s.tables.len()
                        )
                    } else {
                        format!(
                            " tables {}/{}  /{} ",
                            visible.len(),
                            s.tables.len(),
                            self.filter
                        )
                    }),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, cols[0], &mut lstate);
        let off_left = lstate.selected().map(|_| lstate.offset()).unwrap_or(0);
        let mut off_right = 0usize;

        // Right: row grid.
        let title = match self.current_table() {
            Some(t) => {
                let (total, exact) = self
                    .rows
                    .as_ref()
                    .map(|r| (r.total, r.total_exact))
                    .unwrap_or((0, true));
                let sorted = match &self.sort {
                    Some(s) => format!(" — sorted {} {}", s.column, arrow(s.desc)),
                    None => String::new(),
                };
                // `500+` rather than a wrong number: counting the rest is a full
                // scan, so it runs in the background and the title firms up when
                // it lands. The trailing `…` says that scan is still running.
                let of = match (exact, self.counting()) {
                    (true, _) => total.to_string(),
                    (false, true) => format!("{total}+ …"),
                    (false, false) => format!("{total}+"),
                };
                // Work that outran its grace period is still coming.
                let loading = match (self.loading(), self.searching()) {
                    (_, true) => " · searching",
                    (true, false) => " · loading",
                    (false, false) => "",
                };
                format!(
                    " {} — rows {}..{} of {}{}{} ",
                    t,
                    self.page_offset,
                    self.page_offset + self.rows.as_ref().map(|r| r.rows.len() as i64).unwrap_or(0),
                    of,
                    sorted,
                    loading
                )
            }
            None => " (no table) ".into(),
        };

        if let Some(rv) = &self.rows {
            // Which columns are drawn: the ones not hidden, windowed so the
            // cursor is on screen, with the frozen ones always at the left.
            let view = self
                .current_table()
                .map(|t| self.browse.view(&t))
                .unwrap_or_default();
            let visible = view.visible(&rv.columns);
            // Every column is drawn CELL_W wide inside the pane's borders.
            let fits = (rect_right.width.saturating_sub(2) as usize / CELL_W).max(1);
            let rowid_col = view.show_rowid && rv.rowids.iter().any(Option::is_some);
            let (drawn, scroll) = crate::browse::layout(
                &visible,
                view.frozen,
                self.col_idx,
                self.col_scroll,
                fits.saturating_sub(usize::from(rowid_col)),
            );
            self.col_scroll = scroll;

            let mut head: Vec<Cell> = Vec::new();
            if rowid_col {
                head.push(
                    Cell::from("rowid").style(Style::default().fg(self.ov.theme.dim).italic()),
                );
            }
            head.extend(drawn.iter().map(|&i| {
                let c = &rv.columns[i];
                let st = if i == self.col_idx && self.focus == Focus::Right {
                    Style::default()
                        .fg(self.ov.theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                };
                // Mark the sorted column, a frozen one, and a formatted one in
                // the header — the settings are otherwise invisible.
                let mut label = match &self.sort {
                    Some(s) if s.column == *c => format!("{} {}", c, arrow(s.desc)),
                    _ => c.clone(),
                };
                if visible
                    .iter()
                    .position(|&v| v == i)
                    .is_some_and(|p| p < view.frozen)
                {
                    label.push('▏');
                }
                if view.format(c) != crate::browse::Format::Default {
                    label.push('ƒ');
                }
                Cell::from(label).style(st)
            }));
            let header = Row::new(head);
            let body = rv.rows.iter().enumerate().map(|(r, row)| {
                let mut cells: Vec<Cell> = Vec::new();
                if rowid_col {
                    let id = rv
                        .rowids
                        .get(r)
                        .copied()
                        .flatten()
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    cells.push(Cell::from(id).style(Style::default().fg(self.ov.theme.dim)));
                }
                cells.extend(drawn.iter().map(|&i| {
                    let text = row.get(i).map(String::as_str).unwrap_or("");
                    let cell = Cell::from(truncate(text, 40));
                    // The first conditional-format rule that matches paints it.
                    match view.rule_for(&rv.columns[i], text) {
                        Some(rule) => {
                            let mut st = Style::default().fg(rule.color.color());
                            if rule.bold {
                                st = st.add_modifier(Modifier::BOLD);
                            }
                            cell.style(st)
                        }
                        None => cell,
                    }
                }));
                Row::new(cells)
            });
            let widths: Vec<Constraint> = (0..drawn.len() + usize::from(rowid_col))
                .map(|_| Constraint::Length(CELL_W as u16 - 1))
                .collect();
            let mut tstate = TableState::default();
            tstate.select(Some(self.row_idx));
            let table = Table::new(body, widths)
                .header(header)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(self.pane_style(Focus::Right))
                        .title(title),
                )
                .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            f.render_stateful_widget(table, cols[1], &mut tstate);
            off_right = tstate.offset();
        } else {
            let p = Paragraph::new("no rows").block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(self.pane_style(Focus::Right))
                    .title(title),
            );
            f.render_widget(p, cols[1]);
        }

        // Capture hit-test geometry for mouse clicks.
        self.click_left = rect_left;
        self.click_right = rect_right;
        self.off_left = off_left;
        self.off_right = off_right;
    }

    fn render_rkyv(&mut self, f: &mut Frame, area: Rect) {
        // Records mutates click geometry, so handle it before borrowing the store.
        if self.rkyv_view == RkyvView::Records {
            self.render_rkyv_records(f, area);
            return;
        }
        let r = match &self.store {
            Store::Rkyv(r) => r,
            _ => return,
        };
        match self.rkyv_view {
            RkyvView::Info => self.render_rkyv_info(f, area, r),
            RkyvView::Strings => self.render_rkyv_strings(f, area),
            RkyvView::Hex => self.render_rkyv_hex(f, area, r),
            RkyvView::Records => {}
        }
    }

    fn render_rkyv_records(&mut self, f: &mut Frame, area: Rect) {
        let cols =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Min(10)]).split(area);
        self.click_records = cols[0];
        let d = match &self.decoded {
            Some(d) => d,
            None => return,
        };

        // Left: keys the filter leaves listed.
        let visible: Vec<usize> = d
            .records
            .iter()
            .enumerate()
            .filter(|(_, r)| filter_passes(&self.filter, &r.key))
            .map(|(i, _)| i)
            .collect();
        // Fit to the pane, not to a fixed 60: a wide terminal was still cutting
        // keys at 60. Cut from the front, since path-like keys share a long
        // prefix and are told apart only by their tail.
        let key_width = cols[0].width.saturating_sub(2) as usize;
        let items: Vec<ListItem> = visible
            .iter()
            .map(|&i| ListItem::new(truncate_start(&d.records[i].key, key_width)))
            .collect();
        let mut st = ListState::default();
        st.select(
            visible
                .iter()
                .position(|&i| i == self.record_idx)
                .or(Some(0)),
        );
        let title = if self.filter.is_empty() {
            format!(" {} — {} keys ", d.format, d.records.len())
        } else {
            format!(
                " {} — {}/{} keys  /{} ",
                d.format,
                visible.len(),
                d.records.len(),
                self.filter
            )
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.ov.theme.accent))
                    .title(title),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, cols[0], &mut st);
        let off_records = st.offset();

        // Right: selected value — decoded scalar fields, then a hex dump.
        let mut lines: Vec<Line> = Vec::new();
        if let Some(rec) = d.records.get(self.record_idx) {
            // The list can only show a tail of the key, so the whole thing is
            // spelled out here, wrapped rather than cut.
            let body = (cols[1].width as usize).saturating_sub(2 + 22).max(1);
            let key: Vec<char> = rec.key.chars().collect();
            for (n, chunk) in key.chunks(body).enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<22}", if n == 0 { "key" } else { "" }),
                        Style::default().fg(self.ov.theme.dim),
                    ),
                    Span::raw(chunk.iter().collect::<String>()),
                ]));
            }
            for (name, val) in &rec.fields {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<22}", name),
                        Style::default().fg(self.ov.theme.dim),
                    ),
                    Span::raw(val.clone()),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("value — {} bytes (hex):", rec.value.len()),
                Style::default().fg(self.ov.theme.primary),
            )));
            // Whatever height is left once the fields and the wrapped key are on
            // screen, minus the borders and the "value" caption.
            let rows = (area.height as usize).saturating_sub(lines.len() + 3);
            for i in 0..rows {
                let off = i * 16;
                if off >= rec.value.len() {
                    break;
                }
                lines.push(Line::from(hexedit::hex_dump_line(
                    off,
                    &rec.value[off.min(rec.value.len())..(off + 16).min(rec.value.len())],
                )));
            }
        }
        let p =
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" value "));
        f.render_widget(p, cols[1]);
        self.off_records = off_records;
    }

    fn render_rkyv_info(&self, f: &mut Frame, area: Rect, r: &RkyvStore) {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("file:    ", Style::default().fg(self.ov.theme.dim)),
                Span::raw(r.path.display().to_string()),
            ]),
            Line::from(vec![
                Span::styled("size:    ", Style::default().fg(self.ov.theme.dim)),
                Span::raw(format!("{} bytes", r.len())),
            ]),
            Line::from(vec![
                Span::styled("strings: ", Style::default().fg(self.ov.theme.dim)),
                Span::raw(if self.strings_truncated {
                    // Say so rather than implying the list is everything.
                    format!(
                        "{} runs (>= {} printable bytes) — capped, scanned first {}",
                        self.strings.len(),
                        MIN_STRING,
                        human_size(self.strings_scanned as u64)
                    )
                } else {
                    format!(
                        "{} runs (>= {} printable bytes)",
                        self.strings.len(),
                        MIN_STRING
                    )
                }),
            ]),
        ];

        match &self.decoded {
            Some(d) => {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("format:  ", Style::default().fg(self.ov.theme.dim)),
                    Span::styled(d.format.clone(), Style::default().fg(self.ov.theme.label)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("records: ", Style::default().fg(self.ov.theme.dim)),
                    Span::raw(d.records.len().to_string()),
                ]));
                lines.push(Line::from(""));
                for (name, val) in &d.header {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {:<16}", name),
                            Style::default().fg(self.ov.theme.dim),
                        ),
                        Span::raw(val.clone()),
                    ]));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Views:  0 Records (key/value)  2 Strings  3 Hex",
                    Style::default().fg(self.ov.theme.dim),
                )));
            }
            None => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "unrecognized rkyv archive: no matching format decoder.",
                    Style::default().fg(self.ov.theme.primary),
                )));
                lines.push(Line::from(Span::styled(
                    "rkyv stores no field names or type tags, so an unknown type",
                    Style::default().fg(self.ov.theme.primary),
                )));
                lines.push(Line::from(Span::styled(
                    "cannot be decoded generically — showing raw structure.",
                    Style::default().fg(self.ov.theme.primary),
                )));
                lines.push(Line::from(Span::styled(
                    "Views:  2 Strings (embedded text)  3 Hex (raw bytes)",
                    Style::default().fg(self.ov.theme.dim),
                )));
            }
        }
        let p = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" rkyv / binary — Info "),
        );
        f.render_widget(p, area);
    }

    fn render_rkyv_strings(&self, f: &mut Frame, area: Rect) {
        let visible = self.visible_strings();
        let items: Vec<ListItem> = visible
            .iter()
            .map(|&i| {
                let h = &self.strings[i];
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:08x}  ", h.offset),
                        Style::default().fg(self.ov.theme.dim),
                    ),
                    Span::raw(truncate(&h.text, 200)),
                ]))
            })
            .collect();
        let mut st = ListState::default();
        st.select(
            visible
                .iter()
                .position(|&i| i == self.string_idx)
                .or(Some(0)),
        );
        let capped = if self.strings_truncated { "+" } else { "" };
        let title = if self.filter.is_empty() {
            format!(" Strings ({}{}) ", self.strings.len(), capped)
        } else {
            format!(
                " Strings ({}/{}{})  /{} ",
                visible.len(),
                self.strings.len(),
                capped,
                self.filter
            )
        };
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, area, &mut st);
    }

    fn render_rkyv_hex(&self, f: &mut Frame, area: Rect, r: &RkyvStore) {
        let rows_visible = area.height.saturating_sub(2) as usize;
        let start = self.hex_row * 16;
        let mut lines = Vec::new();
        for i in 0..rows_visible {
            let off = start + i * 16;
            if off >= r.len() {
                break;
            }
            lines.push(Line::from(r.hex_row(off)));
        }
        let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!(
            " Hex — offset {:#x} / {} bytes ",
            start,
            r.len()
        )));
        f.render_widget(p, area);
    }

    fn render_status(&self, f: &mut Frame, area: Rect) {
        // Unwritten changes are the one piece of state that is invisible in the
        // grid — the cell already shows its new value — so the status line says
        // so until they are written or reverted.
        let line = if self.has_pending() {
            Line::from(vec![
                Span::styled(
                    " ● unwritten changes (W write · R revert) ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(self.ov.theme.primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(self.status.clone()),
            ])
        } else {
            Line::from(self.status.clone())
        };
        let p = Paragraph::new(line).style(Style::default().fg(Color::Black).bg(Color::Gray));
        f.render_widget(p, area);
    }

    fn render_input(&self, f: &mut Frame, title: &str, buf: &str) {
        let area = centered(f.area(), 60, 3);
        f.render_widget(Clear, area);
        // Draw the buffer with a reversed block cursor over the char at the
        // cursor (or a trailing space when the cursor is at the end).
        let cur = self.input_cursor.min(buf.len());
        let (pre, rest) = buf.split_at(cur);
        let (at, post) = match rest.char_indices().nth(1) {
            Some((i, _)) => rest.split_at(i),
            None => (rest, ""),
        };
        let at_disp = if at.is_empty() { " " } else { at };
        let line = Line::from(vec![
            Span::raw(pre.to_string()),
            Span::styled(
                at_disp.to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            Span::raw(post.to_string()),
        ]);
        let p = Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(self.ov.theme.accent))
                .title(format!(" {} ", title)),
        );
        f.render_widget(p, area);
    }

    fn pane_style(&self, which: Focus) -> Style {
        if self.focus == which {
            Style::default().fg(self.ov.theme.accent)
        } else {
            Style::default().fg(self.ov.theme.dim)
        }
    }
}

/// Recent-files picker shown when zdbview is launched with no file argument.
/// Returns the chosen file, or `None` if the user quits.
/// One row of the picker: a remembered file, or one the startup scan found.
pub struct Choice {
    pub path: PathBuf,
    pub kind: Kind,
    /// When this file was last opened (recent files only).
    pub opened: Option<std::time::SystemTime>,
    /// Recognized rkyv format, when the scan's magic sniff named one.
    pub format: Option<&'static str>,
    /// File size, for scan hits (recent rows show their age instead).
    pub size: Option<u64>,
    /// Last modification time, used to order scan hits newest-first.
    pub modified: Option<std::time::SystemTime>,
    /// Scan display priority (see `scan::Hit::rank`).
    rank: u8,
}

impl Choice {
    fn from_entry(e: &Entry) -> Self {
        Choice {
            path: e.path.clone(),
            kind: e.kind,
            opened: Some(e.opened),
            format: None,
            size: None,
            modified: None,
            rank: 0,
        }
    }

    fn from_hit(h: crate::scan::Hit) -> Self {
        Choice {
            path: h.path,
            kind: h.kind,
            opened: None,
            format: h.format,
            size: Some(h.size),
            modified: Some(h.modified),
            rank: h.rank,
        }
    }

    fn name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
    }
}

/// What the picker returned: the file to open and the scheme it was showing.
pub struct Picked {
    pub path: PathBuf,
    pub theme: Theme,
}

/// The path a row is recognized by: symlinks resolved, so the same file reached
/// two ways is one row, falling back to the path itself when it cannot be
/// resolved (a file that has gone away is still worth listing once).
fn canon(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Merge scan hits into the list, dropping any path already present (a recent
/// file the scan also found stays a recent file, keeping its age column).
///
/// `seen` holds the resolved path of every row already listed. It is the caller's
/// so the resolution is paid once per row: an index of the whole disk is hundreds
/// of rows, merged again on every return to the picker and once per frame while a
/// walk streams, and resolving each listed row against each incoming hit made
/// coming back from a file take most of a second.
fn merge_hits(choices: &mut Vec<Choice>, seen: &mut HashSet<PathBuf>, hits: Vec<crate::scan::Hit>) {
    for hit in hits {
        if seen.insert(canon(&hit.path)) {
            choices.push(Choice::from_hit(hit));
        }
    }
    // Recent files keep their recency order at the top. Scan hits below them go
    // by rank first (recognized shards, then other rkyv archives, then
    // databases), newest-first within a rank, shallowest path breaking ties.
    let first_scanned = choices
        .iter()
        .position(|c| c.opened.is_none())
        .unwrap_or(choices.len());
    choices[first_scanned..].sort_by_key(|c| {
        (
            c.rank,
            std::cmp::Reverse(c.modified),
            c.path.components().count(),
        )
    });
}

/// Everything the picker needs: the remembered files, the scheme override, and
/// where its scan rows come from.
pub struct Picker<'a> {
    pub recent: &'a [Entry],
    /// The scheme to show. Carried from the previous screen when there was one,
    /// so a concurrent prefs write cannot change it mid-session.
    pub theme: Theme,
    /// Rows restored from the saved scan (appdata), shown immediately.
    pub cached: Vec<crate::scan::Hit>,
    /// How old those rows are, for the title.
    pub cache_age: Option<std::time::Duration>,
    /// A walk already in progress, when the index was missing or out of date.
    pub scan: Option<crate::scan::Scan>,
    /// The roots that walk was given: the changed directories when it is a
    /// refresh, the default set otherwise.
    pub walking: Vec<crate::scan::Root>,
    /// Whether that walk is a refresh of an index that is otherwise complete.
    pub refresh: bool,
    /// Roots to walk when the user asks for a rescan with `r`.
    pub roots: Vec<crate::scan::Root>,
    /// Whether a finished walk may be written to appdata (not for `--scan`,
    /// whose roots are not the default set).
    pub persist: bool,
}

/// The walk currently running, and what kind it is.
struct Walking {
    scan: Option<crate::scan::Scan>,
    /// The roots this walk was given: the changed directories for a refresh, the
    /// default set for a full walk.
    roots: Vec<crate::scan::Root>,
    /// Whether the index being updated is otherwise complete, i.e. this walk is
    /// a refresh rather than a walk of everything.
    refresh: bool,
    /// The default root set, watched by the index whatever this walk covers.
    default: Vec<crate::scan::Root>,
}

impl Walking {
    /// Every directory the saved index watches: the default roots, plus whatever
    /// this walk covered.
    fn watched(&self) -> Vec<crate::scan::Root> {
        let mut all = self.default.clone();
        all.extend(self.roots.iter().cloned());
        all
    }

    /// Write the index as it stands after a walk that ran to the end.
    fn save_finished(&self, hits: &[crate::scan::Hit]) {
        crate::scan::save_cache(crate::scan::Save {
            hits,
            roots: &self.watched(),
            complete: true,
            unfinished: &[],
        });
    }
}

/// Stop a walk that is still running and keep what it found so far.
///
/// A walk of the whole filesystem takes minutes, so throwing it away because a
/// file was picked in the first second means paying for all of it again on the
/// next start. What the save records depends on which walk was cut short: a
/// refresh leaves the index whole and re-flags only the directories it did not
/// finish reading, while a full walk that never finished has to run again.
fn park_scan(walk: &mut Walking, scanned: &mut Vec<crate::scan::Hit>, persist: bool) {
    let Some(sc) = walk.scan.as_mut() else { return };
    sc.cancel();
    scanned.extend(sc.drain());
    if persist && !scanned.is_empty() {
        crate::scan::save_cache(crate::scan::Save {
            hits: scanned,
            roots: &walk.watched(),
            complete: walk.refresh,
            unfinished: if walk.refresh { &walk.roots } else { &[] },
        });
    }
}

pub fn pick_mru(terminal: &mut DefaultTerminal, mut p: Picker<'_>) -> Result<Option<Picked>> {
    let entries = p.recent;
    let mut walk = Walking {
        scan: p.scan.take(),
        roots: std::mem::take(&mut p.walking),
        refresh: p.refresh,
        default: p.roots.clone(),
    };
    let mut cache_age = p.cache_age;
    // Hits are kept as well as merged so a finished walk can be saved.
    let mut scanned: Vec<crate::scan::Hit> = std::mem::take(&mut p.cached);
    // Recent files first, then whatever the scan turns up.
    let mut choices: Vec<Choice> = entries.iter().map(Choice::from_entry).collect();
    let mut listed: HashSet<PathBuf> = choices.iter().map(|c| canon(&c.path)).collect();
    merge_hits(&mut choices, &mut listed, scanned.clone());

    // `/` filters the list: `view` holds the indices still listed and `sel` is a
    // position within it, so navigation and clicks address rows that are visible.
    let mut filter = String::new();
    let mut typing = false;
    let mut sel = 0usize;
    let mut pending_g = false;
    // The row `/` was pressed on, restored when the filter is cancelled.
    let mut before_filter = 0usize;
    // First row the list drew last frame, for mapping a click to an entry.
    let mut list_offset = 0usize;
    // The write monitor, when `w` has opened it over the list.
    let mut monitor: Option<crate::monitor::Monitor> = None;

    // The picker carries the same overlay layer as the main screens, so `h`,
    // `c` and `C` work here too — and the scheme it is showing is the one the
    // opened file gets, handed over rather than re-read.
    let mut ov = Overlays::new(p.theme);

    loop {
        // Pull in whatever the scan thread produced since the last frame, and
        // save the finished list so the next start does not walk again.
        let scanning = match walk.scan.as_mut() {
            Some(sc) => {
                let hits = sc.drain();
                scanned.extend(hits.iter().cloned());
                merge_hits(&mut choices, &mut listed, hits);
                if sc.running {
                    Some(sc.found)
                } else {
                    if p.persist {
                        walk.save_finished(&scanned);
                        cache_age = Some(std::time::Duration::ZERO);
                    }
                    walk.scan = None;
                    None
                }
            }
            None => None,
        };

        // Rows the filter leaves listed, and a selection inside that list.
        let view: Vec<usize> = choices
            .iter()
            .enumerate()
            .filter(|(_, c)| filter_passes(&filter, c.path.to_str().unwrap_or("")))
            .map(|(i, _)| i)
            .collect();
        sel = sel.min(view.len().saturating_sub(1));

        // List height for paging: the body minus its borders.
        let page = terminal
            .size()
            .map(|s| s.height.saturating_sub(3) as usize)
            .unwrap_or(10)
            .max(1);
        let prompt = typing.then_some(filter.as_str());
        // The monitor takes the whole screen while it is up; the overlay layer
        // still draws on top of either.
        if let Some(m) = monitor.as_mut() {
            m.tick();
        }
        let ctx = if monitor.is_some() {
            HelpCtx::Top
        } else {
            HelpCtx::Picker
        };
        terminal.draw(|f| {
            match monitor.as_ref() {
                Some(m) => m.render(f, f.area(), &ov.theme),
                None => {
                    list_offset = render_picker(
                        f, &choices, &view, sel, &filter, prompt, scanning, cache_age, &ov.theme,
                    );
                }
            }
            ov.render(f, ctx);
        })?;
        if !event::poll(TICK)? {
            ov.expire_toast();
            continue;
        }
        let ev = event::read()?;
        ov.expire_toast();

        // While the monitor is up it owns the keys, except the overlay's own.
        if let Some(m) = monitor.as_mut() {
            match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if ov.on_key(key.code) {
                        continue;
                    }
                    let size = terminal.size().unwrap_or_default();
                    let page = crate::monitor::Monitor::page_rows(Rect::new(
                        0,
                        0,
                        size.width,
                        size.height,
                    ));
                    let action = m.on_key(key.code, page);
                    if let Some(note) = m.note.take() {
                        ov.toast(note);
                    }
                    match action {
                        crate::monitor::Action::None => {}
                        crate::monitor::Action::Back => monitor = None,
                        crate::monitor::Action::Quit => {
                            park_scan(&mut walk, &mut scanned, p.persist);
                            return Ok(None);
                        }
                        crate::monitor::Action::Open(path) => {
                            park_scan(&mut walk, &mut scanned, p.persist);
                            return Ok(Some(Picked {
                                path,
                                theme: ov.theme,
                            }));
                        }
                        // The log walker lives on a file that is open, so from the
                        // picker `F` opens the database and lands in the monitor
                        // there instead of walking from nowhere.
                        crate::monitor::Action::Frames(path) => {
                            park_scan(&mut walk, &mut scanned, p.persist);
                            return Ok(Some(Picked {
                                path,
                                theme: ov.theme,
                            }));
                        }
                    }
                }
                Event::Mouse(mouse) if !ov.on_mouse(mouse) => {
                    let size = terminal.size().unwrap_or_default();
                    m.on_mouse(mouse, Rect::new(0, 0, size.width, size.height));
                    if let Some(note) = m.note.take() {
                        ov.toast(note);
                    }
                }
                _ => {}
            }
            continue;
        }

        let last = view.len().saturating_sub(1);
        let pick = |i: usize| -> Option<PathBuf> { view.get(i).map(|&c| choices[c].path.clone()) };

        // Mouse: wheel moves the selection, a click opens the entry under it.
        if let Event::Mouse(m) = ev {
            if ov.on_mouse(m) {
                continue;
            }
            match m.kind {
                MouseEventKind::ScrollDown => sel = (sel + 1).min(last),
                MouseEventKind::ScrollUp => sel = sel.saturating_sub(1),
                MouseEventKind::Down(_) => {
                    // The list starts one row below the block's top border and is
                    // scrolled by `list_offset`; rows past the last entry select
                    // nothing.
                    let row = (m.row as usize).saturating_sub(1);
                    let clicked = row + list_offset;
                    if row < page && clicked < view.len() {
                        park_scan(&mut walk, &mut scanned, p.persist);
                        // Hand over the scheme on screen, so the file opens in it.
                        return Ok(pick(clicked).map(|path| Picked {
                            path,
                            theme: ov.theme,
                        }));
                    }
                }
                _ => {}
            }
            continue;
        }
        if let Event::Key(key) = ev {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            // Ctrl-f / Ctrl-b page like the app's grids do.
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('f') => sel = (sel + page).min(last),
                    KeyCode::Char('b') => sel = sel.saturating_sub(page),
                    _ => {}
                }
                continue;
            }

            // While the `/` prompt is open every key edits the filter, and the
            // list shrinks to the matches as it is typed.
            if typing {
                match filter_prompt_key(key.code, &mut filter, &mut sel, last, page) {
                    Prompt::Open => {}
                    Prompt::Accept => typing = false,
                    Prompt::Cancel => {
                        // Drop the filter and go back to the row `/` was pressed on.
                        typing = false;
                        filter.clear();
                        sel = before_filter;
                    }
                }
                continue;
            }

            // Overlay keys (open or drive: h/? c C) come before the picker's own.
            if ov.on_key(key.code) {
                continue;
            }

            if key.code == KeyCode::Char('g') {
                if pending_g {
                    pending_g = false;
                    sel = 0;
                } else {
                    pending_g = true;
                }
                continue;
            }
            pending_g = false;
            match key.code {
                // Esc clears an applied filter first, then quits.
                KeyCode::Esc if !filter.is_empty() => {
                    filter.clear();
                    sel = 0;
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    park_scan(&mut walk, &mut scanned, p.persist);
                    return Ok(None);
                }
                KeyCode::Char('/') => {
                    typing = true;
                    filter.clear();
                    before_filter = sel;
                    sel = 0;
                }
                // The same write monitor the app screens show, over the files
                // listed here plus the rest of the watched set.
                KeyCode::Char('w') => {
                    let mut targets: Vec<(PathBuf, Kind)> = view
                        .iter()
                        .map(|&i| (choices[i].path.clone(), choices[i].kind))
                        .collect();
                    targets.extend(watch_targets());
                    let m = crate::monitor::Monitor::new(targets);
                    if m.is_empty() {
                        ov.toast("nothing to watch yet");
                    } else {
                        ov.toast(format!("watching {} files for writes", m.len()));
                        monitor = Some(m);
                    }
                }
                // Within a filtered list every row matches, so n/N simply step.
                KeyCode::Char('n') => sel = (sel + 1).min(last),
                KeyCode::Char('N') => sel = sel.saturating_sub(1),
                // `r` walks again (keeping the rows on screen until new ones
                // arrive); `R` also drops the saved scan first.
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    if key.code == KeyCode::Char('R') {
                        crate::scan::clear_cache();
                    }
                    // Not parked: a rescan replaces the index rather than adding
                    // to it, and `R` has just deleted the saved one on purpose.
                    if let Some(sc) = &walk.scan {
                        sc.cancel();
                    }
                    scanned.clear();
                    choices.retain(|c| c.opened.is_some());
                    listed = choices.iter().map(|c| canon(&c.path)).collect();
                    sel = 0;
                    cache_age = None;
                    // Everything again, from nothing: a walk of every root, whose
                    // result is the whole index rather than a patch to one.
                    walk.roots = p.roots.clone();
                    walk.refresh = false;
                    walk.scan = Some(crate::scan::spawn(p.roots.clone()));
                    ov.toast("rescanning");
                }
                KeyCode::Char('G') => sel = last,
                // A screenful, from the height this frame was drawn at.
                KeyCode::PageDown => sel = (sel + page).min(last),
                KeyCode::PageUp => sel = sel.saturating_sub(page),
                KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(last),
                KeyCode::Enter => {
                    if let Some(path) = pick(sel) {
                        // Nothing more to walk once a file is chosen.
                        park_scan(&mut walk, &mut scanned, p.persist);
                        return Ok(Some(Picked {
                            path,
                            theme: ov.theme,
                        }));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Archives up to this size are decoded inline; bigger ones go to a thread.
/// 12MB takes ~0.8s to validate, 382MB takes ~25s, so the line sits below the
/// point where a person would notice the wait.
const DECODE_INLINE_MAX: usize = 4 * 1024 * 1024;

/// Append `bytes` to `path`, creating it if it is not there. `.output` collects
/// results until it is reset, and `.once` writes exactly one — which is the same
/// operation, since the file it writes to is fresh.
fn append_to(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(bytes)
}

/// What a background statistics pass returns: the columns, or why it could not.
type StatsResult = std::result::Result<Vec<crate::sqlite::ColumnStat>, String>;

/// Describe `table`'s columns on another thread, over a connection of its own —
/// `SqliteStore`'s connection belongs to the UI thread.
fn spawn_stats(path: std::path::PathBuf, table: String) -> std::sync::mpsc::Receiver<StatsResult> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = SqliteStore::open(&path)
            .and_then(|s| s.column_stats(&table))
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });
    rx
}

/// Validate and decode `bytes` on another thread.
fn spawn_decode(bytes: std::sync::Arc<[u8]>) -> std::sync::mpsc::Receiver<Option<Decoded>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(formats::try_decode(&bytes));
    });
    rx
}

/// What a key did to a `/` filter prompt. Shared by the picker and the write
/// monitor so both prompts behave identically.
#[derive(Debug, PartialEq, Eq)]
pub enum Prompt {
    /// Still typing.
    Open,
    /// Enter: keep the filter and close the prompt.
    Accept,
    /// Esc: discard it.
    Cancel,
}

/// Handle one key of a `/` filter prompt. The list stays navigable while the
/// pattern is typed, so the arrows and paging move the selection instead of being
/// swallowed; only Left/Right belong to the pattern itself.
pub fn filter_prompt_key(
    code: KeyCode,
    filter: &mut String,
    sel: &mut usize,
    last: usize,
    page: usize,
) -> Prompt {
    match code {
        KeyCode::Esc => return Prompt::Cancel,
        KeyCode::Enter => return Prompt::Accept,
        KeyCode::Backspace => {
            filter.pop();
            *sel = 0;
        }
        KeyCode::Up => *sel = sel.saturating_sub(1),
        KeyCode::Down => *sel = (*sel + 1).min(last),
        KeyCode::PageUp => *sel = sel.saturating_sub(page),
        KeyCode::PageDown => *sel = (*sel + page).min(last),
        KeyCode::Home => *sel = 0,
        KeyCode::End => *sel = last,
        KeyCode::Char(c) => {
            filter.push(c);
            // A changed pattern means a different list; start at its top.
            *sel = 0;
        }
        _ => {}
    }
    Prompt::Open
}

#[allow(clippy::too_many_arguments)]
fn render_picker(
    f: &mut Frame,
    choices: &[Choice],
    view: &[usize],
    sel: usize,
    filter: &str,
    prompt: Option<&str>,
    scanning: Option<usize>,
    cache_age: Option<std::time::Duration>,
    t: &Theme,
) -> usize {
    let outer = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
    let mut offset = 0usize;

    if view.is_empty() {
        let body = if !filter.is_empty() {
            vec![
                Line::from(""),
                Line::from(format!("  Nothing matches /{}", filter)),
                Line::from(""),
                Line::from(Span::styled(
                    "  Esc clears the filter",
                    Style::default().fg(t.dim),
                )),
            ]
        } else if scanning.is_some() {
            vec![
                Line::from(""),
                Line::from("  Scanning for databases and rkyv shards…"),
            ]
        } else {
            vec![
                Line::from(""),
                Line::from("  Nothing found."),
                Line::from(""),
                Line::from(Span::styled(
                    "  Open one with:  zdbview <file>",
                    Style::default().fg(t.dim),
                )),
                Line::from(Span::styled(
                    "  Or scan elsewhere:  zdbview --scan <dir>",
                    Style::default().fg(t.dim),
                )),
            ]
        };
        let p = Paragraph::new(body).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.accent))
                .title(" zdbview — files "),
        );
        f.render_widget(p, outer[0]);
    } else {
        let items: Vec<ListItem> = view
            .iter()
            .map(|&i| {
                let c = &choices[i];
                let dir = c.path.parent().and_then(|p| p.to_str()).unwrap_or("");
                let (badge, color) = match c.kind {
                    Kind::Sqlite => ("sqlite", t.primary),
                    Kind::Rkyv => ("rkyv  ", t.alt),
                };
                // Recent files show their age; scanned ones their size.
                let (age, age_style) = match (c.opened, c.size) {
                    (Some(when), _) => (mru::rel_age(when), Style::default().fg(t.dim)),
                    (None, Some(size)) => (human_size(size), Style::default().fg(t.label)),
                    (None, None) => ("found".to_string(), Style::default().fg(t.label)),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {} ", badge), Style::default().fg(color)),
                    Span::styled(
                        format!("{:<28}", truncate(c.name(), 28)),
                        Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("{:>10}  ", age), age_style),
                    Span::styled(truncate(dir, 52), Style::default().fg(t.label)),
                ]))
            })
            .collect();
        let mut st = ListState::default();
        st.select(Some(sel.min(view.len() - 1)));
        let recent = view
            .iter()
            .filter(|&&i| choices[i].opened.is_some())
            .count();
        let title = if !filter.is_empty() {
            // A filtered list says how much of the whole it is showing.
            format!(
                " zdbview — {}/{} files  /{} ",
                view.len(),
                choices.len(),
                filter
            )
        } else {
            match (scanning, cache_age) {
                (Some(found), _) => format!(
                    " zdbview — {} files ({} recent, scanning… {} found) ",
                    view.len(),
                    recent,
                    found
                ),
                // Saved scans are reused, so say how old the rows are.
                (None, Some(age)) => format!(
                    " zdbview — {} files ({} recent, scan {} · r rescans) ",
                    view.len(),
                    recent,
                    age_label(age)
                ),
                (None, None) => format!(" zdbview — {} files ({} recent) ", view.len(), recent),
            }
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.accent))
                    .title(title),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, outer[0], &mut st);
        offset = st.offset();
    }

    // Bottom line: the filter prompt while typing, else the selected row's
    // format, else the keys.
    let help = match prompt {
        Some(q) => {
            Paragraph::new(format!("/{}_", q)).style(Style::default().fg(Color::Black).bg(t.accent))
        }
        None => {
            let detail = view
                .get(sel)
                .and_then(|&i| choices[i].format)
                .map(|fmt| format!("  ·  {}", fmt))
                .unwrap_or_default();
            // Kept short so the selected row's format still fits beside it on a
            // narrow terminal; `h` lists the rest.
            Paragraph::new(format!(
                "j/k move · / filter · Enter open · c scheme · h help · q quit{}",
                detail
            ))
            .style(Style::default().fg(Color::Black).bg(t.help_key))
        }
    };
    f.render_widget(help, outer[1]);
    offset
}

/// Everything the write monitor watches besides the open file: the recent-files
/// list and the saved scan. Shared by the app and the picker so both watch the
/// same set.
pub fn watch_targets() -> Vec<(PathBuf, Kind)> {
    let mut targets: Vec<(PathBuf, Kind)> = crate::mru::load()
        .into_iter()
        .map(|e| (e.path, e.kind))
        .collect();
    if let Some(c) = crate::scan::load_cache() {
        targets.extend(c.hits.into_iter().map(|h| (h.path, h.kind)));
    }
    targets.retain(|(p, _)| p.is_file());
    targets
}

/// The scheme to start from: `--theme` wins, else the saved preference (with any
/// custom palette), else the default.
pub fn resolve_theme(theme_override: Option<ThemeName>) -> Theme {
    let prefs = crate::prefs::load();
    match (theme_override, prefs.custom) {
        (Some(name), _) => Theme::from_name(name),
        (None, Some(c)) => Theme::from_palette(prefs.theme, c),
        (None, None) => Theme::from_name(prefs.theme),
    }
}

/// Whether `hay` passes `filter` (case-insensitive substring; empty passes all).
fn filter_passes(filter: &str, hay: &str) -> bool {
    filter.is_empty() || hay.to_lowercase().contains(&filter.to_lowercase())
}

/// Find the next index (wrapping) from `from` for which `pred` holds, scanning
/// `forward` or backward. Returns `None` if nothing matches.
/// `find_next` over a filtered list: `visible` holds the indices still listed, in
/// display order, and the walk wraps inside it. `from` is an index into the full
/// list — the cursor may sit on a row the filter has since hidden, in which case
/// the walk starts from where that row would be.
fn find_next_visible(
    visible: &[usize],
    from: usize,
    forward: bool,
    pred: impl Fn(usize) -> bool,
) -> Option<usize> {
    if visible.is_empty() {
        return None;
    }
    let pos = match visible.binary_search(&from) {
        Ok(p) => p,
        // Not listed: `p` is where it would sit, so stepping forward from `p - 1`
        // reaches it, and stepping back from `p` reaches the one before.
        Err(p) if forward => p.saturating_sub(1),
        Err(p) => p.min(visible.len() - 1),
    };
    find_next(visible.len(), pos, forward, |i| pred(visible[i])).map(|i| visible[i])
}

fn find_next(
    len: usize,
    from: usize,
    forward: bool,
    pred: impl Fn(usize) -> bool,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    for step in 1..=len {
        let i = if forward {
            (from + step) % len
        } else {
            (from + len - (step % len)) % len
        };
        if pred(i) {
            return Some(i);
        }
    }
    None
}

/// Find the byte offset of `needle` in `hay`, searching from just past `cur`
/// (or just before it, when not `forward`). Case-sensitive. `None` if absent.
fn find_bytes(hay: &[u8], needle: &[u8], cur: usize, forward: bool) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    if forward {
        let start = (cur + 1).min(last + 1);
        (start..=last).find(|&i| &hay[i..i + needle.len()] == needle)
    } else {
        let start = cur.min(last + 1);
        (0..start)
            .rev()
            .find(|&i| &hay[i..i + needle.len()] == needle)
    }
}

/// Whether the point `(col,row)` falls inside `r`.
fn hit(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x
        && col < r.x.saturating_add(r.width)
        && row >= r.y
        && row < r.y.saturating_add(r.height)
}

/// Parse a value-input string: a `0x…` prefix is decoded as hex bytes (spaces
/// Lowercase hex of a byte slice.
/// Whether a byte slice is mostly printable/UTF-8 text (heuristic for Auto
/// value rendering): valid UTF-8 and < 10% control bytes.
fn looks_textual(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if std::str::from_utf8(bytes).is_err() {
        return false;
    }
    let ctrl = bytes
        .iter()
        .filter(|&&b| b < 0x09 || (0x0e..0x20).contains(&b))
        .count();
    ctrl * 10 < bytes.len()
}

fn hex_string(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Make a filename-safe token from a table/base name.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Build the value-pane lines for `bytes` under `render`, starting at row
/// `scroll` (16 bytes/row), up to `height` rows.
fn value_lines(
    bytes: &[u8],
    render: ValueRender,
    scroll: usize,
    height: usize,
) -> Vec<Line<'static>> {
    if render == ValueRender::Disasm {
        return disasm_lines(bytes, scroll, height);
    }
    let as_text = match render {
        ValueRender::Text => true,
        ValueRender::Hex => false,
        ValueRender::Auto => looks_textual(bytes),
        ValueRender::Disasm => unreachable!(),
    };
    let mut lines = Vec::new();
    if as_text {
        let text = String::from_utf8_lossy(bytes);
        for line in text.lines().skip(scroll).take(height) {
            lines.push(Line::from(line.to_string()));
        }
    } else {
        for i in 0..height {
            let off = (scroll + i) * 16;
            if off >= bytes.len() {
                break;
            }
            lines.push(Line::from(hexedit::hex_dump_line(
                off,
                &bytes[off.min(bytes.len())..(off + 16).min(bytes.len())],
            )));
        }
    }
    lines
}

/// Disassemble the value as a fusevm::Chunk. Only functional with the `disasm`
/// feature; otherwise a one-line note.
#[cfg(feature = "disasm")]
fn disasm_lines(bytes: &[u8], scroll: usize, height: usize) -> Vec<Line<'static>> {
    match crate::disasm::disassemble(bytes) {
        Ok(all) => all
            .into_iter()
            .skip(scroll)
            .take(height)
            .map(Line::from)
            .collect(),
        Err(e) => vec![Line::from(format!("not a fusevm chunk: {e}"))],
    }
}

#[cfg(not(feature = "disasm"))]
fn disasm_lines(_bytes: &[u8], _scroll: usize, _height: usize) -> Vec<Line<'static>> {
    vec![Line::from(
        "rebuild with `--features disasm` for bytecode disassembly",
    )]
}

/// Truncate a display string to `max` chars, appending an ellipsis.
/// A saved scan's age, phrased for the picker title.
fn age_label(age: std::time::Duration) -> String {
    let secs = age.as_secs();
    match secs {
        0..=90 => "just now".to_string(),
        _ if secs < 3600 => format!("{}m old", secs / 60),
        _ if secs < 86_400 => format!("{}h old", secs / 3600),
        _ => format!("{}d old", secs / 86_400),
    }
}

/// Byte count in the largest unit that keeps it under four digits.
pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", n, UNITS[0])
    } else if size < 10.0 {
        format!("{:.1} {}", size, UNITS[unit])
    } else {
        format!("{:.0} {}", size, UNITS[unit])
    }
}

/// Sort-direction marker for a column header.
fn arrow(desc: bool) -> &'static str {
    if desc {
        "▼"
    } else {
        "▲"
    }
}

/// How often a statement from the editor checks whether the stop key was
/// pressed, in SQLite VM instructions. Small enough to feel immediate, large
/// enough that the check costs nothing on a statement that returns at once.
const SQL_STOP_CHECK_OPS: i32 = 5_000;

/// Rows a print takes. A printout is read by a person, so it is bounded — a
/// million-row table is an export, not a printout.
const PRINT_ROWS: i64 = 10_000;

/// Width of one grid cell, including the space between columns. The grid draws
/// as many as fit and scrolls the rest, so this decides how many that is.
const CELL_W: usize = 21;

/// Hand `path` to the desktop's opener — `open` on macOS, `xdg-open` elsewhere,
/// which is what every other terminal program uses for this.
fn open_externally(path: &std::path::Path) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(opener)
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

/// Pipe `text` to `lpr`, which is the print dialog a terminal program has.
fn print_via_lpr(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("lpr")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    // Wait, or a failing `lpr` would look like a successful print.
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("lpr exited with {status}")))
    }
}

/// A centered rect `w` cols wide and `h` rows tall inside `area`.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::{find_bytes, find_next, hit, App, Store};
    use crate::mru::Entry;
    use crate::overlay::HelpCtx;
    use crate::rkyv_inspect::RkyvStore;
    use crate::sqlite::SqliteStore;
    use crate::store::Kind;
    use crate::theme::{Theme, ThemeName};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    /// Merge hits into a list the way the picker does: the set of paths already
    /// listed comes from the rows themselves.
    fn merge(choices: &mut Vec<super::Choice>, hits: Vec<crate::scan::Hit>) {
        let mut listed = choices.iter().map(|c| super::canon(&c.path)).collect();
        super::merge_hits(choices, &mut listed, hits);
    }

    /// A unique scratch path per call — these tests run concurrently.
    fn scratch(ext: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "zdbview_app_{}_{}.{}",
            std::process::id(),
            seq,
            ext
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// An App over a throwaway binary file, on a fixed scheme so assertions
    /// don't depend on the developer's saved prefs.
    fn rkyv_app() -> App {
        let path = scratch("bin");
        std::fs::write(&path, b"zdbview overlay render test payload").unwrap();
        let store = Store::Rkyv(RkyvStore::open(&path).unwrap());
        let _ = std::fs::remove_file(&path);
        App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl))
    }

    /// An App over a SQLite table with `n` rows, for paging tests.
    fn sqlite_app_rows(n: usize) -> (App, std::path::PathBuf) {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE t (a TEXT, b TEXT)", []).unwrap();
        for i in 0..n {
            conn.execute("INSERT INTO t VALUES (?1, 'y')", [i.to_string()])
                .unwrap();
        }
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        (
            App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl)),
            path,
        )
    }

    /// An App over arbitrary binary content.
    fn rkyv_app_with(bytes: &[u8]) -> App {
        let path = scratch("bin");
        std::fs::write(&path, bytes).unwrap();
        let store = Store::Rkyv(RkyvStore::open(&path).unwrap());
        let _ = std::fs::remove_file(&path);
        App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl))
    }

    /// An App over a two-column SQLite table, for column-motion tests.
    fn sqlite_app() -> (App, std::path::PathBuf) {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE t (a TEXT, b TEXT, c TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO t VALUES ('x', 'y', 'z')", [])
            .unwrap();
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        (
            App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl)),
            path,
        )
    }

    /// Render one frame and flatten the buffer into per-row strings.
    fn frame_rows(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn contains(rows: &[String], needle: &str) -> bool {
        rows.iter().any(|r| r.contains(needle))
    }

    fn press(app: &mut App, c: char) {
        app.on_key(KeyEvent::from(KeyCode::Char(c)));
    }

    /// Wait for the query threads to go idle, the way the event loop does on its
    /// tick. Pages, counts and searches all arrive this way.
    fn await_queries(app: &mut App) {
        for _ in 0..5000 {
            app.poll_query();
            let busy = app
                .engine
                .as_ref()
                .is_some_and(|e| e.page_inflight() || e.count_inflight() || e.searching());
            if !busy {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("a query never finished");
    }

    /// Wait for a background statistics pass, the way the event loop does.
    fn await_stats(app: &mut App) {
        for _ in 0..2000 {
            app.poll_stats();
            if app.stats_pending.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("the statistics pass never finished");
    }

    /// The dot-commands, which is what makes the editor a stand-in for the shell:
    /// output modes, redirects, schema queries, import, scripts and index advice.
    #[test]
    fn the_sql_editor_runs_dot_commands() {
        use crate::sqledit::Entry;
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (a TEXT, b INTEGER);
             CREATE INDEX t_a ON t(a);
             INSERT INTO t VALUES ('x', 1), ('y', 2);",
        )
        .unwrap();
        drop(conn);
        let mut app = App::with_theme(
            Store::Sqlite(SqliteStore::open(&path).unwrap()),
            Theme::from_name(ThemeName::NeonSprawl),
        );
        press(&mut app, ':');
        let note = |app: &App| -> Vec<String> {
            app.sql
                .as_ref()
                .unwrap()
                .transcript()
                .iter()
                .rev()
                .find_map(|e| match e {
                    Entry::Note(lines) => Some(lines.clone()),
                    _ => None,
                })
                .expect("a note")
        };

        app.execute_sql(".tables");
        assert_eq!(note(&app), vec!["t".to_string()]);

        app.execute_sql(".indexes t");
        assert!(note(&app)[0].starts_with("t_a  (a)"), "{:?}", note(&app));

        app.execute_sql(".help");
        assert!(
            note(&app).iter().any(|l| l.starts_with(".expert")),
            "the help lists every command it implements"
        );

        // `.mode` changes how a result set is rendered: no grid, formatted text.
        app.execute_sql(".mode csv");
        app.execute_sql("SELECT a, b FROM t ORDER BY a");
        assert_eq!(note(&app), vec!["a,b", "x,1", "y,2"]);
        app.execute_sql(".headers off");
        app.execute_sql("SELECT a, b FROM t ORDER BY a");
        assert_eq!(
            note(&app),
            vec!["x,1", "y,2"],
            "headers off drops the names"
        );
        app.execute_sql(".mode list");
        app.execute_sql("SELECT a FROM t");
        assert!(
            matches!(
                app.sql.as_ref().unwrap().transcript().iter().rev().nth(1),
                Some(Entry::Rows { .. })
            ),
            "list mode is the grid again"
        );

        // `.once` writes the next result only.
        let out = scratch("csv");
        app.execute_sql(".mode csv");
        app.execute_sql(".headers on"); // still off from the toggle above
        app.execute_sql(&format!(".once {}", out.display()));
        app.execute_sql("SELECT a FROM t ORDER BY a");
        assert!(note(&app)[0].contains("wrote 2 rows"), "{:?}", note(&app));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "a\nx\ny\n");
        assert!(
            app.sql_out.is_none(),
            "a once redirect ends after one result"
        );

        // `.output` collects every result until it is reset, as the shell does — the
        // second statement must not erase the first.
        let all = scratch("all.csv");
        app.execute_sql(&format!(".output {}", all.display()));
        app.execute_sql("SELECT a FROM t WHERE a = 'x'");
        app.execute_sql("SELECT a FROM t WHERE a = 'y'");
        let collected = std::fs::read_to_string(&all).unwrap();
        assert_eq!(
            collected, "a\nx\na\ny\n",
            "both results are in the file, in order"
        );
        // Pointing it at the same file again starts fresh rather than appending to
        // what was there.
        app.execute_sql(&format!(".output {}", all.display()));
        app.execute_sql("SELECT a FROM t WHERE a = 'x'");
        assert_eq!(std::fs::read_to_string(&all).unwrap(), "a\nx\n");
        app.execute_sql(".output");
        assert!(
            app.sql_out.is_none(),
            "bare .output goes back to the transcript"
        );
        let _ = std::fs::remove_file(&all);

        // `.import` shares the reader with --import; `.read` runs a script.
        let csv = scratch("in.csv");
        std::fs::write(&csv, "a,b\nz,3\n").unwrap();
        app.execute_sql(&format!(".import {} t", csv.display()));
        assert_eq!(note(&app), vec!["imported 1 rows into t".to_string()]);
        let script = scratch("sql");
        std::fs::write(
            &script,
            "UPDATE t SET b = 9 WHERE a = 'z';\nSELECT count(*) FROM t;",
        )
        .unwrap();
        app.execute_sql(&format!(".read {}", script.display()));
        assert!(
            note(&app).iter().any(|l| l.contains("ran 2 statements")),
            "{:?}",
            note(&app)
        );

        // `.expert` asks about the statement the transcript last ran, so this one
        // goes through the editor: typing and Enter is what records it.
        app.execute_sql(".mode list");
        for c in "SELECT a FROM t WHERE b = 1".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.execute_sql(".expert");
        let advice = note(&app);
        assert!(
            advice[0].contains("full scan") && advice[0].contains("CREATE INDEX"),
            "{advice:?}"
        );
        assert!(
            advice[0].contains("\"b\""),
            "the column compared: {advice:?}"
        );

        // An unknown command says so instead of reaching SQLite.
        app.execute_sql(".nope");
        assert!(
            note(&app)[0].contains("unknown command"),
            "{:?}",
            note(&app)
        );
        for f in [path, out, csv, script] {
            let _ = std::fs::remove_file(f);
        }
    }

    /// The monitor's two novel views, driven end to end: `t` swaps to the per-table
    /// breakdown, and `F` opens the log walker on the selected database.
    #[test]
    fn the_monitor_opens_the_table_pane_and_the_log_walker() {
        let path = scratch("db");
        for suffix in ["-wal", "-shm"] {
            let mut n = path.as_os_str().to_os_string();
            n.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(n));
        }
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute_batch("CREATE TABLE t (v TEXT)").unwrap();
        for i in 0..120 {
            conn.execute("INSERT INTO t VALUES (?1)", [format!("row {i} padded out")])
                .unwrap();
        }

        let mut app = App::with_theme(
            Store::Sqlite(SqliteStore::open(&path).unwrap()),
            Theme::from_name(ThemeName::NeonSprawl),
        );
        // The monitor watches every file zdbview knows of, which on a real machine
        // means whatever else is writing right now — this test is about the panes,
        // so it watches only the database it just wrote.
        press(&mut app, 'w'); // the write monitor
        assert_eq!(app.screen, super::Screen::Top);
        app.top = Some(crate::monitor::Monitor::new([(path.clone(), Kind::Sqlite)]));
        // Sample twice so the second tick has frames to attribute.
        if let Some(m) = app.top.as_mut() {
            m.watcher.interval = std::time::Duration::from_millis(0);
            m.tick();
            m.tick();
        }
        press(&mut app, 't');
        assert_eq!(
            app.top.as_ref().unwrap().pane,
            crate::monitor::Pane::Tables,
            "t swaps the bottom pane"
        );
        let rows = frame_rows(&mut app, 120, 30);
        assert!(contains(&rows, "tables —"), "{:?}", rows.last());
        assert!(
            contains(&rows, " t ") || contains(&rows, "sqlite_schema"),
            "the pane names what was written: {rows:?}"
        );

        // `F` walks the log: frames on the left, the selected frame's rows right.
        press(&mut app, 'F');
        assert_eq!(app.screen, super::Screen::Frames);
        let walk = app.walk.as_ref().expect("a log walker");
        assert!(!walk.frames.is_empty());
        assert_eq!(app.help_ctx(), HelpCtx::Frames);
        let rows = frame_rows(&mut app, 120, 30);
        assert!(contains(&rows, "what this frame wrote"), "{:?}", rows[0]);
        assert!(
            contains(&rows, "commits"),
            "the frame list is titled with its commit count"
        );

        // Esc returns to the monitor rather than to the grid.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.screen, super::Screen::Top);
        assert!(app.walk.is_none());
        drop(conn);
        for suffix in ["", "-wal", "-shm"] {
            let mut n = path.as_os_str().to_os_string();
            n.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(n));
        }
    }

    /// `F` follows the foreign key under the cursor into the parent table and puts
    /// the cursor on the row it references.
    #[test]
    fn f_follows_a_foreign_key_to_its_parent_row() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            // Enforcement off so the fixture can hold a dangling key, which is
            // exactly the case `F` has to report rather than follow.
            "PRAGMA foreign_keys=OFF;
             CREATE TABLE author (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE book (title TEXT, author_id INTEGER REFERENCES author(id));
             INSERT INTO author VALUES (1, 'first'), (2, 'second'), (3, 'third');
             INSERT INTO book VALUES ('a book', 3), ('orphan', 99), ('unknown', NULL);",
        )
        .unwrap();
        drop(conn);
        let mut app = App::with_theme(
            Store::Sqlite(SqliteStore::open(&path).unwrap()),
            Theme::from_name(ThemeName::NeonSprawl),
        );
        // `book` is the second table; focus its grid and put the cursor on the key.
        let book = app
            .sqlite()
            .unwrap()
            .tables
            .iter()
            .position(|t| t == "book")
            .unwrap();
        app.select_table(book);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        app.on_key(KeyEvent::from(KeyCode::Right)); // author_id

        press(&mut app, 'F');
        assert_eq!(
            app.current_table().as_deref(),
            Some("author"),
            "it jumps to the parent table"
        );
        let row = &app.rows.as_ref().unwrap().rows[app.row_idx];
        assert_eq!(row[1], "third", "and lands on the referenced row");
        assert_eq!(
            app.rows.as_ref().unwrap().columns[app.col_idx],
            "id",
            "with the cursor on the key column"
        );

        // A key with no matching parent row says so and stays put. (Focus is
        // already on the grid here, so Tab would move it away.)
        app.select_table(book);
        app.on_key(KeyEvent::from(KeyCode::Right));
        app.on_key(KeyEvent::from(KeyCode::Down)); // the orphan row
        press(&mut app, 'F');
        assert_eq!(app.current_table().as_deref(), Some("book"));
        assert!(app.status.contains("no author row"), "got {:?}", app.status);

        // A NULL key, and a column that is not a key at all.
        app.on_key(KeyEvent::from(KeyCode::Down)); // the NULL row
        press(&mut app, 'F');
        assert!(app.status.contains("NULL"), "got {:?}", app.status);
        app.on_key(KeyEvent::from(KeyCode::Left)); // title
        press(&mut app, 'F');
        assert!(
            app.status.contains("not a foreign key"),
            "got {:?}",
            app.status
        );
        let _ = std::fs::remove_file(path);
    }

    /// `Y` puts the row on the clipboard as an `INSERT`. With no tty the copy
    /// cannot land, and the toast has to say so rather than claim success.
    #[test]
    fn y_copies_the_row_as_an_insert_statement() {
        let (mut app, path) = sqlite_app();
        app.on_key(KeyEvent::from(KeyCode::Tab)); // focus the row grid
        press(&mut app, 'Y');
        assert!(
            app.status.contains("copied INSERT") || app.status.contains("clipboard unavailable"),
            "got {:?}",
            app.status
        );
        // The statement itself is built by the same writer the dump uses.
        let view = app.rows.as_ref().unwrap();
        let sql = crate::export::insert_statement("t", &view.columns, &view.rows[0]);
        assert_eq!(
            sql,
            r#"INSERT INTO "t" ("a", "b", "c") VALUES ('x', 'y', 'z');"#
        );
        let _ = std::fs::remove_file(path);
    }

    /// The detail screen shows the whole value, so it is where you edit it: `e`
    /// opens the editor there, the arrows pick the field, and the pane shows the
    /// new bytes without a trip back to the grid.
    #[test]
    fn the_detail_screen_edits_the_field_it_is_showing() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (a TEXT, b TEXT)")
            .unwrap();
        conn.execute("INSERT INTO t VALUES ('one', 'two')", [])
            .unwrap();
        drop(conn);
        let mut app = App::with_theme(
            Store::Sqlite(SqliteStore::open(&path).unwrap()),
            Theme::from_name(ThemeName::NeonSprawl),
        );
        app.on_key(KeyEvent::from(KeyCode::Tab)); // focus the row grid
        app.on_key(KeyEvent::from(KeyCode::Enter)); // detail
        assert_eq!(app.screen, super::Screen::Detail);
        assert_eq!(app.detail_value, b"one");

        // Right moves to the next field and reloads what the value pane shows.
        app.on_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(app.col_idx, 1);
        assert_eq!(app.detail_value, b"two");

        // `e` edits that field in place, and the screen stays where it was.
        press(&mut app, 'e');
        assert!(matches!(app.mode, super::Mode::EditCell(_)));
        assert_eq!(app.screen, super::Screen::Detail);
        // The editor opens seeded with the current value, so clear it first.
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "TWO".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            app.detail_value, b"TWO",
            "the value pane must show what was just written"
        );
        // An edit is buffered until it is written, so nothing reaches another
        // connection before this.
        app.write_changes();
        let store = SqliteStore::open(&path).unwrap();
        let view = store.rows(&crate::sqlite::PageQuery::all("t", 10)).unwrap();
        assert_eq!(view.rows[0], vec!["one".to_string(), "TWO".to_string()]);
        let _ = std::fs::remove_file(path);
    }

    /// `E` opens any cell as bytes, which is the only way to put binary into a cell
    /// that is not already a blob — `e` alone would edit it as text.
    #[test]
    fn shift_e_turns_a_text_or_null_cell_into_bytes() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (txt TEXT, empty BLOB);
             INSERT INTO t VALUES ('hi', NULL);",
        )
        .unwrap();
        drop(conn);
        let mut app = App::with_theme(
            Store::Sqlite(SqliteStore::open(&path).unwrap()),
            Theme::from_name(ThemeName::NeonSprawl),
        );
        app.on_key(KeyEvent::from(KeyCode::Tab)); // the row grid

        // `e` on a text cell still edits text.
        press(&mut app, 'e');
        assert!(matches!(app.mode, super::Mode::EditCell(_)));
        app.on_key(KeyEvent::from(KeyCode::Esc));

        // `E` on the same cell opens its bytes instead.
        press(&mut app, 'E');
        assert_eq!(app.screen, super::Screen::HexEdit);
        assert_eq!(
            app.hex.as_ref().unwrap().bytes,
            b"hi",
            "a string starts as its own bytes"
        );
        press(&mut app, 'i');
        press(&mut app, '4');
        press(&mut app, '1');
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        app.write_changes();
        let store = SqliteStore::open(&path).unwrap();
        let key = crate::sqlite::RowKey::Rowid(1);
        assert_eq!(store.cell_bytes_keyed("t", &key, "txt").unwrap(), b"Ai");
        assert!(
            store.cell_is_blob_keyed("t", &key, "txt").unwrap(),
            "it is a blob now, not a string"
        );
        app.on_key(KeyEvent::from(KeyCode::Esc));
        press(&mut app, 'q');

        // And a NULL cell starts empty, so bytes can be authored from nothing.
        app.on_key(KeyEvent::from(KeyCode::Right));
        press(&mut app, 'E');
        assert_eq!(app.screen, super::Screen::HexEdit);
        assert!(
            app.hex.as_ref().unwrap().bytes.is_empty(),
            "NULL has no bytes"
        );
        press(&mut app, 'o'); // insert a byte to have something to write
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        app.write_changes();
        let store = SqliteStore::open(&path).unwrap();
        assert!(
            store.cell_is_blob_keyed("t", &key, "empty").unwrap(),
            "a NULL became a blob"
        );
        let _ = std::fs::remove_file(path);
    }

    /// A blob field edited from the detail screen goes through the hex editor and
    /// comes back to the detail screen, not to the grid.
    #[test]
    fn a_blob_edited_from_the_detail_screen_returns_there() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (raw BLOB)").unwrap();
        conn.execute("INSERT INTO t VALUES (?1)", [&[0x01u8, 0x02][..]])
            .unwrap();
        drop(conn);
        let mut app = App::with_theme(
            Store::Sqlite(SqliteStore::open(&path).unwrap()),
            Theme::from_name(ThemeName::NeonSprawl),
        );
        app.on_key(KeyEvent::from(KeyCode::Tab));
        app.on_key(KeyEvent::from(KeyCode::Enter)); // detail
        press(&mut app, 'e');
        assert_eq!(app.screen, super::Screen::HexEdit);

        press(&mut app, 'i');
        press(&mut app, 'f');
        press(&mut app, 'f');
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::from(KeyCode::Esc)); // leave EDIT mode
        press(&mut app, 'q'); // close the editor
        assert_eq!(
            app.screen,
            super::Screen::Detail,
            "closing returns to where it opened from"
        );
        assert_eq!(app.detail_value, [0xff, 0x02], "with the new bytes");
        let _ = std::fs::remove_file(path);
    }

    /// Every rkyv edit path needs the whole archive while `self` is borrowed
    /// mutably, which used to mean copying it — 382 MB per operation on a real
    /// shard. The bytes are shared now, so handing them out must not copy.
    #[test]
    fn handing_out_the_archive_bytes_does_not_copy_them() {
        let path = scratch("bin");
        std::fs::write(&path, b"zdbview archive bytes, shared not copied").unwrap();
        let store = RkyvStore::open(&path).unwrap();
        let original = std::sync::Arc::clone(&store.bytes);
        let app = App::with_theme(Store::Rkyv(store), Theme::from_name(ThemeName::NeonSprawl));

        // What every edit path and the background decoder do to get the bytes.
        let handed = match &app.store {
            Store::Rkyv(r) => r.bytes.clone(),
            _ => unreachable!(),
        };
        assert!(
            std::sync::Arc::ptr_eq(&original, &handed),
            "the same allocation must come back, not a copy of it"
        );
        assert_eq!(&*handed, b"zdbview archive bytes, shared not copied");
        let _ = std::fs::remove_file(path);
    }

    /// With a filter active, `n` must step through the listed rows only — in the
    /// grid and in the table pane both.
    #[test]
    fn search_steps_through_filtered_lists_only() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE keep_a (v TEXT); CREATE TABLE drop_b (v TEXT);
             CREATE TABLE keep_c (v TEXT);
             INSERT INTO keep_a VALUES ('x');",
        )
        .unwrap();
        drop(conn);
        let mut app = App::with_theme(
            Store::Sqlite(SqliteStore::open(&path).unwrap()),
            Theme::from_name(ThemeName::NeonSprawl),
        );

        // Table pane: `/keep` hides drop_b, so searching for a name every table
        // matches must still skip it.
        app.set_filter("keep".to_string());
        app.search = "_".to_string();
        app.table_idx = 0;
        app.search_next(true);
        assert_eq!(
            app.sqlite().unwrap().tables[app.table_idx],
            "keep_c",
            "n skipped the filtered-out table"
        );
        app.search_next(true);
        assert_eq!(
            app.sqlite().unwrap().tables[app.table_idx],
            "keep_a",
            "and wraps inside the filtered list"
        );
        let _ = std::fs::remove_file(path);
    }

    /// `n` in the row grid scans the whole table and then counts to find out which
    /// page the match is on — two full scans on a large table, so it runs on the
    /// query thread and lands through the poll. Pressing it must still scroll to
    /// the match and select it.
    #[test]
    fn n_scrolls_the_grid_to_a_match_on_another_page() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (v TEXT)").unwrap();
        {
            let tx = conn.unchecked_transaction().unwrap();
            let mut stmt = tx.prepare("INSERT INTO t (v) VALUES (?1)").unwrap();
            // One needle, well past the first page of 500.
            for i in 0..1200 {
                stmt.execute(rusqlite::params![if i == 900 {
                    "needle".to_string()
                } else {
                    format!("hay-{i}")
                }])
                .unwrap();
            }
            drop(stmt);
            tx.commit().unwrap();
        }
        drop(conn);
        let mut app = App::with_theme(
            Store::Sqlite(SqliteStore::open(&path).unwrap()),
            Theme::from_name(ThemeName::NeonSprawl),
        );
        app.on_key(KeyEvent::from(KeyCode::Tab)); // focus the row grid
        await_queries(&mut app);
        assert_eq!(app.page_offset, 0, "starts on the first page");

        app.search = "needle".into();
        app.search_next(true);
        await_queries(&mut app);

        // The needle is the 901st row, so it sits 400 rows into the page at 500.
        assert_eq!(app.page_offset, 500, "the grid moved to the match's page");
        assert_eq!(app.row_idx, 400, "and selected the match");
        let row = &app.rows.as_ref().unwrap().rows[app.row_idx];
        assert_eq!(row[0], "needle", "the selected row is the match");
        let _ = std::fs::remove_file(path);
    }

    /// A filter re-queries on every keystroke, so an intermediate result must
    /// never be what ends up on screen.
    #[test]
    fn typing_a_filter_leaves_the_grid_showing_the_final_pattern() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (v TEXT);
             INSERT INTO t VALUES ('alpha'), ('alpine'), ('beta'), ('gamma');",
        )
        .unwrap();
        drop(conn);
        let mut app = App::with_theme(
            Store::Sqlite(SqliteStore::open(&path).unwrap()),
            Theme::from_name(ThemeName::NeonSprawl),
        );
        app.on_key(KeyEvent::from(KeyCode::Tab));
        await_queries(&mut app);

        // Type `alpi` one key at a time, as the prompt does.
        for pattern in ["a", "al", "alp", "alpi"] {
            app.set_filter(pattern.to_string());
        }
        await_queries(&mut app);
        let rows = &app.rows.as_ref().unwrap().rows;
        assert_eq!(rows.len(), 1, "only `alpine` matches `alpi`: {rows:?}");
        assert_eq!(rows[0][0], "alpine");
        let _ = std::fs::remove_file(path);
    }

    /// `A` describes the table's columns, Enter drills into one column's
    /// frequency, and Esc unwinds one step at a time.
    #[test]
    fn stats_screen_describes_columns_and_counts_values() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (tag TEXT, qty INTEGER);
             INSERT INTO t VALUES ('a', 1), ('a', 2), ('b', NULL);",
        )
        .unwrap();
        drop(conn);
        let mut app = App::with_theme(
            Store::Sqlite(SqliteStore::open(&path).unwrap()),
            Theme::from_name(ThemeName::NeonSprawl),
        );
        app.on_key(KeyEvent::from(KeyCode::Tab)); // focus the row grid

        press(&mut app, 'A');
        assert_eq!(app.screen, super::Screen::Stats);
        // The pass runs off-thread, so the screen opens saying so and Enter does
        // nothing until the columns land.
        assert!(app.stats_pending.is_some());
        let rows = frame_rows(&mut app, 110, 20);
        assert!(contains(&rows, "analyzing"), "{:?}", rows[0]);
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.stats_freq.is_none(), "nothing to drill into yet");

        await_stats(&mut app);
        assert_eq!(app.stats.len(), 2, "one row per column");
        let rows = frame_rows(&mut app, 110, 20);
        assert!(contains(&rows, "distinct"), "{rows:?}");
        assert!(contains(&rows, "columns of t"), "{:?}", rows[0]);

        // The frequency table is a step deeper: Enter opens it, Esc closes it and
        // only the second Esc leaves the screen.
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let (col, freq) = app.stats_freq.as_ref().expect("a frequency table");
        assert_eq!(col, "tag");
        assert_eq!(freq[0], ("a".to_string(), 2), "the common value leads");
        let rows = frame_rows(&mut app, 110, 20);
        assert!(contains(&rows, "most common values"), "{rows:?}");
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.stats_freq.is_none());
        assert_eq!(app.screen, super::Screen::Stats, "still on the screen");
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.screen, super::Screen::Main);

        // Moving the selection drops a stale frequency table for the old column.
        press(&mut app, 'A');
        await_stats(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Enter));
        press(&mut app, 'j');
        assert!(
            app.stats_freq.is_none(),
            "the table belonged to the old column"
        );
        assert_eq!(app.stats_idx, 1);
        let _ = std::fs::remove_file(path);
    }

    /// A blob cell cannot be edited as text — `e` must open the hex editor over
    /// its bytes and `^s` must write bytes back, not a string.
    #[test]
    fn a_blob_cell_edits_in_the_hex_editor() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (raw BLOB, txt TEXT)")
            .unwrap();
        conn.execute(
            "INSERT INTO t VALUES (?1, 'plain')",
            [&[0x00u8, 0xffu8][..]],
        )
        .unwrap();
        drop(conn);
        let mut app = App::with_theme(
            Store::Sqlite(SqliteStore::open(&path).unwrap()),
            Theme::from_name(ThemeName::NeonSprawl),
        );
        app.on_key(KeyEvent::from(KeyCode::Tab)); // focus the row grid

        press(&mut app, 'e');
        assert_eq!(
            app.screen,
            super::Screen::HexEdit,
            "blob opens the hex editor"
        );
        assert_eq!(app.hex.as_ref().unwrap().bytes, [0x00, 0xff]);

        // Set the first byte to 0x41 and save.
        press(&mut app, 'i');
        press(&mut app, '4');
        press(&mut app, '1');
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        app.write_changes();
        let store = SqliteStore::open(&path).unwrap();
        assert_eq!(
            store
                .cell_bytes_keyed("t", &crate::sqlite::RowKey::Rowid(1), "raw")
                .unwrap(),
            [0x41, 0xff],
            "the edited byte reached the database"
        );
        assert!(
            store
                .cell_is_blob_keyed("t", &crate::sqlite::RowKey::Rowid(1), "raw")
                .unwrap(),
            "still a blob, not text"
        );

        // A text cell in the same row still uses the line editor.
        app.on_key(KeyEvent::from(KeyCode::Esc)); // leave EDIT mode
        app.on_key(KeyEvent::from(KeyCode::Char('q')));
        app.on_key(KeyEvent::from(KeyCode::Right));
        press(&mut app, 'e');
        assert_eq!(app.screen, super::Screen::Main);
        assert!(
            matches!(app.mode, super::Mode::EditCell(_)),
            "text edits inline"
        );
        let _ = std::fs::remove_file(path);
    }

    /// `D` opens the database screen, its checks run on demand, and `q` still
    /// quits from there like it does everywhere else.
    #[test]
    fn dbinfo_screen_reports_and_checks_on_demand() {
        let (mut app, path) = sqlite_app_rows(3);
        press(&mut app, 'D');
        assert_eq!(app.screen, super::Screen::DbInfo);
        assert!(
            app.dbinfo.iter().any(|(k, _)| k == "page size"),
            "pragmas are read when the screen opens: {:?}",
            app.dbinfo
        );
        assert!(
            app.dbinfo_checks.is_empty(),
            "the slow checks wait for a keypress"
        );
        let rows = frame_rows(&mut app, 100, 24);
        assert!(contains(&rows, "journal mode"), "{rows:?}");
        assert!(contains(&rows, "database —"), "{:?}", rows[0]);

        // Both checks pass on a file this small, and the lint has nothing to say
        // about a table with no foreign keys.
        press(&mut app, 'i');
        assert_eq!(app.dbinfo_checks, vec!["integrity check: ok".to_string()]);
        press(&mut app, 'Q');
        assert_eq!(app.dbinfo_checks, vec!["quick check: ok".to_string()]);
        press(&mut app, 'f');
        assert_eq!(
            app.dbinfo_checks,
            vec!["foreign-key lint: every foreign key has an index".to_string()]
        );
        let rows = frame_rows(&mut app, 100, 24);
        assert!(contains(&rows, "every foreign key"), "{rows:?}");

        // The help overlay follows the screen, and `D` again goes back.
        press(&mut app, 'h');
        assert_eq!(app.help_ctx(), HelpCtx::DbInfo);
        press(&mut app, 'h');
        press(&mut app, 'D');
        assert_eq!(app.screen, super::Screen::Main);

        // The maintenance statements report what they did and refresh the pragmas
        // behind them, since all three rewrite the file.
        press(&mut app, 'D');
        press(&mut app, 'z');
        assert_eq!(app.dbinfo_checks.len(), 1);
        assert!(
            app.dbinfo_checks[0].starts_with("ANALYZE: done"),
            "got {:?}",
            app.dbinfo_checks[0]
        );
        press(&mut app, 'v');
        assert!(
            app.dbinfo_checks[0].starts_with("VACUUM: done"),
            "got {:?}",
            app.dbinfo_checks[0]
        );
        press(&mut app, 'r');
        assert!(
            app.dbinfo_checks[0].starts_with("REINDEX: done"),
            "got {:?}",
            app.dbinfo_checks[0]
        );
        assert!(
            app.dbinfo.iter().any(|(k, _)| k == "page count"),
            "the pragmas are re-read after a rewrite"
        );

        press(&mut app, 'q');
        assert!(app.quit, "q quits from the database screen too");
        let _ = std::fs::remove_file(path);
    }

    /// `O` runs `PRAGMA optimize` and reports what it did, and the report
    /// scrolls to both ends: `G` reaches the bottom, which is where the check
    /// output lands, and `g` comes back. The report is longer than a short
    /// terminal, so without a bottom key the newest lines could only be reached
    /// one `j` at a time.
    #[test]
    fn the_database_report_optimizes_and_scrolls_to_both_ends() {
        let (mut app, path) = sqlite_app_rows(3);
        press(&mut app, 'D');
        press(&mut app, 'O');
        assert!(
            app.dbinfo_checks[0].starts_with("PRAGMA optimize"),
            "got {:?}",
            app.dbinfo_checks
        );

        // A terminal shorter than the report, so there is something to scroll.
        let rows = frame_rows(&mut app, 100, 12);
        assert!(contains(&rows, "database —"), "{:?}", rows[0]);
        assert!(
            app.dbinfo_max_scroll > 0,
            "the report must overflow 12 rows for this test to mean anything"
        );

        press(&mut app, 'G');
        assert_eq!(
            app.dbinfo_scroll, app.dbinfo_max_scroll,
            "G goes to the bottom of the report"
        );
        let rows = frame_rows(&mut app, 100, 12);
        assert!(
            contains(&rows, "PRAGMA optimize"),
            "the check output sits at the bottom and must be on screen: {rows:?}"
        );

        press(&mut app, 'g');
        assert_eq!(app.dbinfo_scroll, 0, "g goes back to the top");
        let rows = frame_rows(&mut app, 100, 12);
        assert!(
            !contains(&rows, "PRAGMA optimize"),
            "the top of the report is the pragmas, not the checks: {rows:?}"
        );

        // Scrolling past the end is clamped by the render, not left dangling.
        for _ in 0..200 {
            press(&mut app, 'j');
        }
        let _ = frame_rows(&mut app, 100, 12);
        assert_eq!(app.dbinfo_scroll, app.dbinfo_max_scroll);
        let _ = std::fs::remove_file(path);
    }

    /// The overlay openers must work from the app's screens, and keys the
    /// overlay layer doesn't own must still reach the app.
    #[test]
    fn overlay_keys_route_through_the_app() {
        let mut app = rkyv_app();
        press(&mut app, 'h');
        assert!(app.ov.help, "h must open help");
        press(&mut app, 'j');
        assert!(!app.ov.help, "any key closes help");

        press(&mut app, 'c');
        assert!(app.ov.chooser, "c must open the scheme chooser");
        press(&mut app, 'c');
        assert!(!app.ov.chooser);

        press(&mut app, 'C');
        assert!(app.ov.editor, "C must open the palette editor");
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.ov.editor);

        // `e` belongs to the data screens, not the overlays.
        press(&mut app, 'e');
        assert!(!app.ov.active(), "e must not open an overlay");
    }

    /// `h` is the help key, so it must not double as a column motion; the
    /// arrows own that.
    #[test]
    fn columns_move_with_arrows_and_h_opens_help() {
        let (mut app, path) = sqlite_app();
        app.on_key(KeyEvent::from(KeyCode::Tab)); // focus the row grid
        app.on_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(app.col_idx, 1, "Right must move a column");
        app.on_key(KeyEvent::from(KeyCode::Left));
        assert_eq!(app.col_idx, 0, "Left must move back");

        app.on_key(KeyEvent::from(KeyCode::Right));
        press(&mut app, 'h');
        assert_eq!(app.col_idx, 1, "h must not move the column");
        assert!(app.ov.help, "h must open help instead");
        let _ = std::fs::remove_file(&path);
    }

    /// `s` cycles the row grid's sort on the cursor column (ascending →
    /// descending → off), reloads the page, and marks the column in its header.
    #[test]
    fn s_cycles_the_sort_on_the_cursor_column() {
        let (mut app, path) = sqlite_app();
        app.on_key(KeyEvent::from(KeyCode::Tab)); // focus the row grid
        app.on_key(KeyEvent::from(KeyCode::Right)); // cursor on column `b`
        assert!(app.sort.is_none(), "no sort until asked for");

        press(&mut app, 's');
        let sort = app.sort.as_ref().expect("s must sort");
        assert_eq!(sort.column, "b");
        assert!(!sort.desc, "first press sorts ascending");
        let rows = frame_rows(&mut app, 100, 20);
        assert!(contains(&rows, "b ▲"), "ascending marker missing");
        assert!(contains(&rows, "sorted by b ascending"), "no sort toast");

        press(&mut app, 's');
        assert!(app.sort.as_ref().unwrap().desc, "second press flips it");
        let rows = frame_rows(&mut app, 100, 20);
        assert!(contains(&rows, "b ▼"), "descending marker missing");

        press(&mut app, 's');
        assert!(app.sort.is_none(), "third press clears the sort");

        // `>` walks the sort to the next column, keeping the direction.
        press(&mut app, '>');
        assert_eq!(app.sort.as_ref().unwrap().column, "b");
        press(&mut app, '>');
        assert_eq!(app.sort.as_ref().unwrap().column, "c");
        press(&mut app, '<');
        assert_eq!(app.sort.as_ref().unwrap().column, "b");

        // Selecting another table drops a sort that named its columns.
        app.on_key(KeyEvent::from(KeyCode::Tab));
        app.select_table(0);
        assert!(app.sort.is_none(), "sort must not survive a table switch");
        let _ = std::fs::remove_file(&path);
    }

    /// An App over a real recognized shard on disk, so record edits exercise the
    /// actual rkyv write-back.
    fn script_shard_app() -> (App, std::path::PathBuf) {
        let path = scratch("rkyv");
        std::fs::write(
            &path,
            crate::formats::test_script_shard_bytes("/tmp/a.sh", b"old"),
        )
        .unwrap();
        let store = Store::Rkyv(RkyvStore::open(&path).unwrap());
        (
            App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl)),
            path,
        )
    }

    /// `e` on a record opens the ported hex editor pre-filled with the record's
    /// current bytes — the point of the port: no retyping the whole value.
    #[test]
    fn e_opens_the_hex_editor_on_the_current_value() {
        let (mut app, path) = script_shard_app();
        assert_eq!(app.screen, super::Screen::Main);
        press(&mut app, 'e');
        assert_eq!(app.screen, super::Screen::HexEdit);
        let ed = app.hex.as_ref().expect("editor open");
        assert_eq!(ed.bytes, b"old", "pre-filled with the record's value");
        assert_eq!(ed.label, "/tmp/a.sh");

        let rows = frame_rows(&mut app, 90, 16);
        assert!(contains(&rows, "hex editor"), "editor not drawn");
        assert!(contains(&rows, "6f 6c 64"), "hex cells for \"old\"");
        assert!(contains(&rows, "|old"), "ascii gutter");
        let _ = std::fs::remove_file(&path);
    }

    /// Edits written with `^s` go through the real archive write-back and are
    /// visible in the reloaded records.
    #[test]
    fn hex_editor_saves_edited_bytes_back_into_the_archive() {
        let (mut app, path) = script_shard_app();
        press(&mut app, 'e');
        // EDIT mode, ascii column, overwrite "old" with "new".
        press(&mut app, 'i');
        app.on_key(KeyEvent::from(KeyCode::Tab));
        for c in "new".chars() {
            press(&mut app, c);
        }
        assert_eq!(app.hex.as_ref().unwrap().bytes, b"new");
        assert!(app.hex.as_ref().unwrap().dirty);

        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(!app.hex.as_ref().unwrap().dirty, "save clears dirty");
        assert_eq!(
            app.decoded.as_ref().unwrap().records[0].value,
            b"new".to_vec(),
            "the reloaded record must carry the edited bytes"
        );
        // And the file on disk really changed.
        let on_disk = std::fs::read(&path).unwrap();
        assert!(
            crate::formats::try_decode(&on_disk).unwrap().records[0].value == b"new".to_vec(),
            "write-back must reach the file"
        );

        // In EDIT mode letters are data, so leave it first; then `q` closes on a
        // single press because the buffer is clean.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        press(&mut app, 'q');
        assert_eq!(app.screen, super::Screen::Main);
        assert!(app.hex.is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// Inside the editor the overlay openers must not steal `h` or `c` — they are
    /// motions and hex digits there.
    #[test]
    fn hex_editor_keeps_h_and_c_for_itself() {
        let (mut app, path) = script_shard_app();
        press(&mut app, 'e');
        press(&mut app, 'l');
        press(&mut app, 'h'); // motion, not help
        assert!(!app.ov.help, "h must not open help inside the editor");
        assert_eq!(app.screen, super::Screen::HexEdit);

        press(&mut app, 'i');
        press(&mut app, 'c'); // a hex digit, not the chooser
        assert!(
            !app.ov.chooser,
            "c must not open the chooser inside the editor"
        );
        // 'c' set the high nibble of 'o' (0x6f), keeping the low one.
        assert_eq!(app.hex.as_ref().unwrap().bytes[0], 0xcf);
        let _ = std::fs::remove_file(&path);
    }

    fn scan_hit(
        path: &str,
        kind: Kind,
        format: Option<&'static str>,
        secs: u64,
        rank: u8,
    ) -> crate::scan::Hit {
        crate::scan::Hit {
            path: path.into(),
            kind,
            format,
            size: 1024,
            modified: std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
            rank,
        }
    }

    /// The list an index of the whole disk hands the picker is hundreds of rows,
    /// and it is merged again every time the picker opens — pressing Esc out of a
    /// file has to come straight back to the list. A per-pair filesystem lookup
    /// made that quadratic in syscalls, which is what this holds down.
    #[test]
    fn merging_a_whole_disk_index_is_not_quadratic() {
        let hits: Vec<crate::scan::Hit> = (0..600)
            .map(|i| {
                scan_hit(
                    &format!("/h/dir{}/file{i}.db", i % 40),
                    Kind::Sqlite,
                    None,
                    i as u64,
                    2,
                )
            })
            .collect();
        let mut choices: Vec<super::Choice> = Vec::new();
        let started = std::time::Instant::now();
        merge(&mut choices, hits.clone());
        // The second merge is the one a running walk repeats every frame: every
        // row is already listed, so every one takes the duplicate path.
        merge(&mut choices, hits);
        let took = started.elapsed();
        assert_eq!(choices.len(), 600);
        assert!(
            took < std::time::Duration::from_millis(50),
            "merging 600 rows twice took {took:?}"
        );
    }

    /// Scan hits are ordered by what the tool is for: recognized shards, then
    /// other rkyv archives, then databases — newest first within each group.
    #[test]
    fn scan_hits_are_ranked_before_being_shown() {
        let mut choices: Vec<super::Choice> = Vec::new();
        merge(
            &mut choices,
            vec![
                scan_hit("/h/.zshrs/compsys.db", Kind::Sqlite, None, 300, 2),
                scan_hit("/h/.zshrs/images/a.rkyv", Kind::Rkyv, None, 100, 1),
                scan_hit(
                    "/h/.zshrs/scripts.rkyv",
                    Kind::Rkyv,
                    Some("zshrs script cache (ZRSC)"),
                    50,
                    0,
                ),
                scan_hit("/h/.zshrs/index.rkyv", Kind::Rkyv, None, 200, 1),
            ],
        );
        let order: Vec<&str> = choices.iter().map(|c| c.name()).collect();
        assert_eq!(
            order,
            vec!["scripts.rkyv", "index.rkyv", "a.rkyv", "compsys.db"],
            "recognized shard first, then rkyv newest-first, then databases"
        );
    }

    /// A recent file the scan also finds must not appear twice, and must keep its
    /// recency position and age column.
    #[test]
    fn scan_hits_do_not_duplicate_recent_files() {
        let path = scratch("db");
        std::fs::write(&path, b"x").unwrap();
        let entry = Entry {
            path: path.clone(),
            kind: Kind::Sqlite,
            opened: std::time::SystemTime::now(),
        };
        let mut choices: Vec<super::Choice> = vec![super::Choice::from_entry(&entry)];
        merge(
            &mut choices,
            vec![
                crate::scan::Hit {
                    path: path.clone(),
                    kind: Kind::Sqlite,
                    format: None,
                    size: 1,
                    modified: std::time::SystemTime::now(),
                    rank: 2,
                },
                scan_hit("/h/other.db", Kind::Sqlite, None, 10, 2),
            ],
        );
        assert_eq!(choices.len(), 2, "the duplicate must be dropped");
        assert_eq!(choices[0].path, path);
        assert!(choices[0].opened.is_some(), "still a recent file");
        assert!(choices[1].path.ends_with("other.db"));
        let _ = std::fs::remove_file(&path);
    }

    /// The picker shows scan progress, the recent/scanned split, and the
    /// recognized format of the selected row.
    #[test]
    fn picker_shows_scan_progress_and_row_details() {
        let theme = crate::theme::Theme::from_name(ThemeName::NeonSprawl);
        let mut choices: Vec<super::Choice> = Vec::new();
        merge(
            &mut choices,
            vec![scan_hit(
                "/h/.zshrs/scripts.rkyv",
                Kind::Rkyv,
                Some("zshrs script cache (ZRSC)"),
                50,
                0,
            )],
        );
        let render = |choices: &[super::Choice], scanning: Option<usize>| -> Vec<String> {
            let mut term = Terminal::new(TestBackend::new(100, 8)).unwrap();
            term.draw(|f| {
                let view: Vec<usize> = (0..choices.len()).collect();
                super::render_picker(f, choices, &view, 0, "", None, scanning, None, &theme);
            })
            .unwrap();
            let buf = term.backend().buffer().clone();
            (0..buf.area().height)
                .map(|y| {
                    (0..buf.area().width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect()
        };

        let rows = render(&choices, Some(1));
        assert!(
            contains(&rows, "scanning… 1 found"),
            "no progress in the title"
        );
        assert!(contains(&rows, "1 files (0 recent)") || contains(&rows, "1 files"));
        assert!(contains(&rows, "scripts.rkyv"));
        assert!(contains(&rows, "1.0 K"), "size column for a scanned row");
        assert!(
            contains(&rows, "zshrs script cache (ZRSC)"),
            "selected row's format on the bottom line"
        );

        // Once the scan is done the progress text goes away.
        let rows = render(&choices, None);
        assert!(!contains(&rows, "scanning"));

        // With nothing found at all, the empty state explains what to do.
        let rows = render(&[], None);
        assert!(contains(&rows, "Nothing found."));
        assert!(contains(&rows, "--scan"));
        // While still scanning, it says so instead.
        let rows = render(&[], Some(0));
        assert!(contains(&rows, "Scanning for databases"));
    }

    /// `/` filters the list as the pattern is typed — the iftoprs model — instead
    /// of hopping between matches.
    #[test]
    fn slash_filters_the_records_list_while_typing() {
        let path = scratch("rkyv");
        let recs: Vec<(String, Vec<u8>)> = ["alpha", "bravo", "charlie", "delta"]
            .iter()
            .map(|n| (format!("/tmp/{n}.sh"), vec![b'x']))
            .collect();
        std::fs::write(&path, crate::formats::test_script_shard_bytes_many(&recs)).unwrap();
        let store = Store::Rkyv(RkyvStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        assert_eq!(app.visible_records().len(), 4);

        press(&mut app, '/');
        press(&mut app, 'a');
        // Only keys containing "a" stay listed: alpha, bravo, charlie, delta all
        // do, so narrow further.
        press(&mut app, 'l');
        let listed: Vec<String> = app
            .visible_records()
            .iter()
            .map(|&i| app.decoded.as_ref().unwrap().records[i].key.clone())
            .collect();
        assert_eq!(listed.len(), 1, "got {listed:?}");
        assert!(listed[0].contains("alpha"));
        assert!(app.status.contains("1 match"), "got {:?}", app.status);
        // The selection sits on a listed row.
        assert!(app.visible_records().contains(&app.record_idx));

        // The rendered list shows only the match.
        let rows = frame_rows(&mut app, 100, 16);
        assert!(contains(&rows, "alpha"));
        assert!(!contains(&rows, "bravo"), "filtered-out key still drawn");
        assert!(contains(&rows, "1/4 keys"), "count missing from the title");

        // Backspacing widens the filter again.
        app.on_key(KeyEvent::from(KeyCode::Backspace));
        assert_eq!(app.visible_records().len(), 4, "'a' matches every key");

        // A pattern matching nothing empties the list and says so.
        for c in "zzz".chars() {
            press(&mut app, c);
        }
        assert!(app.visible_records().is_empty());
        assert!(app.status.contains("0 matches"), "got {:?}", app.status);

        // Esc drops the filter and restores the position.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.filter.is_empty(), "Esc must clear the filter");
        assert_eq!(app.visible_records().len(), 4);
        assert_eq!(app.record_idx, 0);

        // Enter keeps it applied.
        press(&mut app, '/');
        for c in "brav".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.filter, "brav", "Enter keeps the filter");
        assert_eq!(app.visible_records().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    /// Navigation stays inside the filtered list.
    #[test]
    fn navigation_skips_filtered_out_rows() {
        let path = scratch("rkyv");
        let recs: Vec<(String, Vec<u8>)> = ["a_one", "b_skip", "a_two", "c_skip", "a_three"]
            .iter()
            .map(|n| (format!("/tmp/{n}.sh"), vec![b'x']))
            .collect();
        std::fs::write(&path, crate::formats::test_script_shard_bytes_many(&recs)).unwrap();
        let store = Store::Rkyv(RkyvStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        press(&mut app, '/');
        for c in "a_".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let visible = app.visible_records();
        assert_eq!(visible.len(), 3, "three keys start with a_");
        app.record_idx = visible[0];
        for expected in &visible[1..] {
            press(&mut app, 'j');
            assert_eq!(
                app.record_idx, *expected,
                "j must land on the next listed row"
            );
        }
        // At the end it stays put rather than falling onto a hidden row.
        press(&mut app, 'j');
        assert_eq!(app.record_idx, *visible.last().unwrap());
        press(&mut app, 'k');
        assert_eq!(app.record_idx, visible[1]);
        let _ = std::fs::remove_file(&path);
    }

    /// The Strings view filters the same way.
    #[test]
    fn slash_filters_the_strings_view() {
        let mut app = rkyv_app_with(b"\x00\x00alpha\x00\x00bravo\x00\x00charlie\x00\x00");
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        let all = app.visible_strings().len();
        assert!(all >= 3, "{:?}", app.strings);
        press(&mut app, '/');
        for c in "brav".chars() {
            press(&mut app, c);
        }
        let listed = app.visible_strings();
        assert_eq!(listed.len(), 1);
        assert!(app.strings[listed[0]].text.contains("bravo"));
        let rows = frame_rows(&mut app, 100, 16);
        assert!(contains(&rows, "bravo") && !contains(&rows, "alpha"));
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.visible_strings().len(), all);
    }

    /// SQLite rows are filtered in SQL, so the whole table is covered and the
    /// totals follow the filter rather than the page.
    #[test]
    fn slash_filters_sqlite_rows_across_the_whole_table() {
        let (mut app, path) = sqlite_app_rows(60);
        app.on_key(KeyEvent::from(KeyCode::Tab)); // focus the grid
        assert_eq!(app.rows.as_ref().unwrap().total, 60);
        press(&mut app, '/');
        press(&mut app, '4');
        let view = app.rows.as_ref().unwrap();
        // Rows 4, 14, 24, 34, 40..49, 54 contain a '4'.
        assert_eq!(view.total, 15, "total must count matches, not all rows");
        assert!(
            view.rows.iter().all(|r| r.iter().any(|c| c.contains('4'))),
            "every listed row must match: {:?}",
            view.rows
        );
        assert!(app.status.contains("15 matches"), "got {:?}", app.status);

        // Esc restores the unfiltered grid.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.rows.as_ref().unwrap().total, 60);
        assert!(app.filter.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// The table list filters too.
    #[test]
    fn slash_filters_the_table_list() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        for t in ["users", "user_roles", "orders"] {
            conn.execute(&format!("CREATE TABLE {t} (x)"), []).unwrap();
        }
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        assert_eq!(app.visible_tables().len(), 3);
        press(&mut app, '/');
        for c in "user".chars() {
            press(&mut app, c);
        }
        assert_eq!(app.visible_tables().len(), 2);
        let rows = frame_rows(&mut app, 100, 16);
        assert!(contains(&rows, "users"));
        assert!(
            !contains(&rows, "orders"),
            "a filtered-out table is still on screen: {rows:#?}"
        );
        assert!(
            contains(&rows, "tables 2/3"),
            "count missing: {:?}",
            rows[0]
        );
        let _ = std::fs::remove_file(&path);
    }

    /// PageUp/PageDown must move by what is on screen. They used to be bound to
    /// the 500-row SQL window, which did nothing at all on a smaller table, and
    /// were missing outright from the Records and Strings views.
    #[test]
    fn paging_moves_by_a_screenful() {
        let (mut app, path) = sqlite_app_rows(120);
        app.on_key(KeyEvent::from(KeyCode::Tab)); // focus the grid
                                                  // A 24-row terminal keeps 23 rows for the body (one is the status bar),
                                                  // of which the grid shows 20 (two borders and a header row).
        frame_rows(&mut app, 80, 24);
        assert_eq!(app.page_rows, 20);
        app.on_key(KeyEvent::from(KeyCode::PageDown));
        assert_eq!(app.row_idx, 20, "PageDown moves one screenful");
        app.on_key(KeyEvent::from(KeyCode::PageDown));
        assert_eq!(app.row_idx, 40);
        app.on_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(app.row_idx, 20);
        // Ctrl-f / Ctrl-b do the same.
        app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(app.row_idx, 40);
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(app.row_idx, 20);
        // Paging down past the end stops on the last row, it does not wrap.
        for _ in 0..20 {
            app.on_key(KeyEvent::from(KeyCode::PageDown));
        }
        assert_eq!(app.row_idx, 119, "clamps to the last loaded row");
        app.on_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(app.row_idx, 99);
        // And up past the start stops at the top.
        for _ in 0..20 {
            app.on_key(KeyEvent::from(KeyCode::PageUp));
        }
        assert_eq!(app.row_idx, 0);
        let _ = std::fs::remove_file(&path);

        // A smaller terminal pages by less.
        let (mut app, path) = sqlite_app_rows(120);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        frame_rows(&mut app, 80, 12);
        assert_eq!(app.page_rows, 8);
        app.on_key(KeyEvent::from(KeyCode::PageDown));
        assert_eq!(app.row_idx, 8);
        let _ = std::fs::remove_file(&path);
    }

    /// The rkyv views scroll their own index, and paging must reach all of them.
    #[test]
    fn paging_works_in_every_rkyv_view() {
        let mut app = rkyv_app_with(&vec![b'A'; 4096]);
        frame_rows(&mut app, 80, 24);
        let step = app.page_rows;
        assert!(step > 1);

        // Hex view: pages of rows, not a fixed 16 bytes.
        app.on_key(KeyEvent::from(KeyCode::Char('3')));
        app.on_key(KeyEvent::from(KeyCode::PageDown));
        assert_eq!(app.hex_row, step);
        app.on_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(app.hex_row, 0);

        // Strings view: one long run of 'A's makes exactly one entry, so paging
        // clamps rather than running away.
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.on_key(KeyEvent::from(KeyCode::PageDown));
        assert_eq!(app.string_idx, app.strings.len().saturating_sub(1));
        app.on_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(app.string_idx, 0);

        // Info scrolls nothing, and must not panic.
        app.on_key(KeyEvent::from(KeyCode::Char('1')));
        app.on_key(KeyEvent::from(KeyCode::PageDown));
    }

    /// Records paging walks the record list.
    #[test]
    fn paging_walks_the_records_view() {
        let path = scratch("rkyv");
        // 40 records, so a page is bounded by the record count.
        let mut keys: Vec<(String, Vec<u8>)> = Vec::new();
        for i in 0..40 {
            keys.push((format!("/tmp/s{i:02}.sh"), vec![b'x']));
        }
        std::fs::write(&path, crate::formats::test_script_shard_bytes_many(&keys)).unwrap();
        let store = Store::Rkyv(RkyvStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        frame_rows(&mut app, 80, 14);
        let step = app.page_rows;
        assert_eq!(app.record_idx, 0);
        app.on_key(KeyEvent::from(KeyCode::PageDown));
        assert_eq!(app.record_idx, step);
        app.on_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(app.record_idx, 0);
        for _ in 0..20 {
            app.on_key(KeyEvent::from(KeyCode::PageDown));
        }
        assert_eq!(app.record_idx, 39, "clamps to the last record");
        let _ = std::fs::remove_file(&path);
    }

    /// The detail screen's value pane pages too. The schema screen pages its
    /// selection rather than its scroll, which
    /// [`the_schema_screen_follows_its_selection`] covers.
    #[test]
    fn paging_scrolls_the_detail_screen() {
        let (mut app, path) = sqlite_app_rows(5);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        app.on_key(KeyEvent::from(KeyCode::Enter)); // detail
        assert_eq!(app.screen, super::Screen::Detail);
        app.detail_value = vec![0u8; 16 * 400];
        frame_rows(&mut app, 80, 30);
        let step = app.page_rows;
        assert!(step > 1);
        app.on_key(KeyEvent::from(KeyCode::PageDown));
        assert_eq!(app.detail_scroll, step);
        app.on_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(app.detail_scroll, 0);
        app.on_key(KeyEvent::from(KeyCode::Esc));

        let _ = std::fs::remove_file(&path);
    }

    /// Esc on the first level backs out to the file list; `q` is what quits. Esc
    /// inside a nested screen still just leaves that screen.
    #[test]
    fn esc_on_the_first_level_returns_to_the_file_list() {
        // rkyv main screen.
        let mut app = rkyv_app();
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.reopen, "Esc must ask for the file list");
        assert!(app.quit, "and end the app loop");

        // `q` still quits outright.
        let mut app = rkyv_app();
        press(&mut app, 'q');
        assert!(app.quit);
        assert!(!app.reopen, "q must not reopen the picker");

        // SQLite main screen behaves the same.
        let (mut app, path) = sqlite_app();
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.reopen && app.quit);
        let _ = std::fs::remove_file(&path);

        // A nested screen keeps Esc for backing out of itself.
        let (mut app, path) = script_shard_app();
        app.on_key(KeyEvent::from(KeyCode::Enter)); // detail
        assert_eq!(app.screen, super::Screen::Detail);
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.screen, super::Screen::Main);
        assert!(
            !app.reopen,
            "Esc in a nested screen must not leave the file"
        );
        assert!(!app.quit);
        // From the main screen the next Esc does back out.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.reopen && app.quit);
        let _ = std::fs::remove_file(&path);
    }

    /// `o` leaves the app loop asking for the picker again, rather than quitting.
    #[test]
    fn o_returns_to_the_file_picker() {
        let mut app = rkyv_app();
        assert!(!app.reopen);
        press(&mut app, 'o');
        assert!(app.reopen, "o must ask for the picker");
        assert!(app.quit, "and end the app loop");

        // Inside the hex editor `o` is an insert, not a way out.
        let (mut app, path) = script_shard_app();
        press(&mut app, 'e');
        let before = app.hex.as_ref().unwrap().bytes.len();
        press(&mut app, 'o');
        assert!(!app.reopen, "the editor keeps o for inserting a byte");
        assert_eq!(app.hex.as_ref().unwrap().bytes.len(), before + 1);
        let _ = std::fs::remove_file(&path);
    }

    /// `:` opens the SQL editor and a statement runs through it: a SELECT comes
    /// back as rows (the old prompt reported only an affected count), a write
    /// reports what it changed and reloads the grid, and a bad statement shows
    /// SQLite's own message.
    #[test]
    fn sql_editor_runs_statements_and_shows_results() {
        use crate::sqledit::Entry;
        let (mut app, path) = sqlite_app_rows(5);
        press(&mut app, ':');
        assert_eq!(app.screen, super::Screen::Sql);
        assert!(app.sql.is_some(), "the editor is open");

        // A SELECT returns rows.
        app.execute_sql("SELECT a FROM t ORDER BY a LIMIT 2");
        // Each statement is timed, as the shell's `.timer` does, so the rows sit
        // one entry back.
        let log = app.sql.as_ref().unwrap().transcript();
        assert!(
            matches!(log.last(), Some(Entry::Timing(_))),
            "the run is timed: {:?}",
            log.last()
        );
        match &log[log.len() - 2] {
            Entry::Rows {
                columns,
                rows,
                truncated,
            } => {
                assert_eq!(columns, &["a".to_string()]);
                assert_eq!(rows.len(), 2, "two rows for LIMIT 2");
                assert!(!truncated);
            }
            other => panic!("expected rows, got {other:?}"),
        }

        // A write reports its change count and the grid reloads with it.
        app.execute_sql("UPDATE t SET b = 'z'");
        let log = app.sql.as_ref().unwrap().transcript();
        assert_eq!(log[log.len() - 2], Entry::Changed(5));
        assert!(
            app.rows.as_ref().unwrap().rows.iter().all(|r| r[1] == "z"),
            "the grid must show the write"
        );

        // A bad statement surfaces the error rather than a bare status line.
        app.execute_sql("SELECT * FROM nope");
        let log = app.sql.as_ref().unwrap().transcript();
        match &log[log.len() - 2] {
            Entry::Error(msg) => assert!(msg.contains("nope"), "got {msg:?}"),
            other => panic!("expected an error, got {other:?}"),
        }

        // Alt-e explains instead of running: the plan lands in the transcript and
        // the table is untouched.
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let mut e = KeyEvent::from(KeyCode::Char('e'));
        e.modifiers = KeyModifiers::ALT;
        app.on_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        for c in "SELECT * FROM t".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_key(e);
        let log = app.sql.as_ref().unwrap().transcript();
        let plan = log
            .iter()
            .rev()
            .find_map(|x| match x {
                Entry::Plan(steps) => Some(steps.clone()),
                _ => None,
            })
            .expect("a plan entry");
        assert!(
            plan.iter()
                .any(|s| s.contains("SCAN") || s.contains("TABLE")),
            "plan should name a scan: {plan:?}"
        );

        // The editor draws over the app, and Esc returns to the data with the
        // transcript intact for the next visit.
        let rows = frame_rows(&mut app, 100, 20);
        assert!(contains(&rows, "SQL ["), "editor not drawn: {:?}", rows[0]);
        assert!(contains(&rows, "no such table"), "error not on screen");
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.screen, super::Screen::Main);
        press(&mut app, ':');
        assert!(
            !app.sql.as_ref().unwrap().transcript().is_empty(),
            "reopening keeps the transcript"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The write monitor: `w` opens it, it shows the watched stores, and rows
    /// light up as bytes land.
    #[test]
    fn write_monitor_shows_live_writes() {
        use std::io::Write;
        let mut app = rkyv_app();
        press(&mut app, 'w');
        assert_eq!(app.screen, super::Screen::Top);
        let watcher = app.top.as_ref().expect("watcher running");
        assert!(
            !watcher.is_empty(),
            "the open file must be watched at least"
        );
        assert!(app.status.contains("watching"), "got {:?}", app.status);
        assert_eq!(app.help_ctx(), HelpCtx::Top);

        // Watch a file we can write to, then write to it.
        let path = scratch("rkyv");
        std::fs::write(&path, b"start").unwrap();
        app.top = Some(crate::monitor::Monitor::new([(path.clone(), Kind::Rkyv)]));
        let rows = frame_rows(&mut app, 110, 14);
        assert!(
            contains(&rows, "writes —"),
            "no monitor header: {:?}",
            rows[0]
        );
        assert!(contains(&rows, "activity"), "no column header");
        assert!(contains(&rows, "0 active"), "nothing should be active yet");

        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&[b'x'; 4096]).unwrap();
        }
        // Force a sample the way the event loop's tick does.
        let m = app.top.as_mut().unwrap();
        m.watcher.interval = std::time::Duration::ZERO;
        assert!(m.tick());
        m.watcher.interval = crate::watch::DEFAULT_INTERVAL;

        let rows = frame_rows(&mut app, 110, 14);
        assert!(contains(&rows, "1 active"), "the write was not noticed");
        assert!(contains(&rows, "4.0 K"), "written bytes missing: {rows:#?}");
        assert!(
            rows.iter().any(|r| r.contains('█') || r.contains('▁')),
            "no activity sparkline drawn"
        );

        // Sorting, pausing and the sample interval are all reachable.
        let before = app.top.as_ref().unwrap().sort;
        press(&mut app, 's');
        assert_ne!(app.top.as_ref().unwrap().sort, before);
        assert!(app.status.contains("sorted by"));
        press(&mut app, 'p');
        assert!(app.top.as_ref().unwrap().watcher.paused);
        assert!(contains(&frame_rows(&mut app, 110, 14), "PAUSED"));
        press(&mut app, 'p');
        assert!(!app.top.as_ref().unwrap().watcher.paused);
        let iv = app.top.as_ref().unwrap().watcher.interval;
        press(&mut app, '+');
        assert!(
            app.top.as_ref().unwrap().watcher.interval < iv,
            "+ samples faster"
        );
        press(&mut app, '-');
        assert_eq!(app.top.as_ref().unwrap().watcher.interval, iv);

        // Enter hands the selected file to the caller instead of opening it here.
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.open_next(), Some(path.clone()));
        assert!(app.reopen && app.quit);

        // `w` again (or Esc) leaves the monitor.
        let mut app = rkyv_app();
        press(&mut app, 'w');
        press(&mut app, 'w');
        assert_eq!(app.screen, super::Screen::Main);
        assert!(app.top.is_none(), "the watcher is dropped on the way out");
        let _ = std::fs::remove_file(&path);
    }

    /// Opening a file must keep the scheme the picker was showing. It used to
    /// re-read prefs, so a concurrent write (or any drift) reset the colours on
    /// open — "randomly resetting my colorscheme when I click a file".
    #[test]
    fn opening_a_file_keeps_the_picker_scheme() {
        let path = scratch("rkyv");
        std::fs::write(
            &path,
            crate::formats::test_script_shard_bytes("/tmp/a.sh", b"x"),
        )
        .unwrap();
        let store = Store::Rkyv(RkyvStore::open(&path).unwrap());

        // Whatever prefs say, the handed-over scheme is what the app shows.
        let handed = Theme::from_name(ThemeName::BladeRunner);
        let app = App::with_theme(store, handed);
        assert_eq!(app.theme().name, ThemeName::BladeRunner);
        assert_eq!(app.theme().accent, handed.accent);

        // A custom palette survives the hand-over too, which `Theme::from_name`
        // alone would have flattened back to the scheme's stock colours.
        let store = Store::Rkyv(RkyvStore::open(&path).unwrap());
        let custom = Theme::from_palette(ThemeName::BladeRunner, [10, 20, 30, 40, 50, 60]);
        let app = App::with_theme(store, custom);
        assert_eq!(app.theme().accent, ratatui::style::Color::Indexed(20));
        assert_eq!(app.theme().primary, ratatui::style::Color::Indexed(10));

        // And the app reports back whatever it ended on, so the picker resumes
        // in the same scheme.
        let store = Store::Rkyv(RkyvStore::open(&path).unwrap());
        let mut app = App::with_theme(store, handed);
        press(&mut app, 'c');
        app.on_key(KeyEvent::from(KeyCode::Down));
        let previewed = app.theme().name;
        assert_ne!(previewed, ThemeName::BladeRunner, "chooser previews live");
        assert_eq!(
            app.theme().name,
            previewed,
            "theme() reports the live scheme"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The arrows must keep working while a filter prompt is open — they were
    /// swallowed by the prompt, so the list froze as soon as `/` was pressed.
    #[test]
    fn arrows_navigate_while_the_filter_prompt_is_open() {
        // rkyv Records: three of five keys match, and Up/Down walk those three.
        let path = scratch("rkyv");
        let recs: Vec<(String, Vec<u8>)> = ["a_one", "b_skip", "a_two", "c_skip", "a_three"]
            .iter()
            .map(|n| (format!("/tmp/{n}.sh"), vec![b'x']))
            .collect();
        std::fs::write(&path, crate::formats::test_script_shard_bytes_many(&recs)).unwrap();
        let store = Store::Rkyv(RkyvStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        press(&mut app, '/');
        for c in "a_".chars() {
            press(&mut app, c);
        }
        let visible = app.visible_records();
        assert_eq!(visible.len(), 3);
        assert!(
            matches!(app.mode, super::Mode::Search(_)),
            "prompt must stay open"
        );

        app.on_key(KeyEvent::from(KeyCode::Down));
        assert!(
            matches!(app.mode, super::Mode::Search(_)),
            "Down must not close it"
        );
        assert_eq!(app.record_idx, visible[1], "Down moves within the matches");
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.record_idx, visible[2]);
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.record_idx, visible[2], "clamps at the last match");
        app.on_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.record_idx, visible[1]);

        // Typing more still narrows from the top.
        press(&mut app, 't');
        assert_eq!(app.visible_records().len(), 2, "a_two and a_three");
        let _ = std::fs::remove_file(&path);

        // SQLite rows: Down moves inside the filtered page.
        let (mut app, path) = sqlite_app_rows(60);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        press(&mut app, '/');
        press(&mut app, '4');
        assert_eq!(app.row_idx, 0);
        app.on_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.row_idx, 1, "Down must move the filtered grid");
        app.on_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.row_idx, 0);
        // Paging works from the prompt too.
        frame_rows(&mut app, 80, 24);
        app.on_key(KeyEvent::from(KeyCode::PageDown));
        assert!(app.row_idx > 0);
        assert!(matches!(app.mode, super::Mode::Search(_)), "still typing");
        let _ = std::fs::remove_file(&path);
    }

    /// The picker's prompt behaves the same way.
    #[test]
    fn picker_prompt_keys_navigate_and_edit() {
        use super::{filter_prompt_key, Prompt};
        let mut filter = String::new();
        let mut sel = 0usize;
        let (last, page) = (9usize, 4usize);

        // Typing narrows and resets to the top of the new list.
        assert_eq!(
            filter_prompt_key(KeyCode::Char('z'), &mut filter, &mut sel, last, page),
            Prompt::Open
        );
        assert_eq!(filter, "z");
        assert_eq!(sel, 0);

        // Arrows move the selection without touching the pattern.
        filter_prompt_key(KeyCode::Down, &mut filter, &mut sel, last, page);
        filter_prompt_key(KeyCode::Down, &mut filter, &mut sel, last, page);
        assert_eq!((sel, filter.as_str()), (2, "z"));
        filter_prompt_key(KeyCode::Up, &mut filter, &mut sel, last, page);
        assert_eq!(sel, 1);
        filter_prompt_key(KeyCode::PageDown, &mut filter, &mut sel, last, page);
        assert_eq!(sel, 5);
        filter_prompt_key(KeyCode::PageUp, &mut filter, &mut sel, last, page);
        assert_eq!(sel, 1);
        filter_prompt_key(KeyCode::End, &mut filter, &mut sel, last, page);
        assert_eq!(sel, last, "End goes to the last match");
        filter_prompt_key(KeyCode::Home, &mut filter, &mut sel, last, page);
        assert_eq!(sel, 0);
        // Clamped at both ends.
        filter_prompt_key(KeyCode::Up, &mut filter, &mut sel, last, page);
        assert_eq!(sel, 0);
        sel = last;
        filter_prompt_key(KeyCode::Down, &mut filter, &mut sel, last, page);
        assert_eq!(sel, last);

        // Backspace edits and returns to the top.
        filter_prompt_key(KeyCode::Backspace, &mut filter, &mut sel, last, page);
        assert!(filter.is_empty());
        assert_eq!(sel, 0);

        assert_eq!(
            filter_prompt_key(KeyCode::Enter, &mut filter, &mut sel, last, page),
            Prompt::Accept
        );
        assert_eq!(
            filter_prompt_key(KeyCode::Esc, &mut filter, &mut sel, last, page),
            Prompt::Cancel
        );
    }

    /// The picker's `/` must remove rows, not just move the cursor.
    #[test]
    fn picker_filter_removes_rows_from_the_list() {
        let theme = crate::theme::Theme::from_name(ThemeName::NeonSprawl);
        let mut choices: Vec<super::Choice> = Vec::new();
        merge(
            &mut choices,
            vec![
                scan_hit("/h/.zshrs/scripts.rkyv", Kind::Rkyv, None, 30, 1),
                scan_hit("/h/.zshrs/compsys.db", Kind::Sqlite, None, 20, 2),
                scan_hit("/h/.pythonrs/scripts.rkyv", Kind::Rkyv, None, 10, 1),
            ],
        );
        let render = |filter: &str| -> Vec<String> {
            let view: Vec<usize> = choices
                .iter()
                .enumerate()
                .filter(|(_, c)| super::filter_passes(filter, c.path.to_str().unwrap()))
                .map(|(i, _)| i)
                .collect();
            let mut term = Terminal::new(TestBackend::new(100, 10)).unwrap();
            term.draw(|f| {
                super::render_picker(f, &choices, &view, 0, filter, None, None, None, &theme);
            })
            .unwrap();
            let buf = term.backend().buffer().clone();
            (0..buf.area().height)
                .map(|y| {
                    (0..buf.area().width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect()
        };

        // Unfiltered: all three.
        let rows = render("");
        assert!(contains(&rows, "compsys.db"));
        assert!(contains(&rows, "3 files"));

        // Filtering by directory drops the pythonrs row entirely.
        let rows = render("zshrs");
        assert!(contains(&rows, "scripts.rkyv") && contains(&rows, "compsys.db"));
        assert!(
            !contains(&rows, ".pythonrs"),
            "a filtered-out row is still listed: {rows:#?}"
        );
        assert!(contains(&rows, "2/3 files"), "count missing: {:?}", rows[0]);
        assert!(contains(&rows, "/zshrs"), "the pattern must be shown");

        // Filtering by name works the same way.
        let rows = render("compsys");
        assert!(contains(&rows, "compsys.db") && !contains(&rows, "pythonrs"));
        assert!(contains(&rows, "1/3 files"));

        // No match: the list is empty and says how to get out.
        let rows = render("nothing-matches-this");
        assert!(contains(&rows, "Nothing matches"));
        assert!(contains(&rows, "Esc clears the filter"));
    }

    /// Rows restored from the saved scan are labelled with their age, so it is
    /// clear the list was not just walked.
    #[test]
    fn picker_titles_a_reused_scan_with_its_age() {
        let theme = crate::theme::Theme::from_name(ThemeName::NeonSprawl);
        let mut choices: Vec<super::Choice> = Vec::new();
        merge(
            &mut choices,
            vec![scan_hit("/h/.zshrs/scripts.rkyv", Kind::Rkyv, None, 50, 1)],
        );
        let render = |age: Option<std::time::Duration>| -> Vec<String> {
            let mut term = Terminal::new(TestBackend::new(100, 6)).unwrap();
            term.draw(|f| {
                let view: Vec<usize> = (0..choices.len()).collect();
                super::render_picker(f, &choices, &view, 0, "", None, None, age, &theme);
            })
            .unwrap();
            let buf = term.backend().buffer().clone();
            (0..buf.area().height)
                .map(|y| {
                    (0..buf.area().width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect()
        };
        let rows = render(Some(std::time::Duration::from_secs(3 * 3600)));
        assert!(contains(&rows, "scan 3h old"), "age missing: {:?}", rows[0]);
        assert!(contains(&rows, "r rescans"), "no way to refresh advertised");
        // A walk that just finished says so instead of showing hours.
        let rows = render(Some(std::time::Duration::ZERO));
        assert!(contains(&rows, "just now"));
        // Without a saved scan the title is plain.
        let rows = render(None);
        assert!(!contains(&rows, "rescans"));
    }

    #[test]
    fn age_labels_read_naturally() {
        use std::time::Duration;
        assert_eq!(super::age_label(Duration::from_secs(5)), "just now");
        assert_eq!(super::age_label(Duration::from_secs(600)), "10m old");
        assert_eq!(super::age_label(Duration::from_secs(7200)), "2h old");
        assert_eq!(super::age_label(Duration::from_secs(3 * 86_400)), "3d old");
    }

    /// Opening a large archive must not block: the 382MB shard here took 25s to
    /// validate, which read as a hang. It now opens on the structural view and
    /// the decode lands later.
    #[test]
    fn a_large_archive_opens_without_blocking() {
        // A buffer past the inline limit that is not a recognized shard, so the
        // background decode returns None.
        let big = vec![0x41u8; super::DECODE_INLINE_MAX + 1024];
        let t0 = std::time::Instant::now();
        let mut app = rkyv_app_with(&big);
        let open = t0.elapsed();
        assert!(
            open < std::time::Duration::from_secs(2),
            "opening took {open:?}"
        );
        assert!(
            app.decoding.is_some(),
            "decode must be running in the background"
        );
        assert!(app.decoded.is_none(), "and not have blocked for the result");
        assert_eq!(app.rkyv_view, super::RkyvView::Info);
        assert!(app.status.contains("decoding"), "got {:?}", app.status);

        // Records is not reachable until it lands, and says so.
        app.on_key(KeyEvent::from(KeyCode::Char('0')));
        assert_eq!(app.rkyv_view, super::RkyvView::Info);
        assert!(
            app.status.contains("still decoding"),
            "got {:?}",
            app.status
        );

        // Wait for the thread, then install the result the way the loop does.
        let rx = app.decoding.as_ref().unwrap();
        let _ = rx.recv_timeout(std::time::Duration::from_secs(30));
        app.poll_decode();
        assert!(
            app.decoding.is_none(),
            "the receiver is dropped once it lands"
        );
        assert!(app.decoded.is_none(), "0x41 filler is not a shard");
        assert!(app.status.contains("unrecognized"), "got {:?}", app.status);

        // A small archive still decodes inline, so nothing changed for shards.
        let (app, path) = script_shard_app();
        assert!(app.decoding.is_none());
        assert!(app.decoded.is_some(), "small shards decode on open");
        let _ = std::fs::remove_file(&path);
    }

    /// String extraction is bounded, so a huge archive cannot stall the open.
    #[test]
    fn string_extraction_is_bounded() {
        // Alternating printable runs and NULs: one run per 8 bytes, far more runs
        // than the cap allows.
        let mut bytes = Vec::new();
        while bytes.len() < 400_000 {
            bytes.extend_from_slice(b"abcdefg\x00");
        }
        let path = scratch("bin");
        std::fs::write(&path, &bytes).unwrap();
        let store = RkyvStore::open(&path).unwrap();
        let t0 = std::time::Instant::now();
        let s = store.strings(4);
        let took = t0.elapsed();
        assert!(took < std::time::Duration::from_secs(1), "took {took:?}");
        assert!(s.hits.len() <= 20_000, "cap ignored: {}", s.hits.len());
        assert!(s.truncated, "truncation must be reported");
        let _ = std::fs::remove_file(&path);

        // A small file is complete, not flagged.
        let path = scratch("bin");
        std::fs::write(&path, b"hello\x00world\x00").unwrap();
        let store = RkyvStore::open(&path).unwrap();
        let s = store.strings(4);
        assert_eq!(s.hits.len(), 2);
        assert!(!s.truncated);
        assert_eq!(s.scanned, 12);
        let _ = std::fs::remove_file(&path);
    }

    /// Help lists the section for what is on screen.
    #[test]
    fn help_ctx_follows_the_screen() {
        assert_eq!(rkyv_app().help_ctx(), HelpCtx::Rkyv);
        let (app, path) = sqlite_app();
        assert_eq!(app.help_ctx(), HelpCtx::Sqlite);
        let _ = std::fs::remove_file(&path);

        let (mut app, path) = script_shard_app();
        press(&mut app, 'e');
        assert_eq!(app.help_ctx(), HelpCtx::HexEdit);
        let _ = std::fs::remove_file(&path);
    }

    /// Action results must raise a toast as well as land in the status bar.
    #[test]
    fn action_results_raise_a_toast() {
        let mut app = rkyv_app();
        assert!(app.ov.toast.is_none(), "no toast before any action");
        // Export on an unrecognized archive: reports that there is nothing to
        // write, without touching the filesystem.
        press(&mut app, 'x');
        let toast = app.ov.toast.as_ref().expect("an action must toast");
        assert!(!toast.text.is_empty());
        assert_eq!(app.status, toast.text, "status bar keeps the same text");
    }

    /// The overlay draws over the app's own screen, including a nested one.
    #[test]
    fn overlays_render_over_the_app_screens() {
        let mut app = rkyv_app();
        app.ov.help = true;
        let rows = frame_rows(&mut app, 100, 40);
        assert!(contains(&rows, "KEYBOARD SHORTCUTS"));
        assert!(contains(&rows, "RKYV"), "store section missing");

        app.ov.help = false;
        app.screen = super::Screen::Detail;
        app.ov.toast("copied 12 bytes to clipboard");
        let rows = frame_rows(&mut app, 100, 40);
        assert!(
            contains(&rows, "copied 12 bytes"),
            "toast missing on detail"
        );
    }

    #[test]
    fn cursor_left_right_utf8() {
        // "aé" — 'é' is 2 bytes, so byte offsets are 0,1,3.
        let s = "aé";
        assert_eq!(crate::input::right(s, 0), 1); // past 'a'
        assert_eq!(crate::input::right(s, 1), 3); // past 'é'
        assert_eq!(crate::input::right(s, 3), 3); // at end, stays
        assert_eq!(crate::input::left(s, 3), 1); // before 'é'
        assert_eq!(crate::input::left(s, 1), 0);
        assert_eq!(crate::input::left(s, 0), 0);
    }

    #[test]
    fn delete_word_skips_trailing_space() {
        let mut s = String::from("foo bar  ");
        let len = s.len();
        let cur = crate::input::delete_word(&mut s, len);
        assert_eq!(s, "foo ");
        assert_eq!(cur, 4);
    }

    #[test]
    fn hit_testing() {
        let r = Rect::new(2, 3, 10, 5); // x=2..12, y=3..8
        assert!(hit(r, 2, 3));
        assert!(hit(r, 11, 7));
        assert!(!hit(r, 12, 3)); // just past right edge
        assert!(!hit(r, 2, 8)); // just past bottom edge
        assert!(!hit(r, 1, 3));
    }

    #[test]
    fn find_next_forward_wraps() {
        // matches at indices 1 and 3 of a length-5 range
        let pred = |i: usize| i == 1 || i == 3;
        assert_eq!(find_next(5, 0, true, pred), Some(1));
        assert_eq!(find_next(5, 1, true, pred), Some(3));
        assert_eq!(find_next(5, 3, true, pred), Some(1)); // wrap past end
        assert_eq!(find_next(5, 4, true, pred), Some(1));
    }

    #[test]
    fn find_next_backward_wraps() {
        let pred = |i: usize| i == 1 || i == 3;
        assert_eq!(find_next(5, 4, false, pred), Some(3));
        assert_eq!(find_next(5, 3, false, pred), Some(1));
        assert_eq!(find_next(5, 1, false, pred), Some(3)); // wrap past start
        assert_eq!(find_next(5, 0, false, pred), Some(3));
    }

    #[test]
    fn find_next_none_and_empty() {
        assert_eq!(find_next(5, 0, true, |_| false), None);
        assert_eq!(find_next(0, 0, true, |_| true), None);
    }

    #[test]
    fn find_bytes_forward_and_backward() {
        let hay = b"abXYabZZab"; // "ab" at 0, 4, 8
        assert_eq!(find_bytes(hay, b"ab", 0, true), Some(4));
        assert_eq!(find_bytes(hay, b"ab", 4, true), Some(8));
        assert_eq!(find_bytes(hay, b"ab", 8, true), None); // nothing after
        assert_eq!(find_bytes(hay, b"ab", 8, false), Some(4));
        assert_eq!(find_bytes(hay, b"ab", 4, false), Some(0));
        assert_eq!(find_bytes(hay, b"zz", 0, true), None); // case-sensitive
        assert_eq!(find_bytes(hay, b"", 0, true), None);
    }

    // ----- what the grid shows ----------------------------------------------

    /// `H` hides the cursor column and `U` brings them all back, and the cursor
    /// never sits on a column that is not drawn.
    #[test]
    fn columns_hide_and_come_back() {
        let (mut app, path) = sqlite_app();
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert!(contains(&frame_rows(&mut app, 90, 12), "b"));

        app.on_key(KeyEvent::from(KeyCode::Right)); // column b
        assert_eq!(app.col_idx, 1);
        press(&mut app, 'H');
        assert_ne!(app.col_idx, 1, "the cursor left the hidden column");
        let rows = frame_rows(&mut app, 90, 12);
        assert!(
            rows.iter().any(|r| r.contains("a") && !r.contains("b")),
            "{rows:?}"
        );

        // Right steps over the hidden column instead of selecting it invisibly.
        app.col_idx = 0;
        app.on_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(app.col_idx, 2, "b was skipped");

        press(&mut app, 'U');
        let rows = frame_rows(&mut app, 90, 12);
        assert!(rows.iter().any(|r| r.contains("b")), "{rows:?}");
        let _ = std::fs::remove_file(path);
    }

    /// A display format is a SQL expression, so it changes what the query
    /// returns — the proof is a value the raw column does not contain.
    #[test]
    fn a_display_format_changes_what_the_query_returns() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (255)")
            .unwrap();
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert!(contains(&frame_rows(&mut app, 90, 12), "255"));

        // default → decimal → exponent → hex blob → hex number.
        for _ in 0..4 {
            press(&mut app, 'm');
        }
        await_queries(&mut app);
        assert_eq!(
            app.browse.view("t").format("n"),
            crate::browse::Format::HexNumber
        );
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "ff"), "255 in hex: {rows:?}");
        assert!(contains(&rows, "ƒ"), "the header marks the format");

        // `M` steps back to where it was.
        for _ in 0..4 {
            press(&mut app, 'M');
        }
        await_queries(&mut app);
        assert_eq!(
            app.browse.view("t").format("n"),
            crate::browse::Format::Default
        );
        assert!(contains(&frame_rows(&mut app, 90, 12), "255"));
        let _ = std::fs::remove_file(path);
    }

    /// `%` types a format of your own, `%1` standing for the column — and an
    /// expression that never mentions the column is refused, since it would show
    /// the same value in every row.
    #[test]
    fn a_custom_display_format_is_typed_and_checked() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (7)")
            .unwrap();
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));

        press(&mut app, '%');
        assert!(matches!(app.mode, super::Mode::CustomFormat(_)));
        for c in "'n=' || 1".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let rows = frame_rows(&mut app, 90, 12);
        assert!(
            contains(&rows, "%1"),
            "the refusal names the placeholder: {rows:?}"
        );
        assert_eq!(
            app.browse.view("t").format("n"),
            crate::browse::Format::Default
        );

        press(&mut app, '%');
        for c in "'n=' || %1".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        await_queries(&mut app);
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "n=7"), "{rows:?}");

        // An empty expression is how the column comes back.
        press(&mut app, '%');
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        await_queries(&mut app);
        assert_eq!(
            app.browse.view("t").format("n"),
            crate::browse::Format::Default
        );
        assert!(contains(&frame_rows(&mut app, 90, 12), "7"));
        let _ = std::fs::remove_file(path);
    }

    /// A table wider than the pane scrolls sideways, and frozen columns stay put
    /// while it does.
    #[test]
    fn a_wide_table_scrolls_and_freezes() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        let cols: Vec<String> = (0..12).map(|i| format!("col{i:02} TEXT")).collect();
        conn.execute_batch(&format!("CREATE TABLE wide ({})", cols.join(", ")))
            .unwrap();
        let vals: Vec<String> = (0..12).map(|i| format!("'v{i:02}'")).collect();
        conn.execute_batch(&format!("INSERT INTO wide VALUES ({})", vals.join(", ")))
            .unwrap();
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));

        // Only the first few columns fit in a 90-wide frame.
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "col00"), "{rows:?}");
        assert!(!contains(&rows, "col11"), "the far column is off screen");

        // Freeze the first column, then walk the cursor to the last one.
        press(&mut app, 'f');
        for _ in 0..11 {
            app.on_key(KeyEvent::from(KeyCode::Right));
        }
        let rows = frame_rows(&mut app, 90, 12);
        assert!(
            contains(&rows, "col11"),
            "the cursor column scrolled in: {rows:?}"
        );
        assert!(
            contains(&rows, "col00"),
            "the frozen column stayed at the edge: {rows:?}"
        );
        assert!(
            !contains(&rows, "col05"),
            "the middle scrolled away: {rows:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    /// `#` puts the rowid on screen as its own column, and says so rather than
    /// doing nothing on a table that has none.
    #[test]
    fn the_rowid_column_can_be_shown() {
        let (mut app, path) = sqlite_app();
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        // The toast that says what was toggled paints over the grid, so the
        // assertions are about the header row alone.
        let header = |app: &mut App| frame_rows(app, 90, 12)[1].clone();
        assert!(!header(&mut app).contains("rowid"));
        press(&mut app, '#');
        let h = header(&mut app);
        assert!(h.contains("rowid"), "{h}");
        assert!(app.browse.view("t").show_rowid);
        press(&mut app, '#');
        let h = header(&mut app);
        assert!(!h.contains("rowid"), "{h}");
        assert!(!app.browse.view("t").show_rowid);
        let _ = std::fs::remove_file(path);
    }

    /// `Ctrl-n` puts NULL in the cell, `Ctrl-r` re-reads the database, and `R`
    /// on the schema screen counts the rows.
    #[test]
    fn the_cell_and_refresh_actions_do_what_they_say() {
        let (mut app, path) = sqlite_app();
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));

        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        await_queries(&mut app);
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "NULL"), "the cell shows NULL: {rows:?}");
        app.write_changes();

        // Another connection writes; a refresh is what brings it into view.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("INSERT INTO t VALUES ('fresh', 'row', 'here')", [])
            .unwrap();
        drop(conn);
        app.on_key(KeyEvent::from(KeyCode::F(5)));
        await_queries(&mut app);
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "fresh"), "{rows:?}");

        // Row counts on the schema screen, and off again.
        press(&mut app, 'S');
        press(&mut app, 'R');
        let rows = frame_rows(&mut app, 90, 16);
        assert!(contains(&rows, "2 rows"), "{rows:?}");
        press(&mut app, 'R');
        let rows = frame_rows(&mut app, 90, 16);
        assert!(!contains(&rows, "2 rows"), "counts cleared: {rows:?}");
        let _ = std::fs::remove_file(path);
    }

    // ----- the SQL editor's files and results --------------------------------

    /// A statement written out with `Alt-s` comes back with `Alt-o`, and the
    /// result of running it exports as CSV or JSON by extension.
    #[test]
    fn the_editor_saves_and_loads_files_and_exports_its_result() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (a TEXT); INSERT INTO t VALUES ('x')")
            .unwrap();
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        press(&mut app, ':');
        assert_eq!(app.screen, super::Screen::Sql);

        let sql_file = scratch("sql");
        for c in "SELECT a FROM t".chars() {
            press(&mut app, c);
        }
        // Alt-s, then the path.
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT));
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in sql_file.to_string_lossy().chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            std::fs::read_to_string(&sql_file).unwrap().trim(),
            "SELECT a FROM t"
        );

        // A new tab, then load the file back into it.
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT));
        assert_eq!(app.sql.as_ref().unwrap().text(), "");
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT));
        for c in sql_file.to_string_lossy().chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.sql.as_ref().unwrap().text(), "SELECT a FROM t");

        // Run it, then export the result.
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let out = scratch("json");
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in out.to_string_lossy().chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let json = std::fs::read_to_string(&out).unwrap();
        assert!(json.contains("\"a\""), "{json}");
        assert!(json.contains("\"x\""), "{json}");

        for p in [path, sql_file, out] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// `Alt-v` turns the statement that produced the result into a view, since a
    /// result set cannot be one.
    #[test]
    fn the_editors_result_can_be_saved_as_a_view() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1), (2)")
            .unwrap();
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        press(&mut app, ':');
        for c in "SELECT a FROM t WHERE a > 1".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));

        app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT));
        for c in "big_a".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));

        let sql = app.sqlite().unwrap().object_sql("big_a").unwrap();
        let (ty, text) = sql.expect("the view exists");
        assert_eq!(ty, "view");
        assert!(text.contains("SELECT a FROM t WHERE a > 1"), "{text}");
        let _ = std::fs::remove_file(path);
    }

    // ----- Browse Data operations --------------------------------------------

    /// `r` asks what to find, then what to put there, and reports how many rows
    /// it touched. The write is buffered, so `R` takes it back.
    #[test]
    fn find_and_replace_runs_over_the_cursor_column() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (note TEXT); INSERT INTO t VALUES ('a cat'), ('a dog')")
            .unwrap();
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));

        press(&mut app, 'r');
        assert!(matches!(app.mode, super::Mode::FindText(_)));
        for c in "cat".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(app.mode, super::Mode::ReplaceText(_)));
        for c in "bird".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        await_queries(&mut app);
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "a bird"), "{rows:?}");
        assert!(app.has_pending(), "the replace is buffered");

        press(&mut app, 'R');
        await_queries(&mut app);
        assert!(contains(&frame_rows(&mut app, 90, 12), "a cat"));

        // A term that matches nothing never reaches the second prompt.
        press(&mut app, 'r');
        for c in "zzz".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(app.mode, super::Mode::Normal));
        assert!(contains(&frame_rows(&mut app, 90, 12), "not in note"));
        let _ = std::fs::remove_file(path);
    }

    /// `!` manages the cursor column's conditional formats, and the rules paint
    /// the grid as soon as they are set.
    #[test]
    fn conditional_formats_are_managed_and_painted() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (5), (500)")
            .unwrap();
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));

        press(&mut app, '!');
        assert_eq!(app.screen, super::Screen::CondFormat);
        let rows = frame_rows(&mut app, 90, 14);
        assert!(contains(&rows, "no rules"), "{rows:?}");

        press(&mut app, 'a');
        for c in "> 100".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        press(&mut app, 'c'); // next colour
        press(&mut app, 'b'); // bold
        let rows = frame_rows(&mut app, 90, 14);
        assert!(contains(&rows, "> 100"), "{rows:?}");
        assert!(contains(&rows, "bold"), "{rows:?}");

        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.screen, super::Screen::Main);
        let rules = app.browse.view("t").rules;
        assert_eq!(rules["n"].len(), 1);
        assert!(rules["n"][0].bold);
        assert_ne!(rules["n"][0].color, crate::browse::RuleColor::default());

        // The rule paints the row it matches, and only that row.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 12)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer().clone();
        // The row that holds 500 is the one the rule matches; 5 is on another row
        // and must be left alone.
        let row_colour = |needle: &str| -> Option<ratatui::style::Color> {
            (0..buf.area().height).find_map(|y| {
                let line: String = (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect();
                let at = line.find(needle)?;
                buf[(at as u16, y)].style().fg
            })
        };
        assert_eq!(
            row_colour("500"),
            Some(rules["n"][0].color.color()),
            "the matching row is painted"
        );
        assert_ne!(
            row_colour("5 "),
            Some(rules["n"][0].color.color()),
            "the row the rule does not match is left alone"
        );

        // Dropping the rule leaves nothing behind.
        press(&mut app, '!');
        press(&mut app, 'd');
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.browse.view("t").rules.is_empty());
        let _ = std::fs::remove_file(path);
    }

    /// `i` types a row column by column, and `n` leaves one NULL.
    #[test]
    fn the_insert_form_writes_a_typed_row() {
        let (mut app, path) = sqlite_app();
        await_queries(&mut app);
        press(&mut app, 'i');
        assert_eq!(app.screen, super::Screen::InsertRow);
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "insert into t"), "{rows:?}");

        app.on_key(KeyEvent::from(KeyCode::Enter));
        for c in "typed".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        press(&mut app, 'W');
        assert_eq!(app.screen, super::Screen::Main);
        app.write_changes();
        await_queries(&mut app);

        let conn = rusqlite::Connection::open(&path).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM t WHERE a = 'typed' AND b IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "the typed value landed and the rest stayed NULL");
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    /// `V` saves what the grid is showing as a view, and `z` / `Z` clear the sort
    /// and the filter.
    #[test]
    fn the_filter_can_be_saved_as_a_view_and_cleared() {
        let (mut app, path) = sqlite_app();
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));

        press(&mut app, 's'); // sort by the cursor column
        assert!(app.sort.is_some());
        press(&mut app, 'z');
        assert!(app.sort.is_none(), "z clears the sort");

        press(&mut app, '/');
        for c in "x".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.filter, "x");
        await_queries(&mut app);

        press(&mut app, 'V');
        assert!(matches!(app.mode, super::Mode::ViewName(_)));
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "just_x".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(
            app.sqlite()
                .unwrap()
                .object_sql("just_x")
                .unwrap()
                .is_some(),
            "the view was created"
        );

        press(&mut app, 'Z');
        assert!(app.filter.is_empty(), "Z clears the filter");
        let _ = std::fs::remove_file(path);
    }

    /// A view is read-only until SQLite can write to it, and the refusal says
    /// what is missing rather than failing silently.
    #[test]
    fn a_view_refuses_edits_until_it_is_unlocked() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (a TEXT); INSERT INTO t VALUES ('x');
             CREATE VIEW v AS SELECT a FROM t;",
        )
        .unwrap();
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        // Select the view rather than the table.
        let at = app
            .sqlite()
            .unwrap()
            .tables
            .iter()
            .position(|t| t == "v")
            .unwrap();
        app.select_table(at);
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));

        press(&mut app, 'i');
        assert_eq!(app.screen, super::Screen::Main, "the form did not open");
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "is a view"), "{rows:?}");

        press(&mut app, 'L');
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "INSTEAD OF"), "{rows:?}");
        let _ = std::fs::remove_file(path);
    }

    // ----- editable pragmas --------------------------------------------------

    /// `P` opens the pragmas, Space cycles one, and what the screen shows is what
    /// the database reported afterwards — not what was asked for.
    #[test]
    fn the_pragma_screen_writes_and_reads_back() {
        let (mut app, path) = sqlite_app();
        press(&mut app, 'P');
        assert_eq!(app.screen, super::Screen::Pragmas);
        let rows = frame_rows(&mut app, 100, 24);
        assert!(contains(&rows, "foreign_keys"), "{rows:?}");
        assert!(contains(&rows, "journal_mode"), "{rows:?}");

        // foreign_keys is a flag. Which way it starts depends on how SQLite was
        // built, so the test flips it rather than assuming a default.
        let at = crate::pragmas::EDITABLE
            .iter()
            .position(|s| s.name == "foreign_keys")
            .unwrap();
        let before = app
            .pragmas
            .as_ref()
            .unwrap()
            .value("foreign_keys")
            .unwrap()
            .to_string();
        let want = if before == "1" { "0" } else { "1" };
        for _ in 0..at {
            app.on_key(KeyEvent::from(KeyCode::Down));
        }
        press(&mut app, ' ');
        assert_eq!(
            app.pragmas.as_ref().unwrap().value("foreign_keys"),
            Some(want)
        );
        assert_eq!(
            app.sqlite().unwrap().pragma("foreign_keys").as_deref(),
            Some(want),
            "the database took it, not just the form"
        );

        // The screen shows the word, not the number.
        let word = if want == "1" { "on" } else { "off" };
        let rows = frame_rows(&mut app, 100, 24);
        assert!(
            rows.iter()
                .any(|r| r.contains("foreign_keys") && r.contains(word)),
            "{rows:?}"
        );
        press(&mut app, 'P');
        assert_eq!(app.screen, super::Screen::Main);
        let _ = std::fs::remove_file(path);
    }

    /// A pragma SQLite takes but does not apply must not be reported as applied.
    /// `max_page_count` cannot go below the pages already in use, so asking for
    /// one page on a database that has more comes back clamped.
    #[test]
    fn a_pragma_that_sqlite_clamps_is_reported_as_it_landed() {
        let (mut app, path) = sqlite_app();
        press(&mut app, 'P');
        let at = crate::pragmas::EDITABLE
            .iter()
            .position(|s| s.name == "max_page_count")
            .unwrap();
        for _ in 0..at {
            app.on_key(KeyEvent::from(KeyCode::Down));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        press(&mut app, '1');
        app.on_key(KeyEvent::from(KeyCode::Enter));

        let landed = app
            .pragmas
            .as_ref()
            .unwrap()
            .value("max_page_count")
            .unwrap()
            .to_string();
        assert_ne!(landed, "1", "the database already uses more than one page");
        let rows = frame_rows(&mut app, 100, 24);
        assert!(contains(&rows, "stayed"), "the status says so: {rows:?}");
        let _ = std::fs::remove_file(path);
    }

    /// A pragma cannot be applied over an open savepoint, so it says what to do
    /// instead of appearing to work.
    #[test]
    fn a_pragma_refuses_while_changes_are_unwritten() {
        let (mut app, path) = sqlite_app();
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        press(&mut app, 'e');
        press(&mut app, 'q');
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.has_pending());

        press(&mut app, 'P');
        press(&mut app, ' ');
        let rows = frame_rows(&mut app, 100, 24);
        assert!(contains(&rows, "unwritten changes"), "{rows:?}");
        let _ = std::fs::remove_file(path);
    }

    // ----- the edit buffer ---------------------------------------------------

    /// The grid reads its pages off the reader threads, which cannot see this
    /// connection's open savepoint — so while a change is unwritten the page has
    /// to come from the store itself, or the cell would still show its old value.
    #[test]
    fn the_grid_shows_an_edit_that_has_not_been_written() {
        let (mut app, path) = sqlite_app();
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        press(&mut app, 'e');
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "EDITED".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));

        assert!(app.has_pending(), "the edit is buffered");
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "EDITED"), "{rows:?}");
        assert!(
            contains(&rows, "unwritten changes"),
            "the status line says so: {rows:?}"
        );
        // And the file still holds the old value.
        let other = rusqlite::Connection::open(&path).unwrap();
        let v: String = other
            .query_row("SELECT a FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "x");
        drop(other);

        press(&mut app, 'W');
        assert!(!app.has_pending());
        await_queries(&mut app);
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "EDITED"), "still there after the write");
        assert!(!contains(&rows, "unwritten changes"));
        let other = rusqlite::Connection::open(&path).unwrap();
        let v: String = other
            .query_row("SELECT a FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "EDITED");
        drop(other);
        let _ = std::fs::remove_file(path);
    }

    /// `R` throws the unwritten edits away and the grid goes back to what the
    /// file holds.
    #[test]
    fn r_reverts_the_grid_to_what_the_file_holds() {
        let (mut app, path) = sqlite_app();
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        press(&mut app, 'e');
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "GONE".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.has_pending());
        assert!(contains(&frame_rows(&mut app, 90, 12), "GONE"));

        press(&mut app, 'R');
        assert!(!app.has_pending());
        await_queries(&mut app);
        let rows = frame_rows(&mut app, 90, 12);
        assert!(!contains(&rows, "GONE"), "the edit is gone: {rows:?}");
        assert!(contains(&rows, "reverted"));
        let _ = std::fs::remove_file(path);
    }

    /// Closing the store rolls its savepoint back, so leaving with unwritten
    /// changes asks first — `w` writes and goes, `r` discards and goes, anything
    /// else stays.
    #[test]
    fn leaving_with_unwritten_changes_asks_first() {
        let (mut app, path) = sqlite_app();
        await_queries(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        press(&mut app, 'e');
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "kept".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));

        press(&mut app, 'q');
        assert!(!app.quit, "the quit was held");
        assert!(matches!(app.mode, super::Mode::ConfirmClose(_)));
        let rows = frame_rows(&mut app, 90, 12);
        assert!(contains(&rows, "unwritten changes"), "{rows:?}");

        // Anything else stays in the file.
        press(&mut app, 'x');
        assert!(!app.quit);
        assert!(app.has_pending());

        press(&mut app, 'q');
        press(&mut app, 'w');
        assert!(app.quit, "w writes and leaves");
        assert!(!app.has_pending());
        let other = rusqlite::Connection::open(&path).unwrap();
        let v: String = other
            .query_row("SELECT a FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "kept");
        drop(other);
        let _ = std::fs::remove_file(path);
    }

    // ----- schema screen and designers --------------------------------------

    /// The whole path DB Browser's Edit Table Definition covers, driven by keys:
    /// schema screen → designer → a changed column type → written back. The type
    /// change is the case `ALTER TABLE` cannot express, so this also proves the
    /// rebuild runs and keeps the row.
    #[test]
    fn the_designer_writes_a_changed_column_type_through_a_rebuild() {
        let (mut app, path) = sqlite_app();
        press(&mut app, 'S');
        assert_eq!(app.screen, super::Screen::Schema);
        assert_eq!(app.schema[app.schema_idx].1, "t");

        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.screen, super::Screen::TableDesign);

        // Column 1, field 1 is its type. Clear it and type a new one.
        app.on_key(KeyEvent::from(KeyCode::Right));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "INTEGER".chars() {
            press(&mut app, c);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        press(&mut app, 'W');

        assert_eq!(app.screen, super::Screen::Schema, "the designer closed");
        let def = app.sqlite().unwrap().table_def("t").unwrap();
        assert_eq!(def.columns[0].ty, "INTEGER");
        // The row survived the rebuild.
        let n: i64 = app.sqlite().unwrap().count_exact("t", "").unwrap();
        assert_eq!(n, 1);
        let _ = std::fs::remove_file(path);
    }

    /// Esc anywhere in the designer leaves the database alone.
    #[test]
    fn cancelling_the_designer_writes_nothing() {
        let (mut app, path) = sqlite_app();
        press(&mut app, 'S');
        app.on_key(KeyEvent::from(KeyCode::Enter));
        press(&mut app, 'd'); // drop the column from the definition
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.screen, super::Screen::Schema);
        let def = app.sqlite().unwrap().table_def("t").unwrap();
        assert_eq!(def.columns.len(), 3, "the table still has every column");
        let _ = std::fs::remove_file(path);
    }

    /// `d` on the schema screen asks before it drops, and only `y` goes through.
    #[test]
    fn dropping_an_object_asks_first() {
        let (mut app, path) = sqlite_app();
        press(&mut app, 'S');
        press(&mut app, 'd');
        assert!(matches!(app.mode, super::Mode::ConfirmDrop(_, _)));
        let rows = frame_rows(&mut app, 90, 20);
        assert!(contains(&rows, "DROP TABLE t"), "{rows:?}");

        press(&mut app, 'n');
        assert!(app.sqlite().unwrap().object_sql("t").unwrap().is_some());

        press(&mut app, 'd');
        press(&mut app, 'y');
        assert!(
            app.sqlite().unwrap().object_sql("t").unwrap().is_none(),
            "confirmed drop went through"
        );
        assert!(app.schema.is_empty(), "the object list was reloaded");
        let _ = std::fs::remove_file(path);
    }

    /// `i` on the schema screen builds an index on the selected table, and the
    /// designer's own keys shape it.
    #[test]
    fn the_index_designer_creates_an_index_on_the_selected_table() {
        let (mut app, path) = sqlite_app();
        press(&mut app, 'S');
        press(&mut app, 'i');
        assert_eq!(app.screen, super::Screen::IndexDesign);
        // Row 2 is UNIQUE; turn it on, then write.
        app.on_key(KeyEvent::from(KeyCode::Down));
        app.on_key(KeyEvent::from(KeyCode::Down));
        press(&mut app, ' ');
        press(&mut app, 'W');
        assert_eq!(app.screen, super::Screen::Schema);
        let idx = app.sqlite().unwrap().index_def("idx_t").unwrap();
        assert!(idx.unique);
        assert_eq!(idx.table, "t");
        assert_eq!(idx.columns[0].expr, "a");
        let _ = std::fs::remove_file(path);
    }

    /// A definition SQLite would reject is reported in the status line, and the
    /// designer stays open on it instead of closing over a failed write.
    #[test]
    fn an_invalid_definition_keeps_the_designer_open() {
        let (mut app, path) = sqlite_app();
        press(&mut app, 'S');
        app.on_key(KeyEvent::from(KeyCode::Enter));
        // Empty the first column's name, then try to write.
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        press(&mut app, 'W');
        assert_eq!(app.screen, super::Screen::TableDesign, "still open");
        let rows = frame_rows(&mut app, 90, 24);
        assert!(contains(&rows, "column 1 needs a name"), "{rows:?}");
        let _ = std::fs::remove_file(path);
    }

    /// The schema screen scrolls to whichever object is selected, however long
    /// the statements above it are.
    #[test]
    fn the_schema_screen_follows_its_selection() {
        let path = scratch("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        // Statements long enough that the last object is far off the first page.
        for i in 0..8 {
            conn.execute_batch(&format!(
                "CREATE TABLE t{i} (\n a TEXT,\n b TEXT,\n c TEXT,\n d TEXT,\n e TEXT\n)"
            ))
            .unwrap();
        }
        drop(conn);
        let store = Store::Sqlite(SqliteStore::open(&path).unwrap());
        let mut app = App::with_theme(store, Theme::from_name(ThemeName::NeonSprawl));
        press(&mut app, 'S');
        press(&mut app, 'G');
        assert_eq!(app.schema_idx, 7);
        let rows = frame_rows(&mut app, 80, 20);
        assert!(contains(&rows, "▸ table  t7"), "{rows:?}");
        press(&mut app, 'g');
        let rows = frame_rows(&mut app, 80, 20);
        assert!(contains(&rows, "▸ table  t0"), "{rows:?}");
        let _ = std::fs::remove_file(path);
    }
}
