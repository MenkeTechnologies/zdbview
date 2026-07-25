//! Themed overlays shared by every screen, ported from iftoprs.
//!
//! One `Overlays` value owns the active color scheme plus the state of the help
//! box, the scheme chooser, the palette editor, and the transient toast. Both
//! the main app and the recent-files picker embed it, so the overlays behave
//! identically everywhere instead of being reimplemented per screen.
//!
//! Ported pieces: `draw_help`, `draw_theme_chooser`, `draw_theme_editor`, the
//! `set_cell` / `set_str` / `draw_box` buffer primitives (iftoprs
//! `src/ui/render.rs`), and `StatusMsg` + `draw_status` — the transient toast —
//! from `iftoprs/src/ui/app.rs:343` and `render.rs:1762`.

use std::time::Instant;

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Clear;
use ratatui::Frame;

use crate::theme::{base_palette, Theme, ThemeName};

/// How long a toast stays up (iftoprs `StatusMsg::expired`).
const TOAST_SECS: u64 = 3;
/// Columns of key bindings in the help overlay.
const HELP_COLS: usize = 3;
/// Width reserved for the key name in a help row.
const HELP_KEY_W: u16 = 9;

/// A transient confirmation message — port of iftoprs's `StatusMsg`. It paints
/// over the UI and dismisses itself after [`TOAST_SECS`] seconds.
pub struct Toast {
    pub text: String,
    since: Instant,
}

impl Toast {
    pub fn new(text: String) -> Self {
        Self {
            text,
            since: Instant::now(),
        }
    }

    pub fn expired(&self) -> bool {
        self.since.elapsed().as_secs() >= TOAST_SECS
    }
}

/// Which key sections the help overlay lists — the bindings differ per screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpCtx {
    Sqlite,
    Rkyv,
    Picker,
    /// The hex editor over a record's value.
    HexEdit,
}

/// The overlay layer: active scheme plus help / chooser / editor / toast state.
pub struct Overlays {
    /// The active color scheme. Every screen styles itself from this.
    pub theme: Theme,
    pub help: bool,
    pub chooser: bool,
    pub chooser_idx: usize,
    /// Scheme to restore when the chooser or editor is cancelled.
    chooser_saved: ThemeName,
    pub editor: bool,
    pub editor_palette: [u8; 6],
    pub editor_slot: usize,
    pub toast: Option<Toast>,
}

impl Overlays {
    pub fn new(theme: Theme) -> Self {
        Overlays {
            chooser_saved: theme.name,
            theme,
            help: false,
            chooser: false,
            chooser_idx: 0,
            editor: false,
            editor_palette: [0; 6],
            editor_slot: 0,
            toast: None,
        }
    }

    /// True while any overlay is up (the caller must not act on the same key).
    pub fn active(&self) -> bool {
        self.help || self.chooser || self.editor
    }

