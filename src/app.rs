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

use std::path::PathBuf;

use crate::formats::{self, Decoded, FormatKind};
use crate::hexedit::{self, HexEdit};
use crate::mru::{self, Entry};
use crate::overlay::{HelpCtx, Overlays};
use crate::rkyv_inspect::RkyvStore;
use crate::sqlite::{RowsView, Sort, SqliteStore};
use crate::store::{Kind, Store};
use crate::theme::{Theme, ThemeName};

/// How many rows per SQLite page.
const PAGE: i64 = 500;
/// Minimum length for an extracted rkyv string run.
const MIN_STRING: usize = 4;
/// Idle wake-up interval: how often the loop redraws with no input pending, so a
/// toast dismisses itself on time (iftoprs's `event::poll` tick).
const TICK: std::time::Duration = std::time::Duration::from_millis(250);

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
    /// A raw SQL command line (`:`); buffer holds the statement.
    Command(String),
    /// A `/` search prompt; buffer holds the pattern being typed.
    Search(String),
    /// Adding a new rkyv record; buffer holds the key being typed.
    AddRecord(String),
    /// Renaming a rkyv record's key; buffer holds the new key.
    RenameRecord(String),
    /// Confirm a destructive action (delete row).
    ConfirmDelete,
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
    /// SQLite schema (CREATE statements) view.
    Schema,
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
    /// Set by `o`: leave the app loop and show the file picker again.
    reopen: bool,
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
}

