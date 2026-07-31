//! What the row grid shows: DB Browser's Browse Data column settings.
//!
//! Four things DB4S keeps per browsed table, and this module models the same
//! way: which columns are hidden, how many are frozen at the left edge, whether
//! the `rowid` is shown as a column of its own, and what display format each
//! column's values are put through.
//!
//! A display format is a SQL expression, not a rendering rule, which is how DB4S
//! does it too — its own dialog says a custom format "must contain a function
//! call applied to %1". That matters for more than fidelity: the grid receives
//! cells already turned into display strings, so a blob has become
//! `<blob 12 bytes>` long before anything here could format it. Asking SQLite for
//! `hex(col)` gets the bytes; asking the string does not.
//!
//! The one format DB4S offers that is not here is *SpatiaLite Geometry to SVG*,
//! which needs the SpatiaLite extension loaded to mean anything.

use crate::text::truncate as clip;
use std::collections::HashMap;

/// A display format applied to one column's values before they are shown.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Default,
    Decimal,
    Exponent,
    HexBlob,
    HexNumber,
    Octal,
    Round,
    Lower,
    Upper,
    /// `dd/mm/yyyy`, DB4S's one fixed date layout.
    DateDmy,
    /// A Julian day number, which is what SQLite's own date functions take.
    JulianDay,
    /// Seconds since 1970.
    UnixEpoch,
    UnixEpochLocal,
    /// Seconds since 2001, which is what Apple's `NSDate` counts.
    AppleNsDate,
    /// Milliseconds since 1970.
    JavaEpochMs,
    /// Microseconds since 1601, which is what Chromium's history stores.
    WebkitEpoch,
    WebkitEpochLocal,
    /// Days since 1899-12-30, the OLE automation date Windows uses.
    WindowsDate,
    /// Any SQL expression, with `%1` standing for the column — the placeholder
    /// DB4S's own dialog names.
    Custom(String),
}