    /// Raise a toast, replacing any current one.
    pub fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some(Toast::new(msg.into()));
    }

    /// Drop the toast once it has aged out. Called from the event loop's tick so
    /// it disappears without needing input (iftoprs clears it on its data tick).
    pub fn expire_toast(&mut self) {
        if self.toast.as_ref().is_some_and(Toast::expired) {
            self.toast = None;
        }
    }

    pub fn open_chooser(&mut self) {
        self.chooser_saved = self.theme.name;
        self.chooser_idx = ThemeName::ALL
            .iter()
            .position(|&t| t == self.theme.name)
            .unwrap_or(0);
        self.chooser = true;
    }

    /// Open the palette editor, seeded from the scheme under the chooser cursor
    /// (or from the active scheme when opened directly with `C`).
    pub fn open_editor(&mut self) {
        if !self.chooser {
            self.chooser_saved = self.theme.name;
            self.chooser_idx = ThemeName::ALL
                .iter()
                .position(|&t| t == self.theme.name)
                .unwrap_or(0);
        }
        self.editor_palette = base_palette(ThemeName::ALL[self.chooser_idx]);
        self.editor_slot = 0;
        self.chooser = false;
        self.editor = true;
        self.preview_editor();
    }

    fn preview_chooser(&mut self) {
        self.theme = Theme::from_name(ThemeName::ALL[self.chooser_idx]);
    }

    fn preview_editor(&mut self) {
        self.theme = Theme::from_palette(self.theme.name, self.editor_palette);
    }

    /// Handle a key. Returns `true` when the overlay layer consumed it, which
    /// means the caller must not treat it as one of its own bindings.
    ///
    /// This covers both directions: closing/driving an open overlay, and the
    /// global openers `h` / `?` (help), `c` (chooser) and `C` (editor).
    pub fn on_key(&mut self, code: KeyCode) -> bool {
        // Any key dismisses help.
        if self.help {
            self.help = false;
            return true;
        }
        if self.editor {
            self.editor_key(code);
            return true;
        }
        if self.chooser {
            self.chooser_key(code);
            return true;
        }
        match code {
            KeyCode::Char('h') | KeyCode::Char('?') => self.help = true,
            KeyCode::Char('c') => self.open_chooser(),
            KeyCode::Char('C') => self.open_editor(),
            _ => return false,
        }
        true
    }

    /// Mouse counterpart of [`Overlays::on_key`]: the wheel drives the chooser
    /// and editor, a click confirms, and any click dismisses help.
    pub fn on_mouse(&mut self, m: MouseEvent) -> bool {
        if self.help {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                self.help = false;
            }
            return true;
        }
        if self.chooser {
            match m.kind {
                MouseEventKind::ScrollDown => self.chooser_key(KeyCode::Down),
                MouseEventKind::ScrollUp => self.chooser_key(KeyCode::Up),
                MouseEventKind::Down(MouseButton::Left) => self.chooser_key(KeyCode::Enter),
                _ => {}
            }
            return true;
        }
        if self.editor {
            match m.kind {
                MouseEventKind::ScrollUp => self.editor_key(KeyCode::Up),
                MouseEventKind::ScrollDown => self.editor_key(KeyCode::Down),
                _ => {}
            }
            return true;
        }
        false
    }

    fn chooser_key(&mut self, code: KeyCode) {
        let n = ThemeName::ALL.len();
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.chooser_idx = (self.chooser_idx + n - 1) % n;
                self.preview_chooser();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.chooser_idx = (self.chooser_idx + 1) % n;
                self.preview_chooser();
            }
            KeyCode::Char('g') => {
                self.chooser_idx = 0;
                self.preview_chooser();
            }
            KeyCode::Char('G') => {
                self.chooser_idx = n - 1;
                self.preview_chooser();
            }
            KeyCode::Enter => {
                self.preview_chooser();
                crate::prefs::save(&crate::prefs::Prefs {
                    theme: self.theme.name,
                    custom: None,
                });
                self.chooser = false;
                self.toast(format!("scheme: {}", self.theme.name.display()));
            }
            // `C`, not `e` — `e` edits data on every other screen.
            KeyCode::Char('C') => self.open_editor(),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') => {
                self.theme = Theme::from_name(self.chooser_saved);
                self.chooser = false;
            }
            _ => {}
        }
    }

    fn editor_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Left | KeyCode::Char('h') => self.editor_slot = (self.editor_slot + 5) % 6,
            KeyCode::Right | KeyCode::Char('l') => self.editor_slot = (self.editor_slot + 1) % 6,
            KeyCode::Up | KeyCode::Char('k') => {
                self.editor_palette[self.editor_slot] =
                    self.editor_palette[self.editor_slot].wrapping_add(1);
                self.preview_editor();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.editor_palette[self.editor_slot] =
                    self.editor_palette[self.editor_slot].wrapping_sub(1);
                self.preview_editor();
            }
            KeyCode::PageUp => {
                self.editor_palette[self.editor_slot] =
                    self.editor_palette[self.editor_slot].wrapping_add(16);
                self.preview_editor();
            }
            KeyCode::PageDown => {
                self.editor_palette[self.editor_slot] =
                    self.editor_palette[self.editor_slot].wrapping_sub(16);
                self.preview_editor();
            }
            KeyCode::Enter => {
                self.preview_editor();
                crate::prefs::save(&crate::prefs::Prefs {
                    theme: self.theme.name,
                    custom: Some(self.editor_palette),
                });
                self.editor = false;
                self.toast("saved custom palette");
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.theme = Theme::from_name(self.chooser_saved);
                self.editor = false;
            }
            _ => {}
        }
    }

    // ----- rendering --------------------------------------------------------

    /// Draw whichever overlay is up, then the toast on top.
    pub fn render(&self, f: &mut Frame, ctx: HelpCtx) {
        if self.help {
            self.render_help(f, ctx);
        }
        if self.chooser {
            self.render_chooser(f);
        }
        if self.editor {
            self.render_editor(f);
        }
        if let Some(t) = &self.toast {
            if !t.expired() {
                self.render_toast(f, &t.text);
            }
        }
    }

    /// The keyboard-shortcut overlay, ported from iftoprs's `draw_help`: a
    /// themed double-line box painted straight into the buffer, with the key
    /// sections laid out across three columns.
    fn render_help(&self, f: &mut Frame, ctx: HelpCtx) {
        let t = &self.theme;
        let sections = help_sections(ctx);

        // Pick the shortest per-column row budget that still fits every
        // section in three columns, so no keys are silently dropped.
        let total: usize = sections.iter().map(|s| s.keys.len() + 2).sum();
        let (limit, placed) = (total.div_ceil(HELP_COLS)..=total)
            .find_map(|l| pack_sections(&sections, l, HELP_COLS).map(|p| (l, p)))
            .expect("a single column always fits every section");

        let area = f.area();
        let bw = 90u16.min(area.width);
        // 4 header rows (border, title, subtitle, blank) + 3 footer rows.
        let bh = (limit as u16 + 7).min(area.height);
        let bg = t.help_bg;
        let text = Style::default().fg(Color::White).bg(bg);
        let key = Style::default().fg(t.help_key).bg(bg);
        let title = Style::default()
            .fg(t.help_title)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        let section = Style::default()
            .fg(t.help_section)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        let hint = Style::default().fg(t.dim).bg(bg);

        f.render_widget(Clear, centered(area, bw, bh));
        let buf = f.buffer_mut();
        let (x0, y0) = draw_box(buf, area, bw, bh, bg, Style::default().fg(t.help_border));

        set_centered(
            buf,
            x0,
            bw,
            y0 + 1,
            &format!(
                "ZDBVIEW v{} — KEYBOARD SHORTCUTS",
                env!("CARGO_PKG_VERSION")
            ),
            title,
        );
        set_centered(buf, x0, bw, y0 + 2, ctx.label(), hint);

        // Three columns of "KEY  description" rows under their section heading.
        let cw = (bw.saturating_sub(4) / HELP_COLS as u16).max(HELP_KEY_W + 2);
        let last_row = y0 + bh.saturating_sub(4);
        for (s, (col, row)) in sections.iter().zip(placed) {
            let cx = x0 + 2 + col as u16 * cw;
            let sy = y0 + 4 + row as u16;
            if sy > last_row {
                continue;
            }
            set_str(buf, cx, sy, s.title, section, cw);
            for (i, (k, desc)) in s.keys.iter().enumerate() {
                let ky = sy + 1 + i as u16;
                if ky > last_row {
                    break;
                }
                set_str(buf, cx, ky, k, key, HELP_KEY_W);
                set_str(
                    buf,
                    cx + HELP_KEY_W + 1,
                    ky,
                    desc,
                    text,
                    cw - HELP_KEY_W - 1,
                );
            }
        }

        set_centered(
            buf,
            x0,
            bw,
            y0 + bh.saturating_sub(3),
            &format!("scheme: {} | c=chooser", t.name.display()),
            Style::default().fg(t.help_val).bg(bg),
        );
        set_centered(
            buf,
            x0,
            bw,
            y0 + bh.saturating_sub(2),
            "press any key to close",
            hint,
        );
    }

    /// The scheme chooser, ported from iftoprs's `draw_theme_chooser`: each row
    /// is the scheme name plus its 6-cell swatch, `▸` marks the active scheme,
    /// and the highlighted row inverts onto the accent color. Unlike iftoprs
    /// (which clips at the box edge) the list scrolls, because the schemes do
    /// not all fit in a modal on a normal-height terminal.
    fn render_chooser(&self, f: &mut Frame) {
        let t = &self.theme;
        let area = f.area();
        let bw = 50u16.min(area.width);
        let bh = (ThemeName::ALL.len() as u16 + 6).min(area.height);
        let bg = t.help_bg;
        let rows = bh.saturating_sub(6) as usize; // rows available for schemes

        // Keep the highlighted scheme in view.
        let first = self
            .chooser_idx
            .saturating_sub(rows.saturating_sub(1))
            .min(ThemeName::ALL.len().saturating_sub(rows));

        f.render_widget(Clear, centered(area, bw, bh));
        let buf = f.buffer_mut();
        let (x0, y0) = draw_box(buf, area, bw, bh, bg, Style::default().fg(t.help_border));
        set_centered(
            buf,
            x0,
            bw,
            y0 + 1,
            "SCHEME CHOOSER",
            Style::default()
                .fg(t.help_title)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        );

        for (i, &tn) in ThemeName::ALL.iter().enumerate().skip(first).take(rows) {
            let ey = y0 + 3 + (i - first) as u16;
            let sel = i == self.chooser_idx;
            let row = if sel {
                Style::default().fg(Color::Black).bg(t.help_key)
            } else {
                Style::default().fg(Color::White).bg(bg)
            };
            if sel {
                for x in x0 + 1..x0 + bw - 1 {
                    set_cell(buf, x, ey, " ", row);
                }
            }
            let marker = if tn == t.name { "▸ " } else { "  " };
            set_str(
                buf,
                x0 + 2,
                ey,
                &format!("{}{:<20}", marker, tn.display()),
                row,
                24,
            );
            for (si, (color, block)) in Theme::swatch(tn).iter().enumerate() {
                let sw = Style::default().fg(*color).bg(row.bg.unwrap_or(bg));
                set_str(buf, x0 + 26 + si as u16 * 2, ey, block, sw, 2);
            }
        }

        set_centered(
            buf,
            x0,
            bw,
            y0 + bh.saturating_sub(2),
            "j/k:nav  Enter:save  C:edit  Esc:cancel",
            Style::default().fg(t.dim).bg(bg),
        );
    }

    /// The palette editor, ported from iftoprs's `draw_theme_editor`: one row
    /// per color slot with its index, a swatch and an arrow sample, plus a
    /// full-palette preview bar.
    fn render_editor(&self, f: &mut Frame) {
        let t = &self.theme;
        let p = self.editor_palette;
        let area = f.area();
        let bw = 56u16.min(area.width);
        let bh = 15u16.min(area.height);
        let bg = t.help_bg;
        let text = Style::default().fg(Color::White).bg(bg);
        let hint = Style::default().fg(t.dim).bg(bg);
        let sel_row = Style::default().fg(Color::White).bg(Color::Indexed(237));

        f.render_widget(Clear, centered(area, bw, bh));
        let buf = f.buffer_mut();
        let (x0, y0) = draw_box(buf, area, bw, bh, bg, Style::default().fg(t.help_border));
        set_centered(
            buf,
            x0,
            bw,
            y0 + 1,
            "PALETTE EDITOR",
            Style::default()
                .fg(t.help_title)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        );

        // The slot names are zdbview's UI roles, not iftoprs's.
        for (i, label) in ["primary", "accent", "alt", "label", "dim", "dark"]
            .iter()
            .enumerate()
        {
            let ry = y0 + 3 + i as u16;
            if ry + 2 >= y0 + bh {
                break;
            }
            let sel = i == self.editor_slot;
            let row = if sel { sel_row } else { text };
            if sel {
                for x in x0 + 1..x0 + bw - 1 {
                    set_cell(buf, x, ry, " ", sel_row);
                }
            }
            set_str(buf, x0 + 2, ry, if sel { "▸ " } else { "  " }, row, 2);
            set_str(buf, x0 + 4, ry, &format!("{:<10}", label), row, 10);
            set_str(buf, x0 + 15, ry, &format!("{:>3}", p[i]), row, 3);
            let swatch = Style::default().fg(Color::Indexed(p[i])).bg(bg);
            set_str(buf, x0 + 20, ry, "█████", swatch, 5);
            set_str(buf, x0 + 26, ry, " ◀──▶", swatch, 5);
        }

        // Preview bar: the six slots across the full width, in role order.
        let py = y0 + 10;
        if py + 2 < y0 + bh {
            set_str(buf, x0 + 2, py, "preview:", hint, 8);
            let pw = bw.saturating_sub(13);
            for j in 0..pw {
                let slot = (j as usize * 6 / pw.max(1) as usize).min(5);
                set_cell(
                    buf,
                    x0 + 11 + j,
                    py,
                    "█",
                    Style::default().fg(Color::Indexed(p[slot])).bg(bg),
                );
            }
        }

        set_str(
            buf,
            x0 + 2,
            y0 + bh.saturating_sub(3),
            "←/→:slot  ↑/↓:±1  PgUp/PgDn:±16",
            hint,
            bw.saturating_sub(4),
        );
        set_str(
            buf,
            x0 + 2,
            y0 + bh.saturating_sub(2),
            "Enter:save  Esc/q:cancel",
            hint,
            bw.saturating_sub(4),
        );
    }

    /// The toast: a one-line pill centered inside the content pane, just above
    /// its bottom border and the status bar, inverse on the scheme's accent
    /// (port of iftoprs's `draw_status`, which likewise offsets from the bottom
    /// so it never lands on a frame line).
    fn render_toast(&self, f: &mut Frame, text: &str) {
        let area = f.area();
        let w = (text.chars().count() as u16 + 4).min(area.width);
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + area.height.saturating_sub(3);
        let style = Style::default()
            .fg(Color::Black)
            .bg(self.theme.help_key)
            .add_modifier(Modifier::BOLD);
        let buf = f.buffer_mut();
        set_str(buf, x, y, &format!(" {} ", text), style, w);
    }
}

