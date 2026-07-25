//! `xxd`-style hex **editor** with a cursor, ported from zmax's
//! `zmax-term/src/ui/hex.rs` (`HexView`).
//!
//! zdbview uses it to edit a value in place instead of retyping it into a
//! one-line prompt: the editor opens pre-filled with the record's current bytes,
//! and every byte is addressable.
//!
//! Ported verbatim in behaviour: two modes (read-only **nav**, then **EDIT** via
//! `i`/`R`), `Tab` toggles the focused column between hex and ASCII, hex digits
//! compose the high then the low nibble of the byte under the cursor and then
//! advance, printable ASCII overwrites and advances, `Ctrl-s` saves, and a dirty
//! buffer arms a quit warning that a second `q` discards.
//!
//! One deliberate difference: zmax edits a file of fixed length ("overwrite-only
//! — the file length never changes in this slice"), while a record's value has no
//! such constraint, so nav mode adds `o` / `O` (insert a `00` byte after / before
//! the cursor) and `x` (delete the byte under it). Typing into an empty buffer in
//! EDIT mode appends the first byte rather than doing nothing.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::theme::Theme;

/// Number of bytes shown per row.
const BYTES_PER_ROW: usize = 16;

/// Which column edits and cursor highlighting are focused on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Hex,
    Ascii,
}

impl Column {
    fn toggled(self) -> Column {
        match self {
            Column::Hex => Column::Ascii,
            Column::Ascii => Column::Hex,
        }
    }
}

/// What the caller must do after a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Stay in the editor.
    None,
    /// Write [`HexEdit::bytes`] back to the store.
    Save,
    /// Leave the editor, discarding any unsaved edits.
    Close,
}

/// The hex editor over one value's bytes.
pub struct HexEdit {
    /// What is being edited (a record key), shown in the header.
    pub label: String,
    /// The bytes being edited — the source of truth for everything rendered.
    pub bytes: Vec<u8>,
    /// Index of the byte under the cursor (`0` when the buffer is empty).
    cursor: usize,
    /// Index of the top visible row (each row is [`BYTES_PER_ROW`] bytes).
    scroll: usize,
    /// Body rows visible in the last render, for paging and cursor tracking.
    viewport: usize,
    /// `true` once `i`/`R` has entered EDIT mode; `false` in read-only nav.
    edit_mode: bool,
    /// The focused column, for editing and the cursor highlight.
    focus: Column,
    /// In hex EDIT mode, the high nibble already typed for the cursor byte when a
    /// second (low-nibble) digit is awaited.
    pending_high: Option<u8>,
    /// `true` once any byte changed since the editor opened or last saved.
    pub dirty: bool,
    /// `true` once a dirty quit has been warned; a second `q` then discards.
    quit_armed: bool,
    /// Transient notice shown in the header, cleared on the next key press.
    message: Option<String>,
}

impl HexEdit {
    pub fn new(label: String, bytes: Vec<u8>) -> Self {
        HexEdit {
            label,
            bytes,
            cursor: 0,
            scroll: 0,
            viewport: 1,
            edit_mode: false,
            focus: Column::Hex,
            pending_high: None,
            dirty: false,
            quit_armed: false,
            message: None,
        }
    }

    /// Number of rows needed to show every byte (at least one, so an empty value
    /// still draws a body).
    fn total_rows(&self) -> usize {
        self.bytes.len().div_ceil(BYTES_PER_ROW).max(1)
    }

    fn max_scroll(&self) -> usize {
        self.total_rows().saturating_sub(self.viewport)
    }

    fn scroll_by(&mut self, delta: isize) {
        let next = self.scroll as isize + delta;
        self.scroll = next.clamp(0, self.max_scroll() as isize) as usize;
    }

    /// Move the cursor to byte `idx` (clamped) and scroll so it stays visible.
    /// No-op on an empty buffer. Abandons any pending high nibble.
    fn move_to(&mut self, idx: isize) {
        self.pending_high = None;
        if self.bytes.is_empty() {
            return;
        }
        let max = self.bytes.len() as isize - 1;
        self.cursor = idx.clamp(0, max) as usize;
        self.ensure_cursor_visible();
    }

    fn ensure_cursor_visible(&mut self) {
        let row = self.cursor / BYTES_PER_ROW;
        if row < self.scroll {
            self.scroll = row;
        } else if row >= self.scroll + self.viewport {
            self.scroll = row + 1 - self.viewport;
        }
    }