impl Format {
    /// Every format the picker cycles through, in DB4S's order. `Custom` is not
    /// here: it is typed, not cycled.
    pub const CYCLE: &'static [Format] = &[
        Format::Default,
        Format::Decimal,
        Format::Exponent,
        Format::HexBlob,
        Format::HexNumber,
        Format::Octal,
        Format::Round,
        Format::Lower,
        Format::Upper,
        Format::DateDmy,
        Format::JulianDay,
        Format::UnixEpoch,
        Format::UnixEpochLocal,
        Format::AppleNsDate,
        Format::JavaEpochMs,
        Format::WebkitEpoch,
        Format::WebkitEpochLocal,
        Format::WindowsDate,
    ];

    pub fn label(&self) -> String {
        match self {
            Format::Default => "default".into(),
            Format::Decimal => "decimal number".into(),
            Format::Exponent => "exponent notation".into(),
            Format::HexBlob => "hex blob".into(),
            Format::HexNumber => "hex number".into(),
            Format::Octal => "octal number".into(),
            Format::Round => "round number".into(),
            Format::Lower => "lower case".into(),
            Format::Upper => "upper case".into(),
            Format::DateDmy => "date as dd/mm/yyyy".into(),
            Format::JulianDay => "julian day to date".into(),
            Format::UnixEpoch => "unix epoch to date".into(),
            Format::UnixEpochLocal => "unix epoch to local time".into(),
            Format::AppleNsDate => "apple NSDate to date".into(),
            Format::JavaEpochMs => "java epoch (ms) to date".into(),
            Format::WebkitEpoch => "webkit / chromium epoch to date".into(),
            Format::WebkitEpochLocal => "webkit / chromium epoch to local time".into(),
            Format::WindowsDate => "windows DATE to date".into(),
            Format::Custom(expr) => format!("custom: {expr}"),
        }
    }

    /// The SQL that produces this format's value. `col` arrives already quoted.
    ///
    /// The epoch conversions are all the same shape — shift the number onto the
    /// Unix epoch, then hand it to `datetime` — because that is the only date
    /// function SQLite has.
    pub fn expression(&self, col: &str) -> String {
        match self {
            Format::Default => col.to_string(),
            Format::Decimal => format!("printf('%d', {col})"),
            Format::Exponent => format!("printf('%e', {col})"),
            Format::HexBlob => format!("hex({col})"),
            Format::HexNumber => format!("printf('%x', {col})"),
            Format::Octal => format!("printf('%o', {col})"),
            Format::Round => format!("round({col})"),
            Format::Lower => format!("lower({col})"),
            Format::Upper => format!("upper({col})"),
            Format::DateDmy => format!("strftime('%d/%m/%Y', {col})"),
            // SQLite reads a bare number as a Julian day already.
            Format::JulianDay => format!("datetime({col})"),
            Format::UnixEpoch => format!("datetime({col}, 'unixepoch')"),
            Format::UnixEpochLocal => format!("datetime({col}, 'unixepoch', 'localtime')"),
            // NSDate counts from 2001-01-01, which is 978307200 Unix seconds.
            Format::AppleNsDate => format!("datetime({col} + 978307200, 'unixepoch')"),
            Format::JavaEpochMs => format!("datetime({col} / 1000, 'unixepoch')"),
            // Chromium counts microseconds from 1601-01-01, 11644473600 seconds
            // before the Unix epoch.
            Format::WebkitEpoch => {
                format!("datetime({col} / 1000000 - 11644473600, 'unixepoch')")
            }
            Format::WebkitEpochLocal => {
                format!("datetime({col} / 1000000 - 11644473600, 'unixepoch', 'localtime')")
            }
            // The OLE automation date counts days from 1899-12-30; 25569 of them
            // separate it from the Unix epoch.
            Format::WindowsDate => format!("datetime(({col} - 25569) * 86400, 'unixepoch')"),
            Format::Custom(expr) => expr.replace("%1", col),
        }
    }

    /// The next format in the cycle. A custom format cycles back to the start,
    /// since there is nothing after it.
    pub fn next(&self, back: bool) -> Format {
        let at = Format::CYCLE.iter().position(|f| f == self);
        let n = Format::CYCLE.len();
        let i = match at {
            Some(i) if back => (i + n - 1) % n,
            Some(i) => (i + 1) % n,
            None => 0,
        };
        Format::CYCLE[i].clone()
    }

    /// What is wrong with a custom format, if anything. DB4S rejects one that
    /// does not mention the column for the same reason: it would show the same
    /// value in every row.
    pub fn validate_custom(expr: &str) -> Result<(), String> {
        if expr.trim().is_empty() {
            return Err("a custom format needs an expression".into());
        }
        if !expr.contains("%1") {
            return Err("a custom format must apply something to %1 (the column)".into());
        }
        Ok(())
    }
}

/// What one table's grid is set to show. Kept per table, so going back to a
/// table finds it as it was left.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableView {
    /// Columns not shown, by name.
    pub hidden: Vec<String>,
    /// How many of the visible columns stay pinned to the left edge while the
    /// rest scroll.
    pub frozen: usize,
    /// Show the `rowid` as a leading column — DB4S's "Show rowid column".
    pub show_rowid: bool,
    /// Per-column display format; a column with none is [`Format::Default`].
    pub formats: HashMap<String, Format>,
}

impl TableView {
    pub fn is_hidden(&self, column: &str) -> bool {
        self.hidden.iter().any(|c| c == column)
    }

    /// Hide or show one column. The last visible column cannot be hidden — a grid
    /// with no columns is not a view of anything.
    pub fn toggle_hidden(&mut self, column: &str, total: usize) -> Result<bool, String> {
        if let Some(i) = self.hidden.iter().position(|c| c == column) {
            self.hidden.remove(i);
            return Ok(false);
        }
        if self.hidden.len() + 1 >= total {
            return Err("that is the last visible column".into());
        }
        self.hidden.push(column.to_string());
        Ok(true)
    }

    pub fn format(&self, column: &str) -> Format {
        self.formats.get(column).cloned().unwrap_or_default()
    }

    pub fn set_format(&mut self, column: &str, f: Format) {
        if f == Format::Default {
            self.formats.remove(column);
        } else {
            self.formats.insert(column.to_string(), f);
        }
    }