// ----- direct-buffer primitives (ported from iftoprs `ui::render`) -----------

/// Write one styled cell, clipped to the buffer.
fn set_cell(buf: &mut Buffer, x: u16, y: u16, ch: &str, s: Style) {
    let a = buf.area();
    if x >= a.x && y >= a.y && x < a.x + a.width && y < a.y + a.height {
        let c = &mut buf[(x, y)];
        c.set_symbol(ch);
        c.set_style(s);
    }
}

/// Write a string, clipped to `mw` columns and to the buffer.
fn set_str(buf: &mut Buffer, x: u16, y: u16, s: &str, st: Style, mw: u16) {
    let a = *buf.area();
    if y < a.y || y >= a.y + a.height {
        return;
    }
    let mut ch_buf = [0u8; 4];
    for (i, ch) in s.chars().enumerate() {
        let cx = x + i as u16;
        if cx >= x + mw || cx >= a.x + a.width {
            break;
        }
        let c = &mut buf[(cx, y)];
        c.set_symbol(ch.encode_utf8(&mut ch_buf));
        c.set_style(st);
    }
}

/// Fill a centered `bw`×`bh` box with `bg` and draw a double-line border.
/// Returns the box's top-left corner.
fn draw_box(
    buf: &mut Buffer,
    area: Rect,
    bw: u16,
    bh: u16,
    bg: Color,
    border: Style,
) -> (u16, u16) {
    let x0 = area.x + (area.width.saturating_sub(bw)) / 2;
    let y0 = area.y + (area.height.saturating_sub(bh)) / 2;
    let x1 = x0 + bw.saturating_sub(1);
    let y1 = y0 + bh.saturating_sub(1);
    let fill = Style::default().bg(bg);
    for y in y0..y0 + bh {
        for x in x0..x0 + bw {
            set_cell(buf, x, y, " ", fill);
        }
    }
    set_cell(buf, x0, y0, "╔", border);
    set_cell(buf, x1, y0, "╗", border);
    set_cell(buf, x0, y1, "╚", border);
    set_cell(buf, x1, y1, "╝", border);
    for x in x0 + 1..x1 {
        set_cell(buf, x, y0, "═", border);
        set_cell(buf, x, y1, "═", border);
    }
    for y in y0 + 1..y1 {
        set_cell(buf, x0, y, "║", border);
        set_cell(buf, x1, y, "║", border);
    }
    (x0, y0)
}