    /// Apply a typed char to the focused column, overwriting the cursor byte.
    fn type_char(&mut self, ch: char) {
        // A value can start out empty (a record added with `a`); typing then
        // creates the first byte instead of doing nothing.
        if self.bytes.is_empty() && (hex_digit(ch).is_some() || is_printable(ch)) {
            self.bytes.push(0);
            self.cursor = 0;
            self.dirty = true;
        }
        match self.focus {
            Column::Hex => {
                if let Some(digit) = hex_digit(ch) {
                    let (cursor, pending, changed) =
                        apply_hex_digit(&mut self.bytes, self.cursor, self.pending_high, digit);
                    self.cursor = cursor;
                    self.pending_high = pending;
                    if changed {
                        self.dirty = true;
                        self.ensure_cursor_visible();
                    }
                }
            }
            Column::Ascii => {
                let (cursor, changed) = apply_ascii_char(&mut self.bytes, self.cursor, ch);
                self.cursor = cursor;
                if changed {
                    self.dirty = true;
                    self.ensure_cursor_visible();
                }
            }
        }
    }

    /// Insert a `00` byte after (or before) the cursor and select it.
    fn insert_byte(&mut self, after: bool) {
        let at = if self.bytes.is_empty() {
            0
        } else if after {
            self.cursor + 1
        } else {
            self.cursor
        };
        self.bytes.insert(at.min(self.bytes.len()), 0);
        self.cursor = at.min(self.bytes.len() - 1);
        self.pending_high = None;
        self.dirty = true;
        self.ensure_cursor_visible();
    }

    /// Delete the byte under the cursor.
    fn delete_byte(&mut self) {
        if self.bytes.is_empty() {
            return;
        }
        self.bytes.remove(self.cursor);
        self.cursor = self.cursor.min(self.bytes.len().saturating_sub(1));
        self.pending_high = None;
        self.dirty = true;
        self.ensure_cursor_visible();
    }

    /// Called by the app after a successful save.
    pub fn mark_saved(&mut self) {
        self.dirty = false;
        self.message = Some(format!("wrote {} bytes", self.bytes.len()));
    }

    /// Handle a key press (ported from zmax's `HexView::handle_event`).
    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // Quit-arming only survives consecutive `q` presses, and the header
        // notice only until the next key.
        let was_armed = self.quit_armed;
        self.quit_armed = false;
        self.message = None;

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let cursor = self.cursor as isize;
        let bpr = BYTES_PER_ROW as isize;
        let page = (self.viewport.max(1) * BYTES_PER_ROW) as isize;
        let row_start = (self.cursor - (self.cursor % BYTES_PER_ROW)) as isize;

        // Handled the same in both modes: save and column toggle.
        if ctrl && key.code == KeyCode::Char('s') {
            return Action::Save;
        }
        if key.code == KeyCode::Tab {
            self.focus = self.focus.toggled();
            self.pending_high = None;
            return Action::None;
        }
        if ctrl {
            match key.code {
                KeyCode::Char('d') | KeyCode::Char('f') => {
                    self.scroll_by(self.viewport.max(1) as isize);
                    self.move_to(cursor + page);
                }
                KeyCode::Char('u') | KeyCode::Char('b') => {
                    self.scroll_by(-(self.viewport.max(1) as isize));
                    self.move_to(cursor - page);
                }
                _ => {}
            }
            return Action::None;
        }

        if self.edit_mode {
            // EDIT mode: arrows navigate, printable keys edit the focused column.
            match key.code {
                KeyCode::Esc => {
                    self.edit_mode = false;
                    self.pending_high = None;
                }
                KeyCode::Left => self.move_to(cursor - 1),
                KeyCode::Right => self.move_to(cursor + 1),
                KeyCode::Up => self.move_to(cursor - bpr),
                KeyCode::Down => self.move_to(cursor + bpr),
                KeyCode::Home => self.move_to(row_start),
                KeyCode::End => self.move_to(row_start + bpr - 1),
                KeyCode::PageDown => {
                    self.scroll_by(self.viewport.max(1) as isize);
                    self.move_to(cursor + page);
                }
                KeyCode::PageUp => {
                    self.scroll_by(-(self.viewport.max(1) as isize));
                    self.move_to(cursor - page);
                }
                KeyCode::Backspace => self.delete_byte(),
                KeyCode::Char(ch) => self.type_char(ch),
                _ => {}
            }
            return Action::None;
        }