    /// The columns actually drawn, in order, as indexes into the table's full
    /// column list.
    pub fn visible(&self, columns: &[String]) -> Vec<usize> {
        columns
            .iter()
            .enumerate()
            .filter(|(_, c)| !self.is_hidden(c))
            .map(|(i, _)| i)
            .collect()
    }
}

/// Every table's settings, for as long as the file is open.
#[derive(Debug, Default)]
pub struct Browse {
    views: HashMap<String, TableView>,
}

impl Browse {
    pub fn view(&self, table: &str) -> TableView {
        self.views.get(table).cloned().unwrap_or_default()
    }

    pub fn view_mut(&mut self, table: &str) -> &mut TableView {
        self.views.entry(table.to_string()).or_default()
    }

    /// Every format set on `table`, as the page query wants them: column name to
    /// SQL expression. Empty when the table is showing itself plainly, which is
    /// the case the query path checks to keep its `SELECT *`.
    pub fn expressions(&self, table: &str) -> HashMap<String, Format> {
        self.views
            .get(table)
            .map(|v| v.formats.clone())
            .unwrap_or_default()
    }
}

/// Which of the visible columns fit on screen, given a cursor and the frozen
/// ones. Returns the column indexes to draw, in draw order.
///
/// The frozen columns are always drawn first; the rest scroll under the cursor.
/// `scroll` is the first non-frozen column currently at the left of the scrolling
/// area — it is passed in and returned so the grid does not jump around when the
/// cursor moves back and forth over the same columns.
pub fn layout(
    visible: &[usize],
    frozen: usize,
    cursor: usize,
    scroll: usize,
    fits: usize,
) -> (Vec<usize>, usize) {
    let fits = fits.max(1);
    let frozen = frozen.min(visible.len()).min(fits.saturating_sub(1));
    let scrolling = &visible[frozen..];
    if scrolling.is_empty() {
        return (visible[..frozen].to_vec(), 0);
    }
    let room = fits.saturating_sub(frozen).max(1);

    // Where the cursor sits among the scrolling columns, if it is one of them.
    let at = visible
        .iter()
        .position(|&c| c == cursor)
        .filter(|&i| i >= frozen)
        .map(|i| i - frozen);
    let mut scroll = scroll.min(scrolling.len().saturating_sub(1));
    if let Some(at) = at {
        if at < scroll {
            scroll = at;
        } else if at >= scroll + room {
            scroll = at + 1 - room;
        }
    }
    let mut out: Vec<usize> = visible[..frozen].to_vec();
    out.extend(scrolling.iter().skip(scroll).take(room).copied());
    (out, scroll)
}

/// What a key asked of the insert form.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum FormAction {
    None,
    Cancel,
    /// `W` — insert the row the form holds.
    Insert,
    Note(String),
}

/// DB Browser's "Insert Values": one row typed a column at a time, rather than
/// the blank row `a` inserts.
///
/// The distinction the form exists to keep is `NULL` against the empty string.
/// A column starts as `NULL` — what `INSERT ... DEFAULT VALUES` would leave —
/// and `n` puts it back there after something has been typed.
pub struct RowForm {
    pub table: String,
    pub columns: Vec<String>,
    /// One per column; `None` is `NULL`.
    pub values: Vec<Option<String>>,
    sel: usize,
    edit: Option<crate::input::Line>,
}

impl RowForm {
    pub fn new(table: &str, columns: Vec<String>) -> Self {
        RowForm {
            table: table.to_string(),
            values: vec![None; columns.len()],
            columns,
            sel: 0,
            edit: None,
        }
    }