/// Write `s` horizontally centered inside a box at `x0` of width `bw`.
fn set_centered(buf: &mut Buffer, x0: u16, bw: u16, y: u16, s: &str, st: Style) {
    let cw = s.chars().count() as u16;
    set_str(buf, x0 + (bw.saturating_sub(cw)) / 2, y, s, st, bw);
}

/// A centered `w`×`h` sub-rect of `area`, clamped to it.
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

// ----- help contents --------------------------------------------------------

impl HelpCtx {
    /// The subtitle under the help title — what the bindings apply to.
    fn label(self) -> &'static str {
        match self {
            HelpCtx::Sqlite => "SQLite database",
            HelpCtx::Rkyv => "rkyv archive",
            HelpCtx::Picker => "recent files",
            HelpCtx::HexEdit => "hex editor",
        }
    }
}

/// One titled group of key bindings in the help overlay.
struct HelpSection {
    title: &'static str,
    keys: &'static [(&'static str, &'static str)],
}

/// Bindings shown on every screen: the overlay layer's own keys.
const DISPLAY_KEYS: &[(&str, &str)] = &[
    ("c", "Scheme chooser"),
    ("C", "Palette editor"),
    ("o", "Back to file list"),
    ("h / ?", "Toggle help"),
    ("q", "Quit"),
];