        // NAV mode: read-only motions plus the structural edits.
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.dirty && !was_armed {
                    self.quit_armed = true;
                    self.message =
                        Some("unsaved edits — ^s to save, or q again to discard".to_string());
                    return Action::None;
                }
                return Action::Close;
            }
            KeyCode::Char('i') | KeyCode::Char('R') => self.edit_mode = true,
            KeyCode::Char('h') | KeyCode::Left => self.move_to(cursor - 1),
            KeyCode::Char('l') | KeyCode::Right => self.move_to(cursor + 1),
            KeyCode::Char('j') | KeyCode::Down => self.move_to(cursor + bpr),
            KeyCode::Char('k') | KeyCode::Up => self.move_to(cursor - bpr),
            KeyCode::Char('0') | KeyCode::Home => self.move_to(row_start),
            KeyCode::Char('$') | KeyCode::End => self.move_to(row_start + bpr - 1),
            KeyCode::Char('g') => self.move_to(0),
            KeyCode::Char('G') => self.move_to(isize::MAX),
            KeyCode::PageDown => {
                self.scroll_by(self.viewport.max(1) as isize);
                self.move_to(cursor + page);
            }
            KeyCode::PageUp => {
                self.scroll_by(-(self.viewport.max(1) as isize));
                self.move_to(cursor - page);
            }
            // Length edits — a value is not a fixed-size file.
            KeyCode::Char('o') => self.insert_byte(true),
            KeyCode::Char('O') => self.insert_byte(false),
            KeyCode::Char('x') | KeyCode::Delete => self.delete_byte(),
            _ => {}
        }
        Action::None
    }

    /// Wheel scrolls; a click inside the hex or ASCII column places the cursor.
    pub fn on_mouse(&mut self, m: MouseEvent, area: Rect) -> Action {
        match m.kind {
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::Down(_) => {
                if let Some(idx) = self.byte_at(m.column, m.row, area) {
                    self.move_to(idx as isize);
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Byte index under a click, or `None` when it misses both columns. The
    /// layout mirrors [`hex_row`]: an 8-wide offset gutter, two spaces, 16 hex
    /// cells of 3 columns each with an extra space after the 8th, then the ASCII
    /// gutter wrapped in `|`.
    fn byte_at(&self, col: u16, row: u16, area: Rect) -> Option<usize> {
        // Body rows start below the border and the two header rows.
        let body_top = area.y + 3;
        if row < body_top || col <= area.x {
            return None;
        }
        let line = (row - body_top) as usize + self.scroll;
        let x = (col - area.x - 1) as usize;
        const HEX_START: usize = 10; // 8 offset digits + 2 spaces
        const ASCII_START: usize = HEX_START + BYTES_PER_ROW * 3 + 2; // + group gap + '|'
        let in_row = if (HEX_START..HEX_START + BYTES_PER_ROW * 3 + 1).contains(&x) {
            let mut off = x - HEX_START;
            // The gap after the 8th cell shifts everything past it by one.
            if off >= 8 * 3 {
                off = off.saturating_sub(1);
            }
            off / 3
        } else if (ASCII_START..ASCII_START + BYTES_PER_ROW).contains(&x) {
            x - ASCII_START
        } else {
            return None;
        };
        let idx = line * BYTES_PER_ROW + in_row;
        (idx < self.bytes.len()).then_some(idx)
    }

    /// Draw the editor, filling `area`.
    pub fn render(&mut self, f: &mut Frame, area: Rect, t: &Theme) {
        let mode = match (self.edit_mode, self.focus) {
            (true, Column::Hex) => "-- EDIT (hex) --",
            (true, Column::Ascii) => "-- EDIT (ascii) --",
            (false, Column::Hex) => "NAV [hex]",
            (false, Column::Ascii) => "NAV [ascii]",
        };
        let header = format!(
            " {}{}  —  {} byte{}  ·  cursor 0x{:08x}  ·  {} ",
            self.label,
            if self.dirty { " [+]" } else { "" },
            self.bytes.len(),
            if self.bytes.len() == 1 { "" } else { "s" },
            self.cursor,
            mode,
        );

        let hint = match &self.message {
            Some(msg) => msg.clone(),
            None if self.edit_mode => {
                "type to edit · Tab hex/ascii · arrows move · ^s save · Esc leave edit".into()
            }
            None => {
                "hjkl move · i edit · Tab hex/ascii · o/O insert · x delete · g/G · ^s save · q back"
                    .into()
            }
        };

        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(
                header,
                Style::default()
                    .fg(t.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(hint, Style::default().fg(t.dim))),
        ];

        // Two header rows plus the block's borders.
        let body_h = area.height.saturating_sub(4) as usize;
        self.viewport = body_h.max(1);
        self.ensure_cursor_visible();

        let offset_style = Style::default().fg(t.dim);
        let hex_style = Style::default().fg(t.label);
        let text_style = Style::default().fg(t.primary);
        // The focused column gets the real cursor; the other one a dim mirror.
        let focused = Style::default().fg(t.accent).add_modifier(Modifier::REVERSED);
        let mirror = Style::default().fg(t.dim).add_modifier(Modifier::REVERSED);
        let (hex_cursor, ascii_cursor) = match self.focus {
            Column::Hex => (focused, mirror),
            Column::Ascii => (mirror, focused),
        };

        for row in self.scroll..self.scroll + body_h {
            let start = row * BYTES_PER_ROW;
            if start >= self.bytes.len() && !(self.bytes.is_empty() && row == 0) {
                lines.push(Line::default());
                continue;
            }
            let end = (start + BYTES_PER_ROW).min(self.bytes.len());
            let chunk = &self.bytes[start.min(self.bytes.len())..end];

            let mut spans: Vec<Span> = Vec::with_capacity(BYTES_PER_ROW * 3 + 4);
            spans.push(Span::styled(format!("{:08x}  ", start), offset_style));
            for i in 0..BYTES_PER_ROW {
                if i == 8 {
                    spans.push(Span::raw(" "));
                }
                match chunk.get(i) {
                    Some(b) => {
                        let style = if start + i == self.cursor {
                            hex_cursor
                        } else {
                            hex_style
                        };
                        spans.push(Span::styled(format!("{:02x}", b), style));
                        spans.push(Span::raw(" "));
                    }
                    None => spans.push(Span::raw("   ")),
                }
            }
            spans.push(Span::styled("|", offset_style));
            for i in 0..BYTES_PER_ROW {
                match chunk.get(i) {
                    Some(&b) => {
                        let ch = if (0x20..=0x7e).contains(&b) {
                            b as char
                        } else {
                            '.'
                        };
                        let style = if start + i == self.cursor {
                            ascii_cursor
                        } else {
                            text_style
                        };
                        spans.push(Span::styled(ch.to_string(), style));
                    }
                    None => spans.push(Span::raw(" ")),
                }
            }
            spans.push(Span::styled("|", offset_style));
            lines.push(Line::from(spans));
        }

        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.accent))
                    .title(" hex editor "),
            ),
            area,
        );
    }
}

