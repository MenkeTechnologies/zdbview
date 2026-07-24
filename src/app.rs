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
use crate::mru::{self, Entry};
use crate::rkyv_inspect::RkyvStore;
use crate::sqlite::{RowsView, SqliteStore};
use crate::store::{Kind, Store};
use crate::theme::{Theme, ThemeName};

/// How many rows per SQLite page.
const PAGE: i64 = 500;
/// Minimum length for an extracted rkyv string run.
const MIN_STRING: usize = 4;

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
    /// Editing a rkyv record's value; buffer holds the new value (text, or a
    /// `0x…` hex string for binary).
    EditValue(String),
    /// Renaming a rkyv record's key; buffer holds the new key.
    RenameRecord(String),
    /// Confirm a destructive action (delete row).
    ConfirmDelete,
}

/// The views for a rkyv/binary file. `Records` is only available when the
/// archive was recognized and decoded to key/value.
#[derive(PartialEq, Eq, Clone, Copy)]
enum RkyvView {
    Records,
    Info,
    Strings,
    Hex,
}

/// Top-level screen. Overlaid modals (`Mode`) and the help overlay sit on top.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Screen {
    Main,
    /// Full-screen detail of one row/record with a scrollable value pane.
    Detail,
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
    show_help: bool,
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

    // Theming (ported from iftoprs)
    theme: Theme,
    /// Theme chooser overlay: index into `ThemeName::ALL`, plus the scheme to
    /// restore if the chooser is cancelled.
    show_chooser: bool,
    chooser_idx: usize,
    chooser_saved: ThemeName,
    /// Editor overlay: which of the 6 base colors is selected.
    show_editor: bool,
    editor_slot: usize,
    editor_palette: [u8; 6],
}

impl App {
    pub fn new(store: Store) -> Self {
        let prefs = crate::prefs::load();
        let theme = match prefs.custom {
            Some(c) => Theme::from_palette(prefs.theme, c),
            None => Theme::from_name(prefs.theme),
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
            show_help: false,
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
            theme,
            show_chooser: false,
            chooser_idx: 0,
            chooser_saved: ThemeName::default(),
            show_editor: false,
            editor_slot: 0,
            editor_palette: [0; 6],
        };
        app.init();
        app
    }