const MOUSE_KEYS: &[(&str, &str)] = &[
    ("wheel", "Scroll"),
    ("click", "Select"),
    ("right", "Select + detail"),
];

/// The help overlay's contents for a screen.
fn help_sections(ctx: HelpCtx) -> Vec<HelpSection> {
    if ctx == HelpCtx::HexEdit {
        // The editor is modal: it owns h and c, so its help is reachable from
        // the Records view before opening it (and from `?` inside it).
        return vec![
            HelpSection {
                title: "HEX NAV",
                keys: &[
                    ("h/l ←→", "Byte back / fwd"),
                    ("j/k ↑↓", "Row up / down"),
                    ("0 / $", "Row start / end"),
                    ("g / G", "First / last byte"),
                    ("^F / ^B", "Page"),
                    ("click", "Place cursor"),
                ],
            },
            HelpSection {
                title: "HEX EDIT",
                keys: &[
                    ("i / R", "Enter edit mode"),
                    ("Tab", "Hex / ascii column"),
                    ("0-9 a-f", "Set nibble (hex)"),
                    ("any char", "Byte (ascii)"),
                    ("Esc", "Leave edit mode"),
                ],
            },
            HelpSection {
                title: "LENGTH",
                keys: &[("o / O", "Insert 00 after/before"), ("x", "Delete byte")],
            },
            HelpSection {
                title: "SAVE",
                keys: &[("^s", "Write value back"), ("q", "Back (twice if dirty)")],
            },
        ];
    }
    if ctx == HelpCtx::Picker {
        return vec![
            HelpSection {
                title: "NAV",
                keys: &[
                    ("j/k ↑↓", "Move"),
                    ("gg / G", "Top / bottom"),
                    ("Enter", "Open file"),
                    ("Esc / q", "Quit"),
                ],
            },
            HelpSection {
                title: "SEARCH",
                keys: &[("/", "Path search"), ("n / N", "Next / prev")],
            },
            HelpSection {
                title: "SCAN",
                keys: &[("r", "Rescan now"), ("R", "Rescan, ignore cache")],
            },
            HelpSection {
                title: "DISPLAY",
                keys: DISPLAY_KEYS,
            },
            HelpSection {
                title: "MOUSE",
                keys: &[("wheel", "Scroll"), ("click", "Open file")],
            },
        ];
    }

    let store = if ctx == HelpCtx::Sqlite {
        HelpSection {
            title: "SQLITE",
            keys: &[
                ("e", "Edit cell"),
                ("a", "Add row"),
                ("d", "Delete row"),
                (":", "Raw SQL"),
                ("S", "Schema view"),
                ("s", "Sort column"),
                ("< / >", "Sort prev/next col"),
            ],
        }
    } else {
        HelpSection {
            title: "RKYV",
            keys: &[
                ("0", "Records"),
                ("1", "Info"),
                ("2", "Strings"),
                ("3", "Hex"),
                ("a", "Add record"),
                ("e", "Edit value (hex)"),
                ("r", "Rename key"),
                ("d", "Delete record"),
            ],
        }
    };
    vec![
        HelpSection {
            title: "NAV",
            keys: &[
                ("j/k ↑↓", "Move"),
                ("←/→", "Column / pane"),
                ("gg / G", "Top / bottom"),
                ("^F / ^B", "Page (SQLite)"),
                ("Tab", "Switch focus"),
                ("Enter", "Open detail"),
                ("Esc", "Back / quit"),
            ],
        },
        HelpSection {
            title: "SEARCH",
            keys: &[("/", "Search"), ("n / N", "Next / prev")],
        },
        store,
        HelpSection {
            title: "VALUE",
            keys: &[
                ("v", "Cycle render"),
                ("y", "Copy (OSC 52)"),
                ("x", "Export to file"),
            ],
        },
        HelpSection {
            title: "DISPLAY",
            keys: DISPLAY_KEYS,
        },
        HelpSection {
            title: "MOUSE",
            keys: MOUSE_KEYS,
        },
        HelpSection {
            title: "INPUT LINE",
            keys: &[
                ("←/→", "Move cursor"),
                ("Home/End", "Line ends"),
                ("^A / ^E", "Start / end"),
                ("^W", "Delete word"),
                ("^U / ^K", "Kill to start/end"),
            ],
        },
    ]
}