/// Value of a hex-digit char `[0-9a-fA-F]` (`0..=15`), or `None`.
fn hex_digit(c: char) -> Option<u8> {
    c.to_digit(16).map(|d| d as u8)
}

/// Whether `ch` is a byte the ASCII column can write.
fn is_printable(ch: char) -> bool {
    (0x20u32..=0x7e).contains(&(ch as u32))
}

/// Replace the **high** nibble of `b`, keeping the low nibble.
pub fn set_high_nibble(b: u8, digit: u8) -> u8 {
    (b & 0x0f) | ((digit & 0x0f) << 4)
}

/// Replace the **low** nibble of `b`, keeping the high nibble.
pub fn set_low_nibble(b: u8, digit: u8) -> u8 {
    (b & 0xf0) | (digit & 0x0f)
}

/// Apply one hex-digit edit (overwrite only). Returns
/// `(new_cursor, new_pending_high, changed)`.
///
/// With no pending high nibble it sets the **high** nibble of the byte under
/// `cursor` and records the digit; with one pending it sets the **low** nibble
/// and advances. Editing past the end (including an empty buffer) is a no-op.
fn apply_hex_digit(
    bytes: &mut [u8],
    cursor: usize,
    pending: Option<u8>,
    digit: u8,
) -> (usize, Option<u8>, bool) {
    if cursor >= bytes.len() {
        return (cursor, None, false);
    }
    match pending {
        None => {
            bytes[cursor] = set_high_nibble(bytes[cursor], digit);
            (cursor, Some(digit), true)
        }
        Some(_) => {
            bytes[cursor] = set_low_nibble(bytes[cursor], digit);
            let next = (cursor + 1).min(bytes.len() - 1);
            (next, None, true)
        }
    }
}