    fn init(&mut self) {
        match &self.store {
            Store::Sqlite(s) => {
                if !s.tables.is_empty() {
                    self.load_table();
                }
                self.status = "hjkl move · Tab focus · / search (n/N) · ^f/^b page · e edit · a add · d delete · : SQL · q quit".into();
            }
            Store::Rkyv(r) => {
                self.strings = r.strings(MIN_STRING);
                self.decoded = formats::try_decode(&r.bytes);
                if let Some(d) = &self.decoded {
                    self.rkyv_view = RkyvView::Records;
                    self.status = format!(
                        "{} · {} records · Enter detail · a add e edit r rename d delete · / search · 0/1/2/3 views · q quit",
                        d.format,
                        d.records.len()
                    );
                } else {
                    self.status = "1 Info · 2 Strings · 3 Hex · j/k scroll · / search (n/N) · q quit  (rkyv: unrecognized — structural view)".into();
                }
            }
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| self.render(f))?;
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
                Event::Mouse(m) => self.on_mouse(m),
                _ => {}
            }
        }
        Ok(())
    }

    // ----- key handling -----------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        let code = key.code;

        // Help overlay swallows the next key (any key closes it).
        if self.show_help {
            self.show_help = false;
            return;
        }
        // Theme editor / chooser overlays take keys next.
        if self.show_editor {
            return self.editor_key(code);
        }
        if self.show_chooser {
            return self.chooser_key(code);
        }

        // Modal input first. Snapshot the buffer into a local so no borrow of
        // `self.mode` is held across the `&mut self` dispatch call.
        enum Modal {
            Edit(String),
            Cmd(String),
            Search(String),
            Add(String),
            EditVal(String),
            Rename(String),
            Confirm,
            None,
        }
        let modal = match &self.mode {
            Mode::EditCell(buf) => Modal::Edit(buf.clone()),
            Mode::Command(buf) => Modal::Cmd(buf.clone()),
            Mode::Search(buf) => Modal::Search(buf.clone()),
            Mode::AddRecord(buf) => Modal::Add(buf.clone()),
            Mode::EditValue(buf) => Modal::EditVal(buf.clone()),
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
            Modal::EditVal(buf) => {
                return self.key_input(key, buf, Mode::EditValue, App::commit_edit_value)
            }
            Modal::Rename(buf) => {
                return self.key_input(key, buf, Mode::RenameRecord, App::commit_rename_record)
            }
            Modal::Confirm => return self.key_confirm_delete(code),
            Modal::None => {}
        }

        // `?` opens help from any screen.
        if code == KeyCode::Char('?') {
            self.show_help = true;
            return;
        }
        // `t` opens the theme chooser from any screen.
        if code == KeyCode::Char('t') {
            self.open_chooser();
            return;
        }

        match self.screen {
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
        // Any click dismisses the help overlay.
        if self.show_help {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                self.show_help = false;
            }
            return;
        }
        // Theme chooser: wheel cycles schemes, click confirms.
        if self.show_chooser {
            match m.kind {
                MouseEventKind::ScrollDown => self.chooser_key(KeyCode::Down),
                MouseEventKind::ScrollUp => self.chooser_key(KeyCode::Up),
                MouseEventKind::Down(MouseButton::Left) => self.chooser_key(KeyCode::Enter),
                _ => {}
            }
            return;
        }
        // Editor: wheel adjusts the selected slot's color.
        if self.show_editor {
            match m.kind {
                MouseEventKind::ScrollUp => self.editor_key(KeyCode::Up),
                MouseEventKind::ScrollDown => self.editor_key(KeyCode::Down),
                _ => {}
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

    // ----- theming: chooser + editor (ported from iftoprs) ------------------

    fn open_chooser(&mut self) {
        self.chooser_saved = self.theme.name;
        self.chooser_idx = ThemeName::ALL
            .iter()
            .position(|&t| t == self.theme.name)
            .unwrap_or(0);
        self.show_chooser = true;
    }

    fn chooser_preview(&mut self) {
        self.theme = Theme::from_name(ThemeName::ALL[self.chooser_idx]);
    }

    fn chooser_key(&mut self, code: KeyCode) {
        let n = ThemeName::ALL.len();
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.chooser_idx = (self.chooser_idx + n - 1) % n;
                self.chooser_preview();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.chooser_idx = (self.chooser_idx + 1) % n;
                self.chooser_preview();
            }
            KeyCode::Char('g') => {
                self.chooser_idx = 0;
                self.chooser_preview();
            }
            KeyCode::Char('G') => {
                self.chooser_idx = n - 1;
                self.chooser_preview();
            }
            KeyCode::Enter => {
                self.chooser_preview();
                crate::prefs::save(&crate::prefs::Prefs {
                    theme: self.theme.name,
                    custom: None,
                });
                self.show_chooser = false;
                self.status = format!("theme: {}", self.theme.name.display());
            }
            KeyCode::Char('e') => {
                // Open the editor seeded from the highlighted scheme.
                self.editor_palette = crate::theme::base_palette(ThemeName::ALL[self.chooser_idx]);
                self.editor_slot = 0;
                self.show_chooser = false;
                self.show_editor = true;
                self.editor_preview();
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t') => {
                self.theme = Theme::from_name(self.chooser_saved);
                self.show_chooser = false;
            }
            _ => {}
        }
    }

    fn editor_preview(&mut self) {
        self.theme = Theme::from_palette(self.theme.name, self.editor_palette);
    }

    fn editor_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Left | KeyCode::Char('h') => self.editor_slot = (self.editor_slot + 5) % 6,
            KeyCode::Right | KeyCode::Char('l') => self.editor_slot = (self.editor_slot + 1) % 6,
            KeyCode::Up | KeyCode::Char('k') => {
                self.editor_palette[self.editor_slot] =
                    self.editor_palette[self.editor_slot].wrapping_add(1);
                self.editor_preview();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.editor_palette[self.editor_slot] =
                    self.editor_palette[self.editor_slot].wrapping_sub(1);
                self.editor_preview();
            }
            KeyCode::PageUp => {
                self.editor_palette[self.editor_slot] =
                    self.editor_palette[self.editor_slot].wrapping_add(16);
                self.editor_preview();
            }
            KeyCode::PageDown => {
                self.editor_palette[self.editor_slot] =
                    self.editor_palette[self.editor_slot].wrapping_sub(16);
                self.editor_preview();
            }
            KeyCode::Enter => {
                self.editor_preview();
                crate::prefs::save(&crate::prefs::Prefs {
                    theme: self.theme.name,
                    custom: Some(self.editor_palette),
                });
                self.show_editor = false;
                self.status = "saved custom theme".into();
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.theme = Theme::from_name(self.chooser_saved);
                self.show_editor = false;
            }
            _ => {}
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
            KeyCode::PageDown => self.detail_scroll = (self.detail_scroll + 16).min(max_scroll),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(16),
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
            KeyCode::PageDown => self.schema_scroll += 16,
            KeyCode::PageUp => self.schema_scroll = self.schema_scroll.saturating_sub(16),
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
        self.status = if ok {
            format!("copied {} bytes to clipboard", self.detail_value.len())
        } else {
            "clipboard unavailable (no tty)".into()
        };
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
        let view = match self.sqlite().unwrap().rows(&table, total.max(1), 0) {
            Ok(v) => v,
            Err(e) => {
                self.status = format!("export failed: {}", e);
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
                self.status = "nothing to export (unrecognized archive)".into();
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
                KeyCode::Char('f') => self.page(PAGE),
                KeyCode::Char('b') => self.page(-PAGE),
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
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Tab => {
                self.focus = if self.focus == Focus::Left {
                    Focus::Right
                } else {
                    Focus::Left
                };
            }
            KeyCode::Up | KeyCode::Char('k') => match self.focus {
                Focus::Left => self.select_table(self.table_idx.wrapping_sub(1)),
                Focus::Right => self.row_idx = self.row_idx.saturating_sub(1),
            },
            KeyCode::Down | KeyCode::Char('j') => match self.focus {
                Focus::Left => self.select_table(self.table_idx + 1),
                Focus::Right => {
                    if let Some(r) = &self.rows {
                        if self.row_idx + 1 < r.rows.len() {
                            self.row_idx += 1;
                        }
                    }
                }
            },
            KeyCode::Left | KeyCode::Char('h') => {
                if self.focus == Focus::Right {
                    self.col_idx = self.col_idx.saturating_sub(1);
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
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
            KeyCode::PageDown => self.page(PAGE),
            KeyCode::PageUp => self.page(-PAGE),
            KeyCode::Char('e') => self.begin_edit_cell(),
            KeyCode::Char('a') => self.insert_row(),
            KeyCode::Char('d') => {
                if self.focus == Focus::Right && self.current_rowid().is_some() {
                    self.mode = Mode::ConfirmDelete;
                }
            }
            KeyCode::Char('S') => self.open_schema(),
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
        self.status = if ok {
            "copied cell to clipboard".into()
        } else {
            "clipboard unavailable (no tty)".into()
        };
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
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('0') => {
                if self.decoded.is_some() {
                    self.rkyv_view = RkyvView::Records;
                }
            }
            KeyCode::Char('1') => self.rkyv_view = RkyvView::Info,
            KeyCode::Char('2') => self.rkyv_view = RkyvView::Strings,
            KeyCode::Char('3') => self.rkyv_view = RkyvView::Hex,
            KeyCode::Up | KeyCode::Char('k') => match self.rkyv_view {
                RkyvView::Records => self.record_idx = self.record_idx.saturating_sub(1),
                RkyvView::Strings => self.string_idx = self.string_idx.saturating_sub(1),
                RkyvView::Hex => self.hex_row = self.hex_row.saturating_sub(1),
                RkyvView::Info => {}
            },
            KeyCode::Down | KeyCode::Char('j') => match self.rkyv_view {
                RkyvView::Records => {
                    let n = self.decoded.as_ref().map(|d| d.records.len()).unwrap_or(0);
                    if self.record_idx + 1 < n {
                        self.record_idx += 1;
                    }
                }
                RkyvView::Strings => {
                    if self.string_idx + 1 < self.strings.len() {
                        self.string_idx += 1;
                    }
                }
                RkyvView::Hex => self.hex_row += 1,
                RkyvView::Info => {}
            },
            KeyCode::PageDown => {
                if self.rkyv_view == RkyvView::Hex {
                    self.hex_row += 16;
                }
            }
            KeyCode::PageUp => {
                if self.rkyv_view == RkyvView::Hex {
                    self.hex_row = self.hex_row.saturating_sub(16);
                }
            }
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
                    self.open_modal(Mode::EditValue(String::new()));
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
                return;
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
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
    }

    /// Open a text-input modal, placing the cursor at the end of its buffer.
    fn open_modal(&mut self, mode: Mode) {
        self.input_cursor = match &mode {
            Mode::EditCell(s)
            | Mode::Command(s)
            | Mode::Search(s)
            | Mode::AddRecord(s)
            | Mode::EditValue(s)
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
                self.status = format!("failed: {}", e);
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
            self.status = format!("write failed: {}", e);
            return;
        }
        if let Store::Rkyv(r) = &mut self.store {
            r.bytes = new_bytes;
        }
        self.reload_rkyv();
        self.status = ok_msg;
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
            self.status = "add cancelled: empty key".into();
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

    fn commit_edit_value(&mut self, val: &str) {
        let (_, del_key, kind, bytes) = match self.rkyv_ctx() {
            Some(v) => v,
            None => return,
        };
        let value = parse_value_input(val);
        let n = value.len();
        self.rkyv_apply(
            crate::formats::set_value(&bytes, kind, &del_key, value),
            format!("value set ({} bytes)", n),
        );
    }

    fn commit_rename_record(&mut self, new_key: &str) {
        if new_key.is_empty() {
            self.status = "rename cancelled".into();
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
        self.strings = strings;
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
        self.load_table();
    }

    fn load_table(&mut self) {
        let (table, res) = match (self.current_table(), self.sqlite()) {
            (Some(t), Some(s)) => {
                let r = s.rows(&t, PAGE, self.page_offset);
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
            self.status = "row has no rowid — cannot edit (WITHOUT ROWID table)".into();
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
                self.status = format!("updated {}.{}", table, col);
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
                self.status = format!("inserted default row into {}", table);
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
                self.status = format!("deleted row {} from {}", rowid, table);
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
                self.status = format!("ok, {} row(s) affected", n);
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
        let from = self
            .current_rowid()
            .unwrap_or(if forward { i64::MIN } else { i64::MAX });

        let outcome: Result<Option<(i64, i64)>, String> = {
            let s = self.sqlite().unwrap();
            let first = s.find_row(&table, &columns, &self.search, from, forward);
            let rid = match first {
                Err(e) => Err(e.to_string()),
                Ok(Some(r)) => Ok(Some(r)),
                Ok(None) => {
                    let edge = if forward { i64::MIN } else { i64::MAX };
                    s.find_row(&table, &columns, &self.search, edge, forward)
                        .map_err(|e| e.to_string())
                }
            };
            match rid {
                Err(e) => Err(e),
                Ok(None) => Ok(None),
                Ok(Some(r)) => Ok(Some((r, s.rowid_ordinal(&table, r).unwrap_or(1)))),
            }
        };

        match outcome {
            Ok(Some((_rid, ord))) => {
                let idx0 = (ord - 1).max(0);
                self.page_offset = (idx0 / PAGE) * PAGE;
                self.load_table();
                self.row_idx = (idx0 - self.page_offset) as usize;
                let total = self.rows.as_ref().map(|r| r.total).unwrap_or(0);
                self.status = format!("/{}  (row {} of {})", self.search, ord, total);
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
                        self.status = format!("/{}  (offset {:#x})", self.search, off);
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

        match self.screen {
            Screen::Detail => self.render_detail(f, outer[0]),
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
            Mode::EditValue(buf) => self.render_input(
                f,
                "new value — text, or 0x<hex> for binary (Enter, Esc)",
                buf,
            ),
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

        if self.show_help {
            self.render_help(f);
        }
        if self.show_chooser {
            self.render_chooser(f);
        }
        if self.show_editor {
            self.render_editor(f);
        }
    }

    fn render_chooser(&self, f: &mut Frame) {
        let items: Vec<ListItem> = ThemeName::ALL
            .iter()
            .map(|&t| {
                let th = Theme::from_name(t);
                ListItem::new(Line::from(vec![
                    Span::styled("██", Style::default().fg(th.accent)),
                    Span::styled("██", Style::default().fg(th.primary)),
                    Span::styled("██", Style::default().fg(th.label)),
                    Span::styled("██  ", Style::default().fg(th.dark)),
                    Span::raw(t.display().to_string()),
                ]))
            })
            .collect();
        let mut st = ListState::default();
        st.select(Some(self.chooser_idx));
        let h = (ThemeName::ALL.len() as u16 + 2).min(f.area().height.saturating_sub(2));
        let area = centered(f.area(), 40.min(f.area().width), h);
        f.render_widget(Clear, area);
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.accent))
                    .title(" theme  (j/k · Enter=save · e=edit · Esc) "),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, area, &mut st);
    }

    fn render_editor(&self, f: &mut Frame) {
        let labels = ["primary", "accent", "alt", "label", "dim", "dark"];
        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(
                "edit palette — ←/→ slot · ↑/↓ ±1 · PgUp/Dn ±16 · Enter save · Esc",
                Style::default().fg(self.theme.dim),
            )),
            Line::from(""),
        ];
        for (i, (lab, &c)) in labels.iter().zip(self.editor_palette.iter()).enumerate() {
            let sel = i == self.editor_slot;
            lines.push(Line::from(vec![
                Span::styled(
                    if sel { "▶ " } else { "  " },
                    Style::default().fg(self.theme.accent),
                ),
                Span::styled(
                    "██████ ",
                    Style::default().fg(ratatui::style::Color::Indexed(c)),
                ),
                Span::styled(
                    format!("{:<9}", lab),
                    Style::default().fg(if sel {
                        self.theme.accent
                    } else {
                        self.theme.dim
                    }),
                ),
                Span::raw(format!("idx {c}")),
            ]));
        }
        let area = centered(f.area(), 44.min(f.area().width), 11.min(f.area().height));
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.accent))
                    .title(" theme editor "),
            ),
            area,
        );
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
                                        self.theme.accent
                                    } else {
                                        self.theme.dim
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
                            Style::default().fg(self.theme.accent),
                        ),
                        Span::raw(truncate(&rec.key, 80)),
                    ]));
                    for (name, val) in &rec.fields {
                        fields.push(Line::from(vec![
                            Span::styled(
                                format!("{:<20}", truncate(name, 20)),
                                Style::default().fg(self.theme.dim),
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

    fn render_schema(&self, f: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        for (ty, name, sql) in &self.schema {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<6}", ty), Style::default().fg(self.theme.alt)),
                Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
            ]));
            for l in sql.lines() {
                lines.push(Line::from(Span::styled(
                    format!("    {}", l),
                    Style::default().fg(self.theme.dim),
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

    fn render_help(&self, f: &mut Frame) {
        let is_sqlite = matches!(self.store, Store::Sqlite(_));
        let mut lines = vec![
            Line::from(Span::styled(
                "zdbview — keys",
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("  hjkl / arrows   move            gg / G   top / bottom"),
            Line::from("  /               search          n / N    next / prev match"),
            Line::from("  Enter           open detail     v        cycle value render"),
            Line::from("  y               copy (OSC52)    x        export to file"),
            Line::from("  t               themes (31)     Esc      back (quit on main)"),
            Line::from("  ?               this help       q        quit"),
        ];
        if is_sqlite {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  SQLite:",
                Style::default().fg(self.theme.label),
            )));
            lines.push(Line::from(
                "  Tab focus   e edit cell   a add row   d delete   : SQL",
            ));
            lines.push(Line::from(
                "  S schema    Ctrl-f/Ctrl-b page   / searches whole table",
            ));
        } else {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  rkyv:",
                Style::default().fg(self.theme.label),
            )));
            lines.push(Line::from("  0 Records   1 Info   2 Strings   3 Hex"));
            lines.push(Line::from(
                "  Records CRUD:  a add   e edit value   r rename   d delete",
            ));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  mouse:",
            Style::default().fg(self.theme.label),
        )));
        lines.push(Line::from(
            "  wheel scroll   click select   right-click select + detail",
        ));
        lines.push(Line::from(
            "  input line: ←/→ move  Home/End  Ctrl-a/e/w/u/k  cursor edit",
        ));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  press any key to close",
            Style::default().fg(self.theme.dim),
        )));

        let h = (lines.len() as u16 + 2).min(f.area().height);
        let area = centered(f.area(), 66.min(f.area().width), h);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.accent))
                    .title(" help "),
            ),
            area,
        );
    }

    fn render_sqlite(&mut self, f: &mut Frame, area: Rect) {
        let cols = Layout::horizontal([Constraint::Length(24), Constraint::Min(10)]).split(area);
        let (rect_left, rect_right) = (cols[0], cols[1]);

        let s = self.sqlite().unwrap();
        // Left: table list.
        let items: Vec<ListItem> = s.tables.iter().map(|t| ListItem::new(t.clone())).collect();
        let mut lstate = ListState::default();
        lstate.select(Some(self.table_idx));
        let left_border = self.pane_style(Focus::Left);
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(left_border)
                    .title(format!(
                        " {} — tables ({}) ",
                        s.path.file_name().and_then(|n| n.to_str()).unwrap_or("db"),
                        s.tables.len()
                    )),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, cols[0], &mut lstate);
        let off_left = lstate.selected().map(|_| lstate.offset()).unwrap_or(0);
        let mut off_right = 0usize;

        // Right: row grid.
        let title = match self.current_table() {
            Some(t) => {
                let total = self.rows.as_ref().map(|r| r.total).unwrap_or(0);
                format!(
                    " {} — rows {}..{} of {} ",
                    t,
                    self.page_offset,
                    self.page_offset + self.rows.as_ref().map(|r| r.rows.len() as i64).unwrap_or(0),
                    total
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
                                .fg(self.theme.accent)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().add_modifier(Modifier::BOLD)
                        };
                        Cell::from(c.clone()).style(st)
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

        // Left: keys.
        let items: Vec<ListItem> = d
            .records
            .iter()
            .map(|rec| ListItem::new(truncate(&rec.key, 60)))
            .collect();
        let mut st = ListState::default();
        st.select(Some(self.record_idx.min(d.records.len().saturating_sub(1))));
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.accent))
                    .title(format!(" {} — {} keys ", d.format, d.records.len())),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, cols[0], &mut st);
        let off_records = st.offset();

        // Right: selected value — decoded scalar fields, then a hex dump.
        let mut lines: Vec<Line> = Vec::new();
        if let Some(rec) = d.records.get(self.record_idx) {
            for (name, val) in &rec.fields {
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<22}", name), Style::default().fg(self.theme.dim)),
                    Span::raw(val.clone()),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("value — {} bytes (hex):", rec.value.len()),
                Style::default().fg(self.theme.primary),
            )));
            let rows = area.height.saturating_sub(6) as usize;
            for i in 0..rows {
                let off = i * 16;
                if off >= rec.value.len() {
                    break;
                }
                lines.push(Line::from(hex_row(&rec.value, off)));
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
                Span::styled("file:    ", Style::default().fg(self.theme.dim)),
                Span::raw(r.path.display().to_string()),
            ]),
            Line::from(vec![
                Span::styled("size:    ", Style::default().fg(self.theme.dim)),
                Span::raw(format!("{} bytes", r.len())),
            ]),
            Line::from(vec![
                Span::styled("strings: ", Style::default().fg(self.theme.dim)),
                Span::raw(format!(
                    "{} runs (>= {} printable bytes)",
                    self.strings.len(),
                    MIN_STRING
                )),
            ]),
        ];

        match &self.decoded {
            Some(d) => {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("format:  ", Style::default().fg(self.theme.dim)),
                    Span::styled(d.format.clone(), Style::default().fg(self.theme.label)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("records: ", Style::default().fg(self.theme.dim)),
                    Span::raw(d.records.len().to_string()),
                ]));
                lines.push(Line::from(""));
                for (name, val) in &d.header {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {:<16}", name),
                            Style::default().fg(self.theme.dim),
                        ),
                        Span::raw(val.clone()),
                    ]));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Views:  0 Records (key/value)  2 Strings  3 Hex",
                    Style::default().fg(self.theme.dim),
                )));
            }
            None => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "unrecognized rkyv archive: no matching format decoder.",
                    Style::default().fg(self.theme.primary),
                )));
                lines.push(Line::from(Span::styled(
                    "rkyv stores no field names or type tags, so an unknown type",
                    Style::default().fg(self.theme.primary),
                )));
                lines.push(Line::from(Span::styled(
                    "cannot be decoded generically — showing raw structure.",
                    Style::default().fg(self.theme.primary),
                )));
                lines.push(Line::from(Span::styled(
                    "Views:  2 Strings (embedded text)  3 Hex (raw bytes)",
                    Style::default().fg(self.theme.dim),
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
        let items: Vec<ListItem> = self
            .strings
            .iter()
            .map(|h| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:08x}  ", h.offset),
                        Style::default().fg(self.theme.dim),
                    ),
                    Span::raw(truncate(&h.text, 200)),
                ]))
            })
            .collect();
        let mut st = ListState::default();
        st.select(Some(
            self.string_idx.min(self.strings.len().saturating_sub(1)),
        ));
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Strings ({}) ", self.strings.len())),
            )
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
                .border_style(Style::default().fg(self.theme.accent))
                .title(format!(" {} ", title)),
        );
        f.render_widget(p, area);
    }

    fn pane_style(&self, which: Focus) -> Style {
        if self.focus == which {
            Style::default().fg(self.theme.accent)
        } else {
            Style::default().fg(self.theme.dim)
        }
    }
}