    /// The values to insert, paired with their columns.
    pub fn pairs(&self) -> Vec<(String, Option<String>)> {
        self.columns
            .iter()
            .cloned()
            .zip(self.values.iter().cloned())
            .collect()
    }

    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) -> FormAction {
        use crossterm::event::KeyCode;
        if let Some(line) = self.edit.as_mut() {
            match line.on_key(key) {
                crate::input::Edit::Commit => {
                    let text = line.buf.clone();
                    self.edit = None;
                    self.values[self.sel] = Some(text);
                }
                crate::input::Edit::Cancel => self.edit = None,
                _ => {}
            }
            return FormAction::None;
        }
        let last = self.columns.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => FormAction::Cancel,
            KeyCode::Char('W') => FormAction::Insert,
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(last);
                FormAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = self.sel.saturating_sub(1);
                FormAction::None
            }
            KeyCode::Home => {
                self.sel = 0;
                FormAction::None
            }
            KeyCode::End => {
                self.sel = last;
                FormAction::None
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                let seed = self.values[self.sel].clone().unwrap_or_default();
                self.edit = Some(crate::input::Line::at_end(&seed));
                FormAction::None
            }
            KeyCode::Char('n') => {
                self.values[self.sel] = None;
                FormAction::Note(format!("{} = NULL", self.columns[self.sel]))
            }
            _ => FormAction::None,
        }
    }

    pub fn render(
        &mut self,
        f: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        t: &crate::theme::Theme,
    ) {
        use ratatui::style::{Modifier, Style};
        use ratatui::text::{Line as TextLine, Span};
        use ratatui::widgets::{Block, Borders, Paragraph};

        let height = area.height.saturating_sub(2) as usize;
        let first = self.sel.saturating_sub(height.saturating_sub(1));
        let lines: Vec<TextLine> = self
            .columns
            .iter()
            .enumerate()
            .skip(first)
            .take(height)
            .map(|(i, c)| {
                let selected = i == self.sel;
                let shown = match (&self.edit, &self.values[i]) {
                    (Some(l), _) if selected => format!("{}_", l.buf),
                    (_, Some(v)) => v.clone(),
                    (_, None) => "NULL".to_string(),
                };
                let value_style = match (&self.values[i], selected) {
                    (_, true) => Style::default().add_modifier(Modifier::REVERSED),
                    (None, false) => Style::default().fg(t.dim),
                    (Some(_), false) => Style::default().fg(t.primary),
                };
                TextLine::from(vec![
                    Span::styled(
                        format!("{} {:<24} ", if selected { "▸" } else { " " }, c),
                        Style::default().fg(t.alt),
                    ),
                    Span::styled(clip(&shown, 48), value_style),
                ])
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!(
                " insert into {} — Enter types · n sets NULL · W inserts · Esc cancels ",
                self.table
            ))),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("c{i}")).collect()
    }

    #[test]
    fn every_format_produces_sql_that_mentions_the_column() {
        for f in Format::CYCLE {
            let sql = f.expression("\"v\"");
            assert!(sql.contains("\"v\""), "{}: {sql}", f.label());
            assert!(!f.label().is_empty());
        }
        assert_eq!(Format::Default.expression("\"v\""), "\"v\"");
        assert_eq!(Format::HexBlob.expression("\"v\""), "hex(\"v\")");
        assert_eq!(
            Format::UnixEpochLocal.expression("\"v\""),
            "datetime(\"v\", 'unixepoch', 'localtime')"
        );
    }

    #[test]
    fn a_custom_format_substitutes_the_column_for_the_placeholder() {
        let f = Format::Custom("substr(%1, 1, 3)".into());
        assert_eq!(f.expression("\"v\""), "substr(\"v\", 1, 3)");
        assert!(Format::validate_custom("substr(%1, 1, 3)").is_ok());
        assert!(Format::validate_custom("substr(v, 1, 3)")
            .unwrap_err()
            .contains("%1"));
        assert!(Format::validate_custom("  ").is_err());
    }

    #[test]
    fn the_cycle_wraps_both_ways_and_leaves_custom_behind() {
        assert_eq!(Format::Default.next(false), Format::Decimal);
        assert_eq!(Format::Default.next(true), *Format::CYCLE.last().unwrap());
        assert_eq!(
            Format::CYCLE.last().unwrap().next(false),
            Format::Default,
            "the cycle wraps"
        );
        // A custom format is not in the cycle, so stepping lands at the start.
        assert_eq!(Format::Custom("x".into()).next(false), Format::Default);
    }

    #[test]
    fn hiding_stops_at_the_last_visible_column() {
        let mut v = TableView::default();
        assert_eq!(v.toggle_hidden("c0", 3), Ok(true));
        assert_eq!(v.toggle_hidden("c1", 3), Ok(true));
        assert!(v.toggle_hidden("c2", 3).is_err(), "one must be left");
        assert_eq!(v.toggle_hidden("c0", 3), Ok(false), "and it unhides");
        assert_eq!(v.visible(&cols(3)), vec![0, 2]);
    }

    #[test]
    fn setting_a_column_back_to_default_forgets_it() {
        let mut v = TableView::default();
        v.set_format("a", Format::HexBlob);
        assert_eq!(v.format("a"), Format::HexBlob);
        v.set_format("a", Format::Default);
        assert!(v.formats.is_empty(), "default is the absence of a format");
        assert_eq!(v.format("a"), Format::Default);
    }

    #[test]
    fn the_layout_scrolls_the_cursor_into_view_and_keeps_the_frozen_columns() {
        let visible: Vec<usize> = (0..10).collect();
        // No frozen columns: the window follows the cursor.
        let (drawn, scroll) = layout(&visible, 0, 0, 0, 4);
        assert_eq!(drawn, vec![0, 1, 2, 3]);
        assert_eq!(scroll, 0);
        let (drawn, scroll) = layout(&visible, 0, 6, 0, 4);
        assert_eq!(drawn, vec![3, 4, 5, 6], "scrolled just far enough");
        assert_eq!(scroll, 3);
        // Stepping back one column scrolls back by one, not to the start.
        let (drawn, _) = layout(&visible, 0, 2, scroll, 4);
        assert_eq!(drawn, vec![2, 3, 4, 5]);

        // Two frozen columns stay put while the rest scroll under the cursor.
        let (drawn, _) = layout(&visible, 2, 8, 0, 4);
        assert_eq!(drawn, vec![0, 1, 7, 8]);
        // A cursor inside the frozen span does not move the window.
        let (drawn, scroll) = layout(&visible, 2, 0, 5, 4);
        assert_eq!(drawn, vec![0, 1, 7, 8]);
        assert_eq!(scroll, 5);
    }

    #[test]
    fn the_insert_form_keeps_null_apart_from_the_empty_string() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty());
        let code = |c: KeyCode| KeyEvent::new(c, KeyModifiers::empty());

        let mut form = RowForm::new("t", vec!["a".into(), "b".into()]);
        assert_eq!(form.pairs(), vec![("a".into(), None), ("b".into(), None)]);

        // Typing nothing at all is an empty string, which is not NULL.
        form.on_key(code(KeyCode::Enter));
        form.on_key(code(KeyCode::Enter));
        assert_eq!(form.values[0], Some(String::new()));

        form.on_key(code(KeyCode::Enter));
        for c in "hi".chars() {
            form.on_key(key(c));
        }
        form.on_key(code(KeyCode::Enter));
        assert_eq!(form.values[0], Some("hi".into()));

        // And `n` puts it back to NULL.
        assert!(matches!(form.on_key(key('n')), FormAction::Note(_)));
        assert_eq!(form.values[0], None);

        assert_eq!(form.on_key(key('W')), FormAction::Insert);
        assert_eq!(form.on_key(code(KeyCode::Esc)), FormAction::Cancel);
    }

    #[test]
    fn the_layout_survives_a_narrow_grid_and_a_short_table() {
        // Room for one column, all of them frozen: the freeze is clamped so the
        // cursor's column can still be drawn.
        let visible: Vec<usize> = (0..5).collect();
        let (drawn, _) = layout(&visible, 5, 4, 0, 1);
        assert_eq!(drawn.len(), 1);
        // Fewer columns than fit: everything is drawn, nothing scrolls.
        let (drawn, scroll) = layout(&[0, 1], 0, 1, 0, 8);
        assert_eq!(drawn, vec![0, 1]);
        assert_eq!(scroll, 0);
        // No columns at all is not a panic.
        assert_eq!(layout(&[], 0, 0, 0, 4), (vec![], 0));
    }
}