/// Assign each help section to a column, greedily filling `limit` rows per
/// column (iftoprs's packing, with the limit chosen so nothing is dropped).
/// A section occupies its heading + one row per key + a blank separator.
/// Returns `None` when the sections do not fit in `cols` columns.
fn pack_sections(
    sections: &[HelpSection],
    limit: usize,
    cols: usize,
) -> Option<Vec<(usize, usize)>> {
    let mut placed = Vec::with_capacity(sections.len());
    let (mut col, mut row) = (0usize, 0usize);
    for s in sections {
        let need = s.keys.len() + 2;
        if row + need > limit && row > 0 {
            col += 1;
            row = 0;
        }
        if col >= cols {
            return None;
        }
        placed.push((col, row));
        row += need;
    }
    Some(placed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn overlays() -> Overlays {
        Overlays::new(Theme::from_name(ThemeName::NeonSprawl))
    }

    /// Render one frame of just the overlay layer, flattened to row strings.
    fn rows(ov: &Overlays, ctx: HelpCtx, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| ov.render(f, ctx)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn has(rows: &[String], needle: &str) -> bool {
        rows.iter().any(|r| r.contains(needle))
    }

    /// Every key section must land in a column: the packing search must never
    /// silently drop a section, which would hide bindings from the overlay.
    #[test]
    fn help_sections_always_pack_into_three_columns() {
        for ctx in [HelpCtx::Sqlite, HelpCtx::Rkyv, HelpCtx::Picker] {
            let sections = help_sections(ctx);
            let total: usize = sections.iter().map(|s| s.keys.len() + 2).sum();
            let (limit, placed) = (total.div_ceil(HELP_COLS)..=total)
                .find_map(|l| pack_sections(&sections, l, HELP_COLS).map(|p| (l, p)))
                .expect("some limit must fit");
            assert_eq!(placed.len(), sections.len(), "a section was dropped");
            assert!(placed.iter().all(|&(c, _)| c < HELP_COLS));
            for (s, (_, row)) in sections.iter().zip(&placed) {
                assert!(row + s.keys.len() + 2 <= limit || *row == 0, "{}", s.title);
            }
        }
    }

    /// A one-column budget must still place everything — the fallback the
    /// overlay's limit search relies on for its `expect`.
    #[test]
    fn help_sections_fit_a_single_column_at_full_budget() {
        let sections = help_sections(HelpCtx::Sqlite);
        let total: usize = sections.iter().map(|s| s.keys.len() + 2).sum();
        let placed = pack_sections(&sections, total, 1).expect("single column fallback");
        assert_eq!(placed.len(), sections.len());
        assert!(placed.iter().all(|&(c, _)| c == 0));
    }

    /// Too tight a budget must report failure rather than overflow the columns.
    #[test]
    fn pack_sections_rejects_an_impossible_budget() {
        assert!(pack_sections(&help_sections(HelpCtx::Rkyv), 3, HELP_COLS).is_none());
    }

    /// Help must draw its frame, title, footer, and the section matching the
    /// screen it was opened from — and only that one.
    #[test]
    fn help_shows_the_section_for_its_screen() {
        let mut ov = overlays();
        ov.help = true;
        let r = rows(&ov, HelpCtx::Rkyv, 100, 40);
        assert!(has(&r, "KEYBOARD SHORTCUTS"), "title missing");
        assert!(has(&r, "╔") && has(&r, "╝"), "frame missing");
        assert!(has(&r, "rkyv archive"));
        assert!(has(&r, "RKYV") && has(&r, "Rename key"));
        assert!(!has(&r, "SQLITE"), "SQLite section leaked in");
        assert!(has(&r, "Scheme chooser") && has(&r, "Palette editor"));
        assert!(has(&r, "scheme: Neon Sprawl"));
        assert!(has(&r, "press any key to close"));

        let r = rows(&ov, HelpCtx::Sqlite, 100, 40);
        assert!(has(&r, "SQLITE") && has(&r, "Raw SQL"));
        assert!(!has(&r, "RKYV"));

        // The picker's help lists its own bindings, not the store ones.
        let r = rows(&ov, HelpCtx::Picker, 100, 40);
        assert!(has(&r, "recent files"));
        assert!(has(&r, "Open file"));
        assert!(has(&r, "Path search"));
        assert!(
            has(&r, "Rescan now"),
            "picker help must list the rescan key"
        );
        assert!(!has(&r, "Raw SQL") && !has(&r, "Rename key"));
    }

    /// Overlay geometry guard. Every glyph must occupy one terminal column and
    /// the box's vertical borders must sit in the same columns on every row — a
    /// double-width glyph (an emoji) shifts its row and pushes the border out,
    /// which is invisible in a cell buffer but obvious on screen.
    #[test]
    fn overlay_glyphs_are_single_width_and_borders_align() {
        const NARROW: &str = "─│┌┐└┘├┤┬┴┼║═╔╗╚╝▸▶◀█↑↓←→—·±";
        for which in 0..3 {
            let mut ov = overlays();
            match which {
                0 => ov.help = true,
                1 => {
                    ov.chooser = true;
                    ov.chooser_idx = 3;
                }
                _ => {
                    ov.editor = true;
                    ov.editor_slot = 2;
                }
            }
            ov.toast("saved custom palette");
            let r = rows(&ov, HelpCtx::Sqlite, 100, 40);
            for (y, row) in r.iter().enumerate() {
                for ch in row.chars() {
                    assert!(
                        ch.is_ascii() || NARROW.contains(ch),
                        "overlay {which} row {y}: {ch:?} (U+{:04X}) may not be one column wide",
                        ch as u32
                    );
                }
            }
            let borders: Vec<Vec<usize>> = r
                .iter()
                // Column positions, not byte offsets — rows hold multi-byte glyphs.
                .map(|row| {
                    row.chars()
                        .enumerate()
                        .filter(|(_, c)| *c == '║')
                        .map(|(i, _)| i)
                        .collect()
                })
                .filter(|v: &Vec<usize>| !v.is_empty())
                .collect();
            assert!(!borders.is_empty(), "overlay {which} drew no box sides");
            assert!(
                borders.windows(2).all(|w| w[0] == w[1]),
                "overlay {which} box sides are ragged: {borders:?}"
            );
        }
    }

    /// Overlays must clip, not panic, on a terminal smaller than their box.
    #[test]
    fn overlays_survive_a_tiny_terminal() {
        for (w, h) in [(20u16, 6u16), (8, 3), (40, 12), (1, 1)] {
            for which in 0..3 {
                let mut ov = overlays();
                match which {
                    0 => ov.help = true,
                    1 => ov.chooser = true,
                    _ => ov.editor = true,
                }
                ov.toast("a toast wider than this terminal is wide");
                rows(&ov, HelpCtx::Picker, w, h);
            }
        }
    }

    /// The chooser scrolls so the highlighted scheme is drawn even when the box
    /// is shorter than the scheme list.
    #[test]
    fn chooser_scrolls_to_keep_the_selection_visible() {
        let mut ov = overlays();
        ov.chooser = true;
        ov.chooser_idx = ThemeName::ALL.len() - 1;
        let last = ThemeName::ALL[ThemeName::ALL.len() - 1].display();
        let r = rows(&ov, HelpCtx::Rkyv, 60, 20);
        assert!(has(&r, last), "last scheme not visible: {last}");
        assert!(has(&r, "SCHEME CHOOSER"));

        ov.chooser_idx = 0;
        let r = rows(&ov, HelpCtx::Rkyv, 60, 20);
        assert!(has(&r, ThemeName::ALL[0].display()));
    }

    /// The editor labels each slot and shows its current palette index.
    #[test]
    fn editor_shows_slot_labels_and_indices() {
        let mut ov = overlays();
        ov.editor = true;
        ov.editor_palette = [11, 22, 33, 44, 55, 66];
        ov.editor_slot = 1;
        let r = rows(&ov, HelpCtx::Rkyv, 80, 30);
        assert!(has(&r, "PALETTE EDITOR"));
        for label in ["primary", "accent", "alt", "label", "dim", "dark"] {
            assert!(has(&r, label), "slot {label} missing");
        }
        for idx in ["11", "22", "33", "44", "55", "66"] {
            assert!(has(&r, idx), "index {idx} missing");
        }
        assert!(has(&r, "preview:") && has(&r, "Enter:save"));
    }

    /// A toast paints just above the status bar and vanishes once expired.
    #[test]
    fn toast_renders_until_it_expires() {
        let mut ov = overlays();
        ov.toast("copied cell to clipboard");
        let r = rows(&ov, HelpCtx::Rkyv, 60, 10);
        assert!(has(&r, "copied cell to clipboard"), "toast not drawn");
        assert!(
            r[7].contains("copied cell"),
            "toast must sit inside the pane, above its border and the status bar, got {:?}",
            r[7]
        );

        // Back-date it past the dismiss window: it must stop rendering and be
        // dropped by the loop's tick.
        ov.toast = Some(Toast {
            text: "copied cell to clipboard".into(),
            since: Instant::now() - std::time::Duration::from_secs(TOAST_SECS + 1),
        });
        assert!(ov.toast.as_ref().unwrap().expired());
        let r = rows(&ov, HelpCtx::Rkyv, 60, 10);
        assert!(!has(&r, "copied cell"), "expired toast still drawn");
        ov.expire_toast();
        assert!(ov.toast.is_none(), "expired toast not cleared by tick");
    }

    /// A fresh toast must not be expired, and `expire_toast` must keep it.
    #[test]
    fn fresh_toast_survives_a_tick() {
        let mut ov = overlays();
        ov.toast("scheme: Blade Runner");
        ov.expire_toast();
        assert!(ov.toast.is_some());
    }

    /// The global openers and their exits, including that `h` opens help (it is
    /// not a motion key) and `c` toggles the chooser.
    #[test]
    fn global_keys_open_and_close_each_overlay() {
        let mut ov = overlays();
        assert!(!ov.active());

        assert!(ov.on_key(KeyCode::Char('h')));
        assert!(ov.help);
        assert!(ov.on_key(KeyCode::Char('x')), "any key closes help");
        assert!(!ov.help);

        assert!(ov.on_key(KeyCode::Char('?')));
        assert!(ov.help);
        ov.on_key(KeyCode::Esc);

        assert!(ov.on_key(KeyCode::Char('c')));
        assert!(ov.chooser);
        assert!(ov.on_key(KeyCode::Char('c')), "c closes the chooser again");
        assert!(!ov.chooser);

        assert!(ov.on_key(KeyCode::Char('C')));
        assert!(ov.editor, "C opens the palette editor directly");
        assert!(ov.on_key(KeyCode::Esc));
        assert!(!ov.editor);

        // Keys the overlay layer doesn't own must fall through to the caller.
        for code in [
            KeyCode::Char('e'),
            KeyCode::Char('j'),
            KeyCode::Char('q'),
            KeyCode::Enter,
        ] {
            assert!(!ov.on_key(code), "{code:?} must not be consumed");
        }
    }

    /// Cancelling the chooser restores the scheme that was active when it
    /// opened, while Enter keeps the previewed one.
    #[test]
    fn chooser_cancel_restores_and_enter_keeps() {
        let mut ov = overlays();
        let before = ov.theme.name;
        ov.on_key(KeyCode::Char('c'));
        ov.on_key(KeyCode::Down);
        let previewed = ov.theme.name;
        assert_ne!(previewed, before, "j/k must preview live");
        ov.on_key(KeyCode::Esc);
        assert_eq!(ov.theme.name, before, "cancel must restore");

        ov.on_key(KeyCode::Char('c'));
        ov.on_key(KeyCode::Down);
        ov.on_key(KeyCode::Enter);
        assert_eq!(ov.theme.name, previewed);
        assert!(!ov.chooser);
        assert!(
            ov.toast
                .as_ref()
                .is_some_and(|t| t.text.starts_with("scheme:")),
            "saving a scheme must toast"
        );
    }

    /// `C` from inside the chooser seeds the editor from the highlighted scheme,
    /// and editing a slot changes the live theme.
    #[test]
    fn editor_seeds_from_the_highlighted_scheme_and_previews() {
        let mut ov = overlays();
        ov.on_key(KeyCode::Char('c'));
        ov.on_key(KeyCode::Down);
        let seeded = base_palette(ThemeName::ALL[ov.chooser_idx]);
        ov.on_key(KeyCode::Char('C'));
        assert!(ov.editor && !ov.chooser);
        assert_eq!(ov.editor_palette, seeded);

        ov.on_key(KeyCode::Right);
        assert_eq!(ov.editor_slot, 1);
        ov.on_key(KeyCode::Up);
        assert_eq!(ov.editor_palette[1], seeded[1].wrapping_add(1));
        assert_eq!(
            ov.theme.accent,
            Color::Indexed(seeded[1].wrapping_add(1)),
            "the edit must preview live"
        );
    }

    /// The wheel drives the chooser and a click confirms, as in iftoprs.
    #[test]
    fn mouse_drives_the_chooser() {
        let mut ov = overlays();
        ov.open_chooser();
        let start = ov.chooser_idx;
        assert!(ov.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }));
        assert_eq!(ov.chooser_idx, start + 1);
        assert!(ov.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }));
        assert!(!ov.chooser, "click must confirm the selection");
    }
}