/// Apply one ASCII-char edit (overwrite only). Returns `(new_cursor, changed)`.
/// Only printable bytes act; past the end is a no-op.
fn apply_ascii_char(bytes: &mut [u8], cursor: usize, ch: char) -> (usize, bool) {
    if cursor >= bytes.len() || !is_printable(ch) {
        return (cursor, false);
    }
    bytes[cursor] = ch as u8;
    let next = (cursor + 1).min(bytes.len() - 1);
    (next, true)
}

/// Format one hex-dump row: the `{offset:08x}` gutter, two spaces and 16 hex
/// cells grouped 8 + 8, plus exactly 16 ASCII characters. A short final row pads
/// both columns so the layout stays aligned with full rows.
pub fn hex_row(offset: usize, chunk: &[u8]) -> (String, String) {
    let mut hex = format!("{:08x}  ", offset);
    let mut ascii = String::with_capacity(BYTES_PER_ROW);
    for i in 0..BYTES_PER_ROW {
        if i == 8 {
            hex.push(' ');
        }
        match chunk.get(i) {
            Some(&b) => {
                hex.push_str(&format!("{:02x} ", b));
                ascii.push(if (0x20..=0x7e).contains(&b) {
                    b as char
                } else {
                    '.'
                });
            }
            None => {
                hex.push_str("   ");
                ascii.push(' ');
            }
        }
    }
    (hex, ascii)
}