/// Recent-files picker shown when zdbview is launched with no file argument.
/// Returns the chosen file, or `None` if the user quits.
pub fn pick_mru(terminal: &mut DefaultTerminal, entries: &[Entry]) -> Result<Option<PathBuf>> {
    let mut idx = 0usize;
    let mut pending_g = false;
    let mut search = String::new();
    let mut searching = false;
    loop {
        let query = if searching {
            Some(search.as_str())
        } else {
            None
        };
        terminal.draw(|f| render_picker(f, entries, idx, query))?;
        let ev = event::read()?;
        // Mouse: wheel moves the selection, a click opens the entry under it.
        if let Event::Mouse(m) = ev {
            match m.kind {
                MouseEventKind::ScrollDown => {
                    if idx + 1 < entries.len() {
                        idx += 1;
                    }
                }
                MouseEventKind::ScrollUp => idx = idx.saturating_sub(1),
                MouseEventKind::Down(_) => {
                    // The list starts one row below the block's top border.
                    let clicked = (m.row as usize).saturating_sub(1);
                    if clicked < entries.len() {
                        return Ok(Some(entries[clicked].path.clone()));
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

            // Search-input capture takes priority.
            if searching {
                match key.code {
                    KeyCode::Esc => {
                        searching = false;
                        search.clear();
                    }
                    KeyCode::Enter => {
                        searching = false;
                        if let Some(i) = picker_find(entries, idx, true, &search) {
                            idx = i;
                        }
                    }
                    KeyCode::Backspace => {
                        search.pop();
                    }
                    KeyCode::Char(c) => search.push(c),
                    _ => {}
                }
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
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                KeyCode::Char('/') => {
                    searching = true;
                    search.clear();
                }
                KeyCode::Char('n') => {
                    if let Some(i) = picker_find(entries, idx, true, &search) {
                        idx = i;
                    }
                }
                KeyCode::Char('N') => {
                    if let Some(i) = picker_find(entries, idx, false, &search) {
                        idx = i;
                    }
                }
                KeyCode::Char('G') => idx = entries.len().saturating_sub(1),
                KeyCode::Up | KeyCode::Char('k') => idx = idx.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    if idx + 1 < entries.len() {
                        idx += 1;
                    }
                }
                KeyCode::Enter => {
                    if let Some(e) = entries.get(idx) {
                        return Ok(Some(e.path.clone()));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Find the next/previous MRU entry whose filename contains `q`.
fn picker_find(entries: &[Entry], from: usize, forward: bool, q: &str) -> Option<usize> {
    if q.is_empty() {
        return None;
    }
    let ql = q.to_lowercase();
    find_next(entries.len(), from, forward, |i| {
        entries[i]
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_lowercase().contains(&ql))
            .unwrap_or(false)
    })
}

fn render_picker(f: &mut Frame, entries: &[Entry], idx: usize, query: Option<&str>) {
    let outer = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());

    if entries.is_empty() {
        let p = Paragraph::new(vec![
            Line::from(""),
            Line::from("  No recent files."),
            Line::from(""),
            Line::from(Span::styled(
                "  Open one with:  zdbview <file>",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" zdbview — recent "),
        );
        f.render_widget(p, outer[0]);
    } else {
        let items: Vec<ListItem> = entries
            .iter()
            .map(|e| {
                let name = e.path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                let dir = e.path.parent().and_then(|p| p.to_str()).unwrap_or("");
                let (badge, color) = match e.kind {
                    Kind::Sqlite => ("sqlite", Color::Green),
                    Kind::Rkyv => ("rkyv  ", Color::Magenta),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {} ", badge), Style::default().fg(color)),
                    Span::styled(
                        format!("{:<28}", truncate(name, 28)),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:>10}  ", mru::rel_age(e.opened)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(truncate(dir, 60), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let mut st = ListState::default();
        st.select(Some(idx.min(entries.len().saturating_sub(1))));
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" zdbview — recent files ({}) ", entries.len())),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, outer[0], &mut st);
    }

    let help = match query {
        Some(q) => Paragraph::new(format!("/{}_", q))
            .style(Style::default().fg(Color::Black).bg(Color::Cyan)),
        None => Paragraph::new("j/k move · / search · n/N next/prev · Enter open · q quit")
            .style(Style::default().fg(Color::Black).bg(Color::Gray)),
    };
    f.render_widget(help, outer[1]);
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
/// ignored), otherwise the string's UTF-8 bytes are used verbatim.
fn parse_value_input(s: &str) -> Vec<u8> {
    if let Some(hex) = s.strip_prefix("0x") {
        let clean: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
        if !clean.is_empty() && clean.len().is_multiple_of(2) {
            if let Some(b) = hex_to_bytes(&clean) {
                return b;
            }
        }
    }
    s.as_bytes().to_vec()
}

fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i + 1 < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

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

/// Lowercase hex of a byte slice.
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
            lines.push(Line::from(hex_row(bytes, off)));
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

/// One 16-byte `offset  hex  |ascii|` line for an arbitrary slice.
fn hex_row(bytes: &[u8], offset: usize) -> String {
    let end = (offset + 16).min(bytes.len());
    let chunk = &bytes[offset.min(bytes.len())..end];
    let mut hex = String::with_capacity(50);
    for i in 0..16 {
        if i < chunk.len() {
            hex.push_str(&format!("{:02x} ", chunk[i]));
        } else {
            hex.push_str("   ");
        }
        if i == 7 {
            hex.push(' ');
        }
    }
    let ascii: String = chunk
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("{:08x}  {} |{}|", offset, hex, ascii)
}

/// Truncate a display string to `max` chars, appending an ellipsis.
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
    use super::{find_bytes, find_next, hit, input_delete_word, input_left, input_right};
    use ratatui::layout::Rect;

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