impl App {
    /// `theme_override` is `--theme`: it wins over the saved preference (and
    /// over a saved custom palette) for this run only.
    pub fn new(store: Store, theme_override: Option<ThemeName>) -> Self {
        let prefs = crate::prefs::load();
        let theme = match (theme_override, prefs.custom) {
            (Some(name), _) => Theme::from_name(name),
            (None, Some(c)) => Theme::from_palette(prefs.theme, c),
            (None, None) => Theme::from_name(prefs.theme),
        };
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
            reopen: false,
            search_origin: None,
            filter: String::new(),
            strings_truncated: false,
            strings_scanned: 0,
            decoding: None,
            page_rows: 10,
            ov: Overlays::new(theme),
        };
        app.init();
        app
    }

    /// Leave the open file and ask for the picker again (`o`, or `Esc` on the
    /// first level).
    fn back_to_files(&mut self) {
        self.reopen = true;
        self.quit = true;
    }

    /// Which key sections the help overlay lists for what is on screen.
    fn help_ctx(&self) -> HelpCtx {
        if self.screen == Screen::HexEdit {
            return HelpCtx::HexEdit;
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
            Cmd(String),
            Search(String),
            Add(String),
            Rename(String),
            Confirm,
            None,
        }
        let modal = match &self.mode {
            Mode::EditCell(buf) => Modal::Edit(buf.clone()),
            Mode::Command(buf) => Modal::Cmd(buf.clone()),
            Mode::Search(buf) => Modal::Search(buf.clone()),
            Mode::AddRecord(buf) => Modal::Add(buf.clone()),
            Mode::RenameRecord(buf) => Modal::Rename(buf.clone()),
            Mode::ConfirmDelete => Modal::Confirm,
            Mode::Normal => Modal::None,
        };
        match modal {
            Modal::Edit(buf) => {
                return self.key_input(key, buf, Mode::EditCell, App::commit_edit_cell)
            }
            Modal::Cmd(buf) => return self.key_input(key, buf, Mode::Command, App::commit_command),
            Modal::Search(buf) => {
                return self.key_input(key, buf, Mode::Search, App::commit_search)
            }
            Modal::Add(buf) => {
                return self.key_input(key, buf, Mode::AddRecord, App::commit_add_record)
            }
            Modal::Rename(buf) => {
                return self.key_input(key, buf, Mode::RenameRecord, App::commit_rename_record)
            }
            Modal::Confirm => return self.key_confirm_delete(code),
            Modal::None => {}
        }

        // The overlay openers (`h`/`?` help, `c` chooser, `C` editor) work from
        // every screen, exactly as on the recent-files picker — except inside the
        // hex editor, where those keys are motions and data.
        if self.screen != Screen::HexEdit && self.ov.on_key(code) {
            return;
        }

        // `o` goes back to the file picker from any screen but the hex editor,
        // where it inserts a byte.
        if code == KeyCode::Char('o') && self.screen != Screen::HexEdit {
            self.back_to_files();
            return;
        }

        match self.screen {
            // The hex editor is modal: it owns every key, including `h` and `c`,
            // because those are its own motions and data.
            Screen::HexEdit => return self.key_hex(key),
            Screen::Detail => return self.key_detail(code),
            Screen::Schema => return self.key_schema(code),
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
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc | KeyCode::Enter => {
                self.screen = Screen::Main;
                self.detail_scroll = 0;
            }
            KeyCode::Char('v') => self.value_render = self.value_render.next(),
            KeyCode::Char('y') => self.copy_detail_value(),
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

    fn key_schema(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc | KeyCode::Char('S') => {
                self.screen = Screen::Main;
                self.schema_scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => self.schema_scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => {
                self.schema_scroll = self.schema_scroll.saturating_sub(1)
            }
            KeyCode::Char('g') => self.schema_scroll = 0,
            KeyCode::PageDown => self.schema_scroll += self.page_step(),
            KeyCode::PageUp => {
                self.schema_scroll = self.schema_scroll.saturating_sub(self.page_step())
            }
            _ => {}
        }
    }

    /// Enter the detail screen for the current SQLite row or rkyv record.
    fn enter_detail(&mut self) {
        self.detail_scroll = 0;
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
                            .and_then(|s| s.cell_bytes(&t, rid, &col).ok())
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
                self.screen = Screen::Detail;
            }
            Store::Rkyv(_) => {
                if let Some(rec) = self
                    .decoded
                    .as_ref()
                    .and_then(|d| d.records.get(self.record_idx))
                {
                    self.detail_value = rec.value.clone();
                    self.screen = Screen::Detail;
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
        let total = self.rows.as_ref().map(|r| r.total).unwrap_or(0);
        let view = match self.sqlite().unwrap().rows(
            &table,
            total.max(1),
            0,
            self.sort.as_ref(),
            &self.filter,
        ) {
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

        // Ctrl-f / Ctrl-b page forward / back (vim page motions).
        if ctrl {
            match code {
                KeyCode::Char('f') => self.page_sqlite(true),
                KeyCode::Char('b') => self.page_sqlite(false),
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
            KeyCode::Char('q') => self.quit = true,
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
                    self.col_idx = self.col_idx.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                if self.focus == Focus::Right {
                    if let Some(r) = &self.rows {
                        if self.col_idx + 1 < r.columns.len() {
                            self.col_idx += 1;
                        }
                    }
                }
            }
            KeyCode::Enter => match self.focus {
                Focus::Left => self.focus = Focus::Right,
                Focus::Right => self.enter_detail(),
            },
            KeyCode::PageDown => self.page_sqlite(true),
            KeyCode::PageUp => self.page_sqlite(false),
            KeyCode::Char('e') => self.begin_edit_cell(),
            KeyCode::Char('a') => self.insert_row(),
            KeyCode::Char('d') => {
                if self.focus == Focus::Right && self.current_rowid().is_some() {
                    self.mode = Mode::ConfirmDelete;
                }
            }
            KeyCode::Char('S') => self.open_schema(),
            // Sorting: `s` toggles the cursor column (asc → desc → off), and
            // `<`/`>` walk the sort across columns keeping the direction.
            KeyCode::Char('s') => self.sort_by_current_column(),
            KeyCode::Char('<') => self.sort_shift_column(false),
            KeyCode::Char('>') => self.sort_shift_column(true),
            KeyCode::Char('x') => self.export_current(),
            KeyCode::Char('y') => self.copy_sqlite_cell(),
            KeyCode::Char(':') => self.open_modal(Mode::Command(String::new())),
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
            KeyCode::Char('q') => self.quit = true,
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
            KeyCode::Left => cur = input_left(&buf, cur),
            KeyCode::Right => cur = input_right(&buf, cur),
            KeyCode::Home => cur = 0,
            KeyCode::End => cur = buf.len(),
            KeyCode::Char('a') if ctrl => cur = 0,
            KeyCode::Char('e') if ctrl => cur = buf.len(),
            KeyCode::Char('b') if ctrl => cur = input_left(&buf, cur),
            KeyCode::Char('f') if ctrl => cur = input_right(&buf, cur),
            KeyCode::Char('w') if ctrl => cur = input_delete_word(&mut buf, cur),
            KeyCode::Char('u') if ctrl => {
                buf.drain(..cur);
                cur = 0;
            }
            KeyCode::Char('k') if ctrl => buf.truncate(cur),
            KeyCode::Backspace => {
                if cur > 0 {
                    let p = input_left(&buf, cur);
                    buf.drain(p..cur);
                    cur = p;
                }
            }
            KeyCode::Delete => {
                if cur < buf.len() {
                    let n = input_right(&buf, cur);
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
            Mode::EditCell(s)
            | Mode::Command(s)
            | Mode::Search(s)
            | Mode::AddRecord(s)
            | Mode::RenameRecord(s) => s.len(),
            _ => 0,
        };
        self.mode = mode;
    }

    fn commit_command(&mut self, sql: &str) {
        self.run_sql(sql);
    }

    fn commit_search(&mut self, pattern: &str) {
        self.search = pattern.to_string();
        self.search_next(true);
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

    /// (kind, shard bytes) for the current rkyv archive, if decoded.
    fn rkyv_kind_bytes(&self) -> Option<(FormatKind, Vec<u8>)> {
        let kind = self.decoded.as_ref().map(|d| d.kind)?;
        match &self.store {
            Store::Rkyv(r) => Some((kind, r.bytes.clone())),
            _ => None,
        }
    }

    /// (display key, del_key, kind, shard bytes) for the selected record.
    fn rkyv_ctx(&self) -> Option<(String, String, FormatKind, Vec<u8>)> {
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
            r.bytes = new_bytes;
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
                self.screen = Screen::Main;
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

    fn load_table(&mut self) {
        let (table, res) = match (self.current_table(), self.sqlite()) {
            (Some(t), Some(s)) => {
                let r = s.rows(&t, PAGE, self.page_offset, self.sort.as_ref(), &self.filter);
                (t, r)
            }
            _ => return,
        };
        match res {
            Ok(v) => {
                self.rows = Some(v);
                if self.row_idx >= self.rows.as_ref().map(|r| r.rows.len()).unwrap_or(0) {
                    self.row_idx = 0;
                }
            }
            Err(e) => self.status = format!("load {}: {}", table, e),
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
        let cur = self
            .rows
            .as_ref()
            .and_then(|r| r.rows.get(self.row_idx))
            .and_then(|row| row.get(self.col_idx))
            .cloned()
            .unwrap_or_default();
        if self.current_rowid().is_some() {
            self.open_modal(Mode::EditCell(cur));
        } else {
            self.notify("row has no rowid — cannot edit (WITHOUT ROWID table)");
        }
    }

    fn commit_edit_cell(&mut self, val: &str) {
        let (table, rowid, col) = match (
            self.current_table(),
            self.current_rowid(),
            self.rows
                .as_ref()
                .and_then(|r| r.columns.get(self.col_idx).cloned()),
        ) {
            (Some(t), Some(rid), Some(c)) => (t, rid, c),
            _ => return,
        };
        let res = self.sqlite().unwrap().update_cell(&table, rowid, &col, val);
        match res {
            Ok(()) => {
                self.notify(format!("updated {}.{}", table, col));
                self.load_table();
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
                self.load_table();
            }
            Err(e) => self.status = format!("insert failed: {}", e),
        }
    }

    fn delete_current_row(&mut self) {
        let (table, rowid) = match (self.current_table(), self.current_rowid()) {
            (Some(t), Some(r)) => (t, r),
            _ => return,
        };
        match self.sqlite().unwrap().delete_row(&table, rowid) {
            Ok(()) => {
                self.notify(format!("deleted row {} from {}", rowid, table));
                self.row_idx = self.row_idx.saturating_sub(1);
                self.load_table();
            }
            Err(e) => self.status = format!("delete failed: {}", e),
        }
    }

    fn run_sql(&mut self, sql: &str) {
        if sql.trim().is_empty() {
            return;
        }
        match self.sqlite().unwrap().exec(sql) {
            Ok(n) => {
                self.notify(format!("ok, {} row(s) affected", n));
                self.load_table();
            }
            Err(e) => self.status = format!("sql error: {}", e),
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
            Focus::Right => {
                let total = self.rows.as_ref().map(|r| r.total).unwrap_or(0);
                if total > 0 {
                    self.page_offset = ((total - 1) / PAGE) * PAGE;
                    self.load_table();
                    let last = self.rows.as_ref().map(|r| r.rows.len()).unwrap_or(0);
                    self.row_idx = last.saturating_sub(1);
                }
            }
        }
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
                match find_next(tables.len(), self.table_idx, forward, |i| {
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
        let outcome: Result<Option<(i64, i64)>, String> = {
            let sort = self.sort.clone();
            let s = self.sqlite().unwrap();
            // From the selected row, else from the edge the scan comes in from.
            let first = match self.current_rowid() {
                Some(from) => {
                    s.find_row(&table, &columns, &self.search, from, forward, sort.as_ref())
                }
                None => s.find_row_edge(&table, &columns, &self.search, forward, sort.as_ref()),
            };
            let rid = match first {
                Err(e) => Err(e.to_string()),
                Ok(Some(r)) => Ok(Some(r)),
                // Nothing ahead: wrap to the first/last match in display order.
                Ok(None) => s
                    .find_row_edge(&table, &columns, &self.search, forward, sort.as_ref())
                    .map_err(|e| e.to_string()),
            };
            match rid {
                Err(e) => Err(e),
                Ok(None) => Ok(None),
                Ok(Some(r)) => Ok(Some((
                    r,
                    s.rowid_ordinal(&table, r, sort.as_ref()).unwrap_or(1),
                ))),
            }
        };

        match outcome {
            Ok(Some((_rid, ord))) => {
                let idx0 = (ord - 1).max(0);
                self.page_offset = (idx0 / PAGE) * PAGE;
                self.load_table();
                self.row_idx = (idx0 - self.page_offset) as usize;
                let total = self.rows.as_ref().map(|r| r.total).unwrap_or(0);
                self.notify(format!("/{}  (row {} of {})", self.search, ord, total));
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
                match find_next(keys.len(), self.record_idx, forward, |i| {
                    keys[i].contains(&term)
                }) {
                    Some(i) => self.record_idx = i,
                    None => self.status = format!("not found: {}", self.search),
                }
            }
            RkyvView::Strings => {
                match find_next(self.strings.len(), self.string_idx, forward, |i| {
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
            Screen::Schema => self.render_schema(f, outer[0]),
            Screen::Main => match &self.store {
                Store::Sqlite(_) => self.render_sqlite(f, outer[0]),
                Store::Rkyv(_) => self.render_rkyv(f, outer[0]),
            },
        }
        self.render_status(f, outer[1]);

        // Modal overlays.
        match &self.mode {
            Mode::EditCell(buf) => self.render_input(f, "edit cell (Enter=save, Esc=cancel)", buf),
            Mode::Command(buf) => self.render_input(f, "SQL (Enter=run, Esc=cancel)", buf),
            Mode::Search(buf) => self.render_input(f, "search / (Enter, Esc)", buf),
            Mode::AddRecord(buf) => self.render_input(f, "new record key (Enter=add, Esc)", buf),
            Mode::RenameRecord(buf) => self.render_input(f, "rename key to (Enter, Esc)", buf),
            Mode::ConfirmDelete => {
                let what = match self.store {
                    Store::Sqlite(_) => "row",
                    Store::Rkyv(_) => "record (rewrites the cache file)",
                };
                self.render_input(f, &format!("delete this {}? (y = yes, any = no)", what), "")
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

    fn render_hex(&mut self, f: &mut Frame, area: Rect) {
        let theme = self.ov.theme;
        self.hex_area = area;
        if let Some(ed) = self.hex.as_mut() {
            ed.render(f, area, &theme);
        }
    }

    fn render_schema(&self, f: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        for (ty, name, sql) in &self.schema {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<6}", ty), Style::default().fg(self.ov.theme.alt)),
                Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
            ]));
            for l in sql.lines() {
                lines.push(Line::from(Span::styled(
                    format!("    {}", l),
                    Style::default().fg(self.ov.theme.dim),
                )));
            }
            lines.push(Line::from(""));
        }
        let height = area.height.saturating_sub(2) as usize;
        let visible: Vec<Line> = lines
            .into_iter()
            .skip(self.schema_scroll)
            .take(height)
            .collect();
        f.render_widget(
            Paragraph::new(visible).block(Block::default().borders(Borders::ALL).title(format!(
                " schema — {} objects (j/k scroll · Esc back) ",
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
        let items: Vec<ListItem> = visible
            .iter()
            .map(|&i| ListItem::new(s.tables[i].clone()))
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
                let total = self.rows.as_ref().map(|r| r.total).unwrap_or(0);
                let sorted = match &self.sort {
                    Some(s) => format!(" — sorted {} {}", s.column, arrow(s.desc)),
                    None => String::new(),
                };
                format!(
                    " {} — rows {}..{} of {}{} ",
                    t,
                    self.page_offset,
                    self.page_offset + self.rows.as_ref().map(|r| r.rows.len() as i64).unwrap_or(0),
                    total,
                    sorted
                )
            }
            None => " (no table) ".into(),
        };

        if let Some(rv) = &self.rows {
            let header = Row::new(
                rv.columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let st = if i == self.col_idx && self.focus == Focus::Right {
                            Style::default()
                                .fg(self.ov.theme.accent)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().add_modifier(Modifier::BOLD)
                        };
                        // Mark the sorted column in its header.
                        let label = match &self.sort {
                            Some(s) if s.column == *c => format!("{} {}", c, arrow(s.desc)),
                            _ => c.clone(),
                        };
                        Cell::from(label).style(st)
                    })
                    .collect::<Vec<_>>(),
            );
            let body = rv.rows.iter().map(|row| {
                Row::new(
                    row.iter()
                        .map(|c| Cell::from(truncate(c, 40)))
                        .collect::<Vec<_>>(),
                )
            });
            let widths: Vec<Constraint> =
                rv.columns.iter().map(|_| Constraint::Length(20)).collect();
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
        let items: Vec<ListItem> = visible
            .iter()
            .map(|&i| ListItem::new(truncate(&d.records[i].key, 60)))
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
            let rows = area.height.saturating_sub(6) as usize;
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
        let p = Paragraph::new(self.status.clone())
            .style(Style::default().fg(Color::Black).bg(Color::Gray));
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

/// Merge scan hits into the list, dropping any path already present (a recent
/// file the scan also found stays a recent file, keeping its age column).
fn merge_hits(choices: &mut Vec<Choice>, hits: Vec<crate::scan::Hit>) {
    for hit in hits {
        let dup = choices.iter().any(|c| {
            c.path == hit.path
                || (c.path.canonicalize().ok() == hit.path.canonicalize().ok()
                    && c.path.canonicalize().is_ok())
        });
        if !dup {
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
    pub theme_override: Option<ThemeName>,
    /// Rows restored from the saved scan (appdata), shown immediately.
    pub cached: Vec<crate::scan::Hit>,
    /// How old those rows are, for the title.
    pub cache_age: Option<std::time::Duration>,
    /// A walk already in progress, when the cache was missing or stale.
    pub scan: Option<crate::scan::Scan>,
    /// Roots to walk when the user asks for a rescan with `r`.
    pub roots: Vec<crate::scan::Root>,
    /// Whether a finished walk may be written to appdata (not for `--scan`,
    /// whose roots are not the default set).
    pub persist: bool,
}

pub fn pick_mru(terminal: &mut DefaultTerminal, mut p: Picker<'_>) -> Result<Option<PathBuf>> {
    let entries = p.recent;
    let theme_override = p.theme_override;
    let mut scan = p.scan.take();
    let mut cache_age = p.cache_age;
    // Hits are kept as well as merged so a finished walk can be saved.
    let mut scanned: Vec<crate::scan::Hit> = std::mem::take(&mut p.cached);
    let mut idx = 0usize;
    let mut pending_g = false;
    let mut search = String::new();
    let mut searching = false;
    // Where `/` was pressed, so the incremental match is measured from there.
    let mut search_origin = 0usize;
    // First row the list drew last frame, for mapping a click to an entry.
    let mut list_offset = 0usize;
    // Recent files first, then whatever the scan turns up.
    let mut choices: Vec<Choice> = entries.iter().map(Choice::from_entry).collect();
    merge_hits(&mut choices, scanned.clone());
    // The picker carries the same overlay layer as the main screens, so `h`,
    // `c` and `C` work here too — and a scheme picked here is the one the file
    // opens with.
    let prefs = crate::prefs::load();
    let mut ov = Overlays::new(match (theme_override, prefs.custom) {
        (Some(name), _) => Theme::from_name(name),
        (None, Some(c)) => Theme::from_palette(prefs.theme, c),
        (None, None) => Theme::from_name(prefs.theme),
    });
    loop {
        let query = if searching {
            Some(search.as_str())
        } else {
            None
        };
        // Pull in whatever the scan thread produced since the last frame, and
        // save the finished list so the next start does not walk again.
        let scanning = match scan.as_mut() {
            Some(sc) => {
                let hits = sc.drain();
                scanned.extend(hits.iter().cloned());
                merge_hits(&mut choices, hits);
                if sc.running {
                    Some(sc.found)
                } else {
                    if p.persist {
                        crate::scan::save_cache(&scanned);
                        cache_age = Some(std::time::Duration::ZERO);
                    }
                    scan = None;
                    None
                }
            }
            None => None,
        };
        // List height for paging: the body minus its borders.
        let page = terminal
            .size()
            .map(|s| s.height.saturating_sub(3) as usize)
            .unwrap_or(10)
            .max(1);
        terminal.draw(|f| {
            list_offset = render_picker(f, &choices, idx, query, scanning, cache_age, &ov.theme);
            ov.render(f, HelpCtx::Picker);
        })?;
        if !event::poll(TICK)? {
            ov.expire_toast();
            continue;
        }
        let ev = event::read()?;
        ov.expire_toast();
        // Mouse: wheel moves the selection, a click opens the entry under it.
        if let Event::Mouse(m) = ev {
            if ov.on_mouse(m) {
                continue;
            }
            match m.kind {
                MouseEventKind::ScrollDown => {
                    if idx + 1 < choices.len() {
                        idx += 1;
                    }
                }
                MouseEventKind::ScrollUp => idx = idx.saturating_sub(1),
                MouseEventKind::Down(_) => {
                    // The list starts one row below the block's top border, and
                    // is scrolled by `offset` — without that a click in a
                    // scrolled list opened the wrong file. Rows below the last
                    // entry (and the status line) select nothing.
                    let row = (m.row as usize).saturating_sub(1);
                    let clicked = row + list_offset;
                    if row < page && clicked < choices.len() {
                        if let Some(sc) = &scan {
                            sc.cancel();
                        }
                        return Ok(Some(choices[clicked].path.clone()));
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
                    KeyCode::Char('f') => idx = (idx + page).min(choices.len().saturating_sub(1)),
                    KeyCode::Char('b') => idx = idx.saturating_sub(page),
                    _ => {}
                }
                continue;
            }

            // Search-input capture takes priority. The selection follows the
            // pattern as it is typed, from where `/` was pressed.
            if searching {
                match key.code {
                    KeyCode::Esc => {
                        searching = false;
                        search.clear();
                        idx = search_origin;
                    }
                    KeyCode::Enter => searching = false,
                    KeyCode::Backspace => {
                        search.pop();
                    }
                    KeyCode::Char(c) => search.push(c),
                    _ => {}
                }
                if searching {
                    idx = if search.is_empty() {
                        search_origin
                    } else {
                        picker_find_from(&choices, search_origin, &search).unwrap_or(search_origin)
                    };
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
                    idx = 0;
                } else {
                    pending_g = true;
                }
                continue;
            }
            pending_g = false;
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    if let Some(sc) = &scan {
                        sc.cancel();
                    }
                    return Ok(None);
                }
                KeyCode::Char('/') => {
                    searching = true;
                    search.clear();
                    search_origin = idx;
                }
                KeyCode::Char('n') => {
                    if let Some(i) = picker_find(&choices, idx, true, &search) {
                        idx = i;
                    }
                }
                KeyCode::Char('N') => {
                    if let Some(i) = picker_find(&choices, idx, false, &search) {
                        idx = i;
                    }
                }
                // `r` walks again (keeping the rows on screen until new ones
                // arrive); `R` also drops the saved scan first.
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    if key.code == KeyCode::Char('R') {
                        crate::scan::clear_cache();
                    }
                    if let Some(sc) = &scan {
                        sc.cancel();
                    }
                    scanned.clear();
                    choices.retain(|c| c.opened.is_some());
                    idx = 0;
                    cache_age = None;
                    scan = Some(crate::scan::spawn(p.roots.clone()));
                    ov.toast("rescanning");
                }
                KeyCode::Char('G') => idx = choices.len().saturating_sub(1),
                // A screenful, from the height this frame was drawn at.
                KeyCode::PageDown => {
                    idx = (idx + page).min(choices.len().saturating_sub(1));
                }
                KeyCode::PageUp => idx = idx.saturating_sub(page),
                KeyCode::Up | KeyCode::Char('k') => idx = idx.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    if idx + 1 < choices.len() {
                        idx += 1;
                    }
                }
                KeyCode::Enter => {
                    if let Some(c) = choices.get(idx) {
                        // Nothing more to walk once a file is chosen.
                        if let Some(sc) = &scan {
                            sc.cancel();
                        }
                        return Ok(Some(c.path.clone()));
                    }
                }
                _ => {}
            }
        }
    }
}

/// First row at or after `from` whose path contains `q`, for the incremental
/// search while typing.
fn picker_find_from(choices: &[Choice], from: usize, q: &str) -> Option<usize> {
    if q.is_empty() {
        return None;
    }
    let ql = q.to_lowercase();
    find_from(choices.len(), from, |i| {
        choices[i]
            .path
            .to_str()
            .map(|p| p.to_lowercase().contains(&ql))
            .unwrap_or(false)
    })
}

/// Find the next/previous picker row whose path contains `q` (the whole path, so
/// a scan hit can be found by its directory as well as its name).
fn picker_find(choices: &[Choice], from: usize, forward: bool, q: &str) -> Option<usize> {
    if q.is_empty() {
        return None;
    }
    let ql = q.to_lowercase();
    find_next(choices.len(), from, forward, |i| {
        choices[i]
            .path
            .to_str()
            .map(|p| p.to_lowercase().contains(&ql))
            .unwrap_or(false)
    })
}

#[allow(clippy::too_many_arguments)]
fn render_picker(
    f: &mut Frame,
    choices: &[Choice],
    idx: usize,
    query: Option<&str>,
    scanning: Option<usize>,
    cache_age: Option<std::time::Duration>,
    t: &Theme,
) -> usize {
    let outer = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
    let mut offset = 0usize;

    if choices.is_empty() {
        let body = match scanning {
            Some(_) => vec![
                Line::from(""),
                Line::from("  Scanning for databases and rkyv shards…"),
            ],
            None => vec![
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
            ],
        };
        let p = Paragraph::new(body).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.accent))
                .title(" zdbview — files "),
        );
        f.render_widget(p, outer[0]);
    } else {
        let items: Vec<ListItem> = choices
            .iter()
            .map(|c| {
                let dir = c.path.parent().and_then(|p| p.to_str()).unwrap_or("");
                let (badge, color) = match c.kind {
                    Kind::Sqlite => ("sqlite", t.primary),
                    Kind::Rkyv => ("rkyv  ", t.alt),
                };
                // Recent files show their age; scanned ones show a marker, so it
                // is obvious which rows came from where.
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
        st.select(Some(idx.min(choices.len().saturating_sub(1))));
        let recent = choices.iter().filter(|c| c.opened.is_some()).count();
        let title = match (scanning, cache_age) {
            (Some(found), _) => format!(
                " zdbview — {} files ({} recent, scanning… {} found) ",
                choices.len(),
                recent,
                found
            ),
            // Saved scans are reused, so say how old the rows are and how to
            // refresh them.
            (None, Some(age)) => format!(
                " zdbview — {} files ({} recent, scan {} · r rescans) ",
                choices.len(),
                recent,
                age_label(age)
            ),
            (None, None) => format!(" zdbview — {} files ({} recent) ", choices.len(), recent),
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

    // Bottom line: the search prompt, else the selected row's format / size.
    let help = match query {
        Some(q) => {
            Paragraph::new(format!("/{}_", q)).style(Style::default().fg(Color::Black).bg(t.accent))
        }
        None => {
            let detail = choices
                .get(idx)
                .and_then(|c| c.format)
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

/// Archives up to this size are decoded inline; bigger ones go to a thread.
/// 12MB takes ~0.8s to validate, 382MB takes ~25s, so the line sits below the
/// point where a person would notice the wait.
const DECODE_INLINE_MAX: usize = 4 * 1024 * 1024;

/// Validate and decode `bytes` on another thread.
fn spawn_decode(bytes: Vec<u8>) -> std::sync::mpsc::Receiver<Option<Decoded>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(formats::try_decode(&bytes));
    });
    rx
}

/// Whether `hay` passes `filter` (case-insensitive substring; empty passes all).
fn filter_passes(filter: &str, hay: &str) -> bool {
    filter.is_empty() || hay.to_lowercase().contains(&filter.to_lowercase())
}

/// First index at or after `from` (wrapping once) for which `pred` holds. Unlike
/// [`find_next`], `from` itself counts — an incremental search must be able to
/// stay where it is as the pattern grows.
fn find_from(len: usize, from: usize, pred: impl Fn(usize) -> bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let start = from.min(len - 1);
    (0..len).map(|step| (start + step) % len).find(|&i| pred(i))
}

/// Find the next index (wrapping) from `from` for which `pred` holds, scanning
/// `forward` or backward. Returns `None` if nothing matches.
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

/// Move the cursor one char left (UTF-8-safe). Ported from iftoprs `FilterState::left`.
fn input_left(buf: &str, cur: usize) -> usize {
    if cur > 0 {
        buf[..cur]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    } else {
        0
    }
}

/// Move the cursor one char right (UTF-8-safe). Ported from iftoprs `FilterState::right`.
fn input_right(buf: &str, cur: usize) -> usize {
    if cur < buf.len() {
        buf[cur..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| cur + i)
            .unwrap_or(buf.len())
    } else {
        buf.len()
    }
}

/// Delete the word before the cursor (Ctrl+W). Ported from iftoprs
/// `FilterState::delete_word` — skips trailing whitespace, then the word,
/// stepping by real UTF-8 widths. Returns the new cursor position.
fn input_delete_word(buf: &mut String, cur: usize) -> usize {
    let s = &buf[..cur];
    let trimmed = s.trim_end();
    let word_start = match trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
    {
        Some((i, c)) => i + c.len_utf8(),
        None => 0,
    };
    buf.drain(word_start..cur);
    word_start
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
fn human_size(n: u64) -> String {
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
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
    use super::{
        find_bytes, find_next, hit, input_delete_word, input_left, input_right, App, Store,
    };
    use crate::mru::Entry;
    use crate::overlay::HelpCtx;
    use crate::rkyv_inspect::RkyvStore;
    use crate::sqlite::SqliteStore;
    use crate::store::Kind;
    use crate::theme::ThemeName;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

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
        App::new(store, Some(ThemeName::NeonSprawl))
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
        (App::new(store, Some(ThemeName::NeonSprawl)), path)
    }

    /// An App over arbitrary binary content.
    fn rkyv_app_with(bytes: &[u8]) -> App {
        let path = scratch("bin");
        std::fs::write(&path, bytes).unwrap();
        let store = Store::Rkyv(RkyvStore::open(&path).unwrap());
        let _ = std::fs::remove_file(&path);
        App::new(store, Some(ThemeName::NeonSprawl))
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
        (App::new(store, Some(ThemeName::NeonSprawl)), path)
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
        (App::new(store, Some(ThemeName::NeonSprawl)), path)
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

    /// Scan hits are ordered by what the tool is for: recognized shards, then
    /// other rkyv archives, then databases — newest first within each group.
    #[test]
    fn scan_hits_are_ranked_before_being_shown() {
        let mut choices: Vec<super::Choice> = Vec::new();
        super::merge_hits(
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
        super::merge_hits(
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
        super::merge_hits(
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
                super::render_picker(f, choices, 0, None, scanning, None, &theme);
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
        let mut app = App::new(store, Some(ThemeName::NeonSprawl));
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
        let mut app = App::new(store, Some(ThemeName::NeonSprawl));
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
        let mut app = App::new(store, Some(ThemeName::NeonSprawl));
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

    #[test]
    fn find_from_is_inclusive_and_wraps() {
        let pred = |i: usize| i == 1 || i == 4;
        assert_eq!(super::find_from(5, 1, pred), Some(1), "from itself counts");
        assert_eq!(super::find_from(5, 2, pred), Some(4));
        assert_eq!(super::find_from(5, 0, pred), Some(1));
        assert_eq!(super::find_from(5, 4, pred), Some(4));
        // Wraps past the end back to the first match.
        assert_eq!(super::find_from(5, 3, |i| i == 0), Some(0));
        assert_eq!(super::find_from(5, 9, pred), Some(4), "clamps a wild start");
        assert_eq!(super::find_from(0, 0, pred), None);
        assert_eq!(super::find_from(5, 0, |_| false), None);
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
        let mut app = App::new(store, Some(ThemeName::NeonSprawl));
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

    /// The detail screen's value pane and the schema view page too.
    #[test]
    fn paging_scrolls_the_detail_and_schema_screens() {
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

        app.on_key(KeyEvent::from(KeyCode::Char('S'))); // schema
        assert_eq!(app.screen, super::Screen::Schema);
        frame_rows(&mut app, 80, 30);
        let step = app.page_rows;
        app.on_key(KeyEvent::from(KeyCode::PageDown));
        assert_eq!(app.schema_scroll, step);
        app.on_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(app.schema_scroll, 0);
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

    /// Rows restored from the saved scan are labelled with their age, so it is
    /// clear the list was not just walked.
    #[test]
    fn picker_titles_a_reused_scan_with_its_age() {
        let theme = crate::theme::Theme::from_name(ThemeName::NeonSprawl);
        let mut choices: Vec<super::Choice> = Vec::new();
        super::merge_hits(
            &mut choices,
            vec![scan_hit("/h/.zshrs/scripts.rkyv", Kind::Rkyv, None, 50, 1)],
        );
        let render = |age: Option<std::time::Duration>| -> Vec<String> {
            let mut term = Terminal::new(TestBackend::new(100, 6)).unwrap();
            term.draw(|f| {
                super::render_picker(f, &choices, 0, None, None, age, &theme);
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
        assert_eq!(input_right(s, 0), 1); // past 'a'
        assert_eq!(input_right(s, 1), 3); // past 'é'
        assert_eq!(input_right(s, 3), 3); // at end, stays
        assert_eq!(input_left(s, 3), 1); // before 'é'
        assert_eq!(input_left(s, 1), 0);
        assert_eq!(input_left(s, 0), 0);
    }

    #[test]
    fn delete_word_skips_trailing_space() {
        let mut s = String::from("foo bar  ");
        let len = s.len();
        let cur = input_delete_word(&mut s, len);
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
}