/// One complete `xxd`-style dump line: `offset  hex cells |ascii|`. Every hex
/// view in zdbview (the rkyv Hex view, the record dump in the detail pane, and
/// this editor) formats its rows through [`hex_row`], so they cannot drift apart.
pub fn hex_dump_line(offset: usize, chunk: &[u8]) -> String {
    let (hex, ascii) = hex_row(offset, chunk);
    format!("{}|{}|", hex, ascii.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeName;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(c: char) -> KeyEvent {
        KeyEvent::from(KeyCode::Char(c))
    }

    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::from(c)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn rows(ed: &mut HexEdit, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let theme = Theme::from_name(ThemeName::NeonSprawl);
        term.draw(|f| {
            let a = f.area();
            ed.render(f, a, &theme)
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
    }

    // ── row formatting (ported from zmax's tests) ────────────────────────────

    #[test]
    fn full_row() {
        let (hex, ascii) = hex_row(0, b"ABCDEFGHIJKLMNOP");
        assert_eq!(
            hex,
            "00000000  41 42 43 44 45 46 47 48  49 4a 4b 4c 4d 4e 4f 50 "
        );
        assert_eq!(ascii, "ABCDEFGHIJKLMNOP");
    }

    #[test]
    fn short_final_row_is_padded_to_keep_columns_aligned() {
        let (hex, ascii) = hex_row(0x10, &[0xAA, 0xBB, 0xCC]);
        assert!(hex.starts_with("00000010  aa bb cc "));
        // 8 offset + 2 gap + 16*3 cells + 1 group gap.
        assert_eq!(hex.len(), 59);
        assert_eq!(ascii, format!("{:<16}", "..."));
    }

    /// The shared dump line is what the rkyv Hex view and the detail pane print.
    #[test]
    fn dump_line_matches_xxd_layout() {
        let line = hex_dump_line(0x10, b"AB\x00");
        assert!(line.starts_with("00000010  41 42 00 "));
        assert!(line.ends_with("|AB.|"), "got {line:?}");
    }

    #[test]
    fn non_printable_bytes_become_dot() {
        let (_hex, ascii) = hex_row(0, &[0x00, 0x41, 0x7f, 0x80, 0x1b]);
        assert_eq!(ascii, format!("{:<16}", ".A..."));
    }

    // ── byte edits ───────────────────────────────────────────────────────────

    #[test]
    fn nibbles_set_independently() {
        assert_eq!(set_high_nibble(0xab, 0x1), 0x1b);
        assert_eq!(set_low_nibble(0xab, 0x9), 0xa9);
        // Only the low 4 bits of the digit are used.
        assert_eq!(set_high_nibble(0x00, 0xff), 0xf0);
        assert_eq!(set_low_nibble(0x00, 0xff), 0x0f);
    }

    #[test]
    fn hex_digits_compose_a_byte_then_advance() {
        let mut ed = HexEdit::new("k".into(), vec![0x00, 0x00]);
        ed.on_key(key('i'));
        ed.on_key(key('4'));
        assert_eq!(ed.bytes, vec![0x40, 0x00], "high nibble first");
        assert_eq!(ed.cursor, 0, "cursor waits for the low nibble");
        ed.on_key(key('1'));
        assert_eq!(ed.bytes, vec![0x41, 0x00]);
        assert_eq!(ed.cursor, 1, "advances after the low nibble");
        assert!(ed.dirty);
    }

    #[test]
    fn ascii_column_overwrites_and_advances() {
        let mut ed = HexEdit::new("k".into(), vec![0x00, 0x00]);
        ed.on_key(key('i'));
        ed.on_key(code(KeyCode::Tab));
        ed.on_key(key('Z'));
        assert_eq!(ed.bytes, vec![0x5a, 0x00]);
        assert_eq!(ed.cursor, 1);
        // A non-printable key does nothing to the bytes.
        let before = ed.bytes.clone();
        ed.on_key(code(KeyCode::Enter));
        assert_eq!(ed.bytes, before);
    }

    #[test]
    fn nav_mode_does_not_edit() {
        let mut ed = HexEdit::new("k".into(), vec![0xff]);
        ed.on_key(key('4'));
        assert_eq!(ed.bytes, vec![0xff], "digits must not edit in nav mode");
        assert!(!ed.dirty);
    }

    #[test]
    fn motions_move_the_cursor_and_clamp() {
        let mut ed = HexEdit::new("k".into(), (0..40u8).collect());
        ed.on_key(key('l'));
        assert_eq!(ed.cursor, 1);
        ed.on_key(key('j'));
        assert_eq!(ed.cursor, 17, "j moves a full row");
        ed.on_key(key('0'));
        assert_eq!(ed.cursor, 16, "0 goes to the row start");
        ed.on_key(key('$'));
        assert_eq!(ed.cursor, 31, "$ goes to the row end");
        ed.on_key(key('G'));
        assert_eq!(ed.cursor, 39, "G clamps to the last byte");
        ed.on_key(key('l'));
        assert_eq!(ed.cursor, 39, "cannot move past the end");
        ed.on_key(key('g'));
        assert_eq!(ed.cursor, 0);
        ed.on_key(key('h'));
        assert_eq!(ed.cursor, 0);
    }

    /// zmax edits a fixed-length file; a value can grow and shrink.
    #[test]
    fn insert_and_delete_change_the_length() {
        let mut ed = HexEdit::new("k".into(), vec![0x11, 0x22]);
        ed.on_key(key('o'));
        assert_eq!(ed.bytes, vec![0x11, 0x00, 0x22], "o inserts after");
        assert_eq!(ed.cursor, 1, "and selects the new byte");
        ed.on_key(key('O'));
        assert_eq!(ed.bytes, vec![0x11, 0x00, 0x00, 0x22], "O inserts before");
        ed.on_key(key('x'));
        assert_eq!(ed.bytes, vec![0x11, 0x00, 0x22]);
        ed.on_key(key('x'));
        ed.on_key(key('x'));
        ed.on_key(key('x'));
        assert!(ed.bytes.is_empty(), "deleting past empty must not panic");
        ed.on_key(key('x'));
        assert!(ed.bytes.is_empty());
    }

    /// A record created with `a` starts empty; typing must still work.
    #[test]
    fn typing_into_an_empty_value_appends_the_first_byte() {
        let mut ed = HexEdit::new("k".into(), Vec::new());
        ed.on_key(key('i'));
        ed.on_key(key('7'));
        ed.on_key(key('f'));
        assert_eq!(ed.bytes, vec![0x7f]);
        assert!(ed.dirty);
    }

    #[test]
    fn ctrl_s_asks_the_caller_to_save_and_q_closes() {
        let mut ed = HexEdit::new("k".into(), vec![0x00]);
        assert_eq!(ed.on_key(ctrl('s')), Action::Save);
        assert_eq!(ed.on_key(key('q')), Action::Close, "clean buffer closes");
    }

    /// Ported dirty-quit guard: the first `q` warns, the second discards.
    #[test]
    fn dirty_quit_needs_two_presses() {
        let mut ed = HexEdit::new("k".into(), vec![0x00]);
        ed.on_key(key('i'));
        ed.on_key(key('f'));
        ed.on_key(code(KeyCode::Esc)); // leave edit mode
        assert!(ed.dirty);
        assert_eq!(ed.on_key(key('q')), Action::None, "first q warns");
        let r = rows(&mut ed, 80, 10);
        assert!(
            r.iter().any(|l| l.contains("unsaved edits")),
            "the warning must be visible"
        );
        assert_eq!(ed.on_key(key('q')), Action::Close, "second q discards");

        // Saving clears the flag, so one q is enough again.
        ed.dirty = true;
        ed.mark_saved();
        assert!(!ed.dirty);
        assert_eq!(ed.on_key(key('q')), Action::Close);
    }

    /// The arming must not survive an unrelated key in between.
    #[test]
    fn quit_arming_resets_on_another_key() {
        let mut ed = HexEdit::new("k".into(), vec![0x00, 0x11]);
        ed.on_key(key('i'));
        ed.on_key(key('f'));
        ed.on_key(code(KeyCode::Esc));
        assert_eq!(ed.on_key(key('q')), Action::None);
        ed.on_key(key('j'));
        assert_eq!(ed.on_key(key('q')), Action::None, "must warn again");
    }

    #[test]
    fn render_shows_offsets_bytes_ascii_and_header() {
        let mut ed = HexEdit::new("plugins/foo.zsh".into(), b"hello\0world".to_vec());
        let r = rows(&mut ed, 90, 10);
        assert!(r.iter().any(|l| l.contains("plugins/foo.zsh")), "label");
        assert!(r.iter().any(|l| l.contains("11 bytes")), "byte count");
        assert!(r.iter().any(|l| l.contains("NAV [hex]")), "mode");
        assert!(r.iter().any(|l| l.contains("00000000")), "offset gutter");
        assert!(r.iter().any(|l| l.contains("68 65 6c 6c 6f")), "hex cells");
        assert!(
            r.iter().any(|l| l.contains("|hello.world")),
            "ascii gutter with NUL as a dot"
        );

        // Entering edit mode is reflected in the header and the hint.
        ed.on_key(key('i'));
        let r = rows(&mut ed, 90, 10);
        assert!(r.iter().any(|l| l.contains("-- EDIT (hex) --")));
        assert!(r.iter().any(|l| l.contains("type to edit")));
    }

    #[test]
    fn dirty_marker_appears_in_the_header() {
        let mut ed = HexEdit::new("k".into(), vec![0x00]);
        assert!(!rows(&mut ed, 60, 8).iter().any(|l| l.contains("[+]")));
        ed.on_key(key('i'));
        ed.on_key(key('a'));
        assert!(rows(&mut ed, 60, 8).iter().any(|l| l.contains("[+]")));
    }

    /// A value longer than the viewport must scroll to follow the cursor.
    #[test]
    fn scrolling_follows_the_cursor() {
        let mut ed = HexEdit::new("k".into(), (0..=255u8).collect());
        rows(&mut ed, 90, 8); // 4 body rows
        assert_eq!(ed.scroll, 0);
        ed.on_key(key('G'));
        rows(&mut ed, 90, 8);
        assert_eq!(ed.cursor, 255);
        assert!(ed.scroll > 0, "the last row must be brought into view");
        let r = rows(&mut ed, 90, 8);
        assert!(
            r.iter().any(|l| l.contains("000000f0")),
            "the cursor's row must be drawn"
        );
        ed.on_key(key('g'));
        rows(&mut ed, 90, 8);
        assert_eq!(ed.scroll, 0);
    }

    /// Clicking a hex cell or an ASCII character selects that byte.
    #[test]
    fn click_places_the_cursor_in_either_column() {
        let mut ed = HexEdit::new("k".into(), (0..32u8).collect());
        let area = Rect::new(0, 0, 90, 10);
        rows(&mut ed, 90, 10);
        let click = |col: u16, row: u16| MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // Body starts at y=3 (border + 2 header rows). Third hex cell of row 0:
        // x = 1 (border) + 10 (offset gutter) + 2*3.
        ed.on_mouse(click(1 + 10 + 6, 3), area);
        assert_eq!(ed.cursor, 2);
        // Second row, fifth ASCII character.
        let ascii_start = 1 + 10 + 16 * 3 + 2;
        ed.on_mouse(click(ascii_start as u16 + 4, 4), area);
        assert_eq!(ed.cursor, BYTES_PER_ROW + 4);
        // A click in the gutter changes nothing.
        let before = ed.cursor;
        ed.on_mouse(click(2, 3), area);
        assert_eq!(ed.cursor, before);
    }
}
