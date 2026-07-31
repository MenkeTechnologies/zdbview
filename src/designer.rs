//! The schema designers: DB Browser for SQLite's "Edit Table Definition" and
//! "Edit Index" dialogs, as two full-screen forms.
//!
//! Both are grids of fields over a [`crate::ddl`] definition: the cursor moves by
//! row and by field, a boolean field toggles with Space, a text field opens a
//! [`crate::input::Line`] in place, and `W` hands the finished definition back to
//! the caller to apply. Nothing here touches the database — the form edits a
//! value, [`crate::ddl::plan`] turns the difference into statements, and the store
//! runs them, so what the designer does is testable by pressing keys at it.
//!
//! The SQL the edit will produce is on screen the whole time. DB4S shows the
//! same thing at the bottom of its dialog, and it is the only way to see what a
//! change to a constraint actually does before it is applied.

use crate::ddl::{ColumnDef, IndexColumn, IndexDef, TableDef};
use crate::input::{Edit, Line};
use crate::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// What a key asked the caller to do.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Action {
    /// The designer took the key.
    None,
    /// Esc — close without touching the database.
    Cancel,
    /// `W` — apply the definition. The caller reads it off the designer.
    Apply,
    /// Something for the status line: a rejected edit, usually.
    Note(String),
}

/// The columns of the table designer's grid, in cursor order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Type,
    Pk,
    Ai,
    NotNull,
    Unique,
    Default,
    Collate,
    Check,
    Fk,
}

impl Field {
    const ALL: [Field; 10] = [
        Field::Name,
        Field::Type,
        Field::Pk,
        Field::Ai,
        Field::NotNull,
        Field::Unique,
        Field::Default,
        Field::Collate,
        Field::Check,
        Field::Fk,
    ];

    /// Header text and column width. The flags are two characters wide, so a
    /// whole definition fits on an 80-column terminal.
    fn label(self) -> (&'static str, u16) {
        match self {
            Field::Name => ("name", 18),
            Field::Type => ("type", 12),
            Field::Pk => ("PK", 3),
            Field::Ai => ("AI", 3),
            Field::NotNull => ("NN", 3),
            Field::Unique => ("UQ", 3),
            Field::Default => ("default", 12),
            Field::Collate => ("collate", 9),
            Field::Check => ("check", 16),
            Field::Fk => ("references", 24),
        }
    }

    fn is_flag(self) -> bool {
        matches!(self, Field::Pk | Field::Ai | Field::NotNull | Field::Unique)
    }
}

/// The header row's fields — the table itself rather than one of its columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Head {
    Name,
    WithoutRowid,
    Strict,
}

impl Head {
    const ALL: [Head; 3] = [Head::Name, Head::WithoutRowid, Head::Strict];
}

/// DB Browser's Edit Table Definition dialog.
pub struct TableDesigner {
    /// The definition as it was read, or `None` when the table is new. The plan
    /// is the difference between this and `def`.
    pub original: Option<TableDef>,
    pub def: TableDef,
    /// 0 is the header row; 1.. are the columns.
    row: usize,
    field: usize,
    edit: Option<Line>,
    /// First column row on screen, so a wide table scrolls.
    scroll: usize,
    /// Column rows the last frame had space for, which is what paging moves by.
    page: usize,
}

impl TableDesigner {
    /// Edit an existing table.
    pub fn edit(def: TableDef) -> Self {
        TableDesigner {
            original: Some(def.clone()),
            def,
            row: 1,
            field: 0,
            edit: None,
            scroll: 0,
            page: 10,
        }
    }

    /// A new table, opened on its name with one column ready to fill in.
    pub fn create() -> Self {
        TableDesigner {
            original: None,
            def: TableDef {
                name: String::new(),
                columns: vec![ColumnDef::new("id", "INTEGER")],
                ..Default::default()
            },
            row: 0,
            field: 0,
            edit: None,
            scroll: 0,
            page: 10,
        }
    }

    /// The column the cursor is on, if it is not on the header row.
    fn col_idx(&self) -> Option<usize> {
        self.row
            .checked_sub(1)
            .filter(|i| *i < self.def.columns.len())
    }

    fn field_count(&self) -> usize {
        if self.row == 0 {
            Head::ALL.len()
        } else {
            Field::ALL.len()
        }
    }

    /// The text of the field under the cursor, for opening an editor on it.
    fn field_text(&self) -> Option<String> {
        match self.col_idx() {
            None => match Head::ALL.get(self.field)? {
                Head::Name => Some(self.def.name.clone()),
                _ => None,
            },
            Some(i) => {
                let c = self.def.columns.get(i)?;
                match Field::ALL.get(self.field)? {
                    Field::Name => Some(c.name.clone()),
                    Field::Type => Some(c.ty.clone()),
                    Field::Default => Some(c.default.clone()),
                    Field::Collate => Some(c.collate.clone()),
                    Field::Check => Some(c.check.clone()),
                    Field::Fk => Some(c.fk.clone()),
                    _ => None,
                }
            }
        }
    }

    /// Write an edited field back.
    fn set_field(&mut self, text: String) {
        let text = text.trim().to_string();
        match self.col_idx() {
            None => {
                if let Some(Head::Name) = Head::ALL.get(self.field) {
                    self.def.name = text;
                }
            }
            Some(i) => {
                let field = match Field::ALL.get(self.field) {
                    Some(f) => *f,
                    None => return,
                };
                let c = &mut self.def.columns[i];
                match field {
                    Field::Name => c.name = text,
                    Field::Type => c.ty = text.to_uppercase(),
                    Field::Default => c.default = text,
                    Field::Collate => c.collate = text,
                    Field::Check => c.check = text,
                    // A reference is stored as the whole clause, so a bare
                    // `parent(id)` is completed into one rather than rejected.
                    Field::Fk => {
                        c.fk = if text.is_empty() || text.to_uppercase().starts_with("REFERENCES") {
                            text
                        } else {
                            format!("REFERENCES {text}")
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Space on a flag: flip it, keeping the combinations SQLite allows.
    fn toggle(&mut self) -> Action {
        match self.col_idx() {
            None => {
                match Head::ALL.get(self.field) {
                    Some(Head::WithoutRowid) => self.def.without_rowid = !self.def.without_rowid,
                    Some(Head::Strict) => self.def.strict = !self.def.strict,
                    _ => return Action::None,
                }
                Action::None
            }
            Some(i) => {
                let field = match Field::ALL.get(self.field) {
                    Some(f) => *f,
                    None => return Action::None,
                };
                let c = &mut self.def.columns[i];
                match field {
                    Field::Pk => {
                        c.pk = !c.pk;
                        if !c.pk {
                            c.autoincrement = false;
                        }
                    }
                    Field::Ai => {
                        // AUTOINCREMENT is only legal on a single INTEGER key, so
                        // turning it on makes the column one rather than leaving
                        // an edit SQLite will reject.
                        c.autoincrement = !c.autoincrement;
                        if c.autoincrement {
                            c.pk = true;
                            c.ty = "INTEGER".into();
                            let me = c as *const ColumnDef;
                            for other in self.def.columns.iter_mut() {
                                if !std::ptr::eq(other as *const ColumnDef, me) {
                                    other.pk = false;
                                }
                            }
                        }
                    }
                    Field::NotNull => c.not_null = !c.not_null,
                    Field::Unique => c.unique = !c.unique,
                    _ => return Action::None,
                }
                Action::None
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // An open field editor owns the key until it commits or is cancelled.
        if let Some(line) = self.edit.as_mut() {
            match line.on_key(key) {
                Edit::Commit => {
                    let text = line.buf.clone();
                    self.edit = None;
                    self.set_field(text);
                }
                Edit::Cancel => self.edit = None,
                _ => {}
            }
            return Action::None;
        }

        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let last_row = self.def.columns.len();
        match key.code {
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Char('W') => return Action::Apply,
            KeyCode::Down | KeyCode::Char('j') if !shift => {
                self.row = (self.row + 1).min(last_row);
                self.field = self.field.min(self.field_count() - 1);
            }
            KeyCode::Up | KeyCode::Char('k') if !shift => {
                self.row = self.row.saturating_sub(1);
                self.field = self.field.min(self.field_count() - 1);
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.field = self.field.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Tab => {
                self.field = (self.field + 1).min(self.field_count() - 1);
            }
            KeyCode::Home => self.row = 0,
            KeyCode::End => self.row = last_row,
            KeyCode::PageDown => self.row = (self.row + self.page).min(last_row),
            KeyCode::PageUp => self.row = self.row.saturating_sub(self.page),
            KeyCode::Char(' ') => return self.toggle(),
            KeyCode::Enter | KeyCode::Char('e') => {
                if self.row > 0 && Field::ALL[self.field].is_flag() {
                    return self.toggle();
                }
                if self.row == 0 && !matches!(Head::ALL.get(self.field), Some(Head::Name)) {
                    return self.toggle();
                }
                match self.field_text() {
                    Some(t) => self.edit = Some(Line::at_end(&t)),
                    None => return Action::None,
                }
            }
            KeyCode::Char('a') => {
                let at = self.col_idx().map(|i| i + 1).unwrap_or(0);
                self.def.columns.insert(at, ColumnDef::new("", ""));
                self.row = at + 1;
                self.field = 0;
                self.edit = Some(Line::default());
            }
            KeyCode::Char('d') => match self.col_idx() {
                Some(i) if self.def.columns.len() > 1 => {
                    self.def.columns.remove(i);
                    self.row = self.row.min(self.def.columns.len());
                }
                Some(_) => return Action::Note("a table needs at least one column".into()),
                None => return Action::Note("the cursor is on the table, not a column".into()),
            },
            // Shift-J / Shift-K move a column, which is a rebuild — the one edit
            // ALTER TABLE cannot express.
            KeyCode::Char('J') => {
                if let Some(i) = self.col_idx() {
                    if i + 1 < self.def.columns.len() {
                        self.def.columns.swap(i, i + 1);
                        self.row += 1;
                    }
                }
            }
            KeyCode::Char('K') => {
                if let Some(i) = self.col_idx() {
                    if i > 0 {
                        self.def.columns.swap(i, i - 1);
                        self.row -= 1;
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    /// The statements this definition would run, or why it cannot.
    pub fn plan(&self, aux: &[crate::ddl::Dependent]) -> Result<crate::ddl::AlterPlan, String> {
        self.def.validate()?;
        Ok(match &self.original {
            None => crate::ddl::plan_create(&self.def),
            Some(old) => crate::ddl::plan(old, &self.def, aux),
        })
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, t: &Theme) {
        let sql = self.def.create_sql();
        let preview_height = (sql.lines().count() as u16 + 2).min(area.height / 2).max(3);
        let parts =
            Layout::vertical([Constraint::Min(5), Constraint::Length(preview_height)]).split(area);

        // Header row: the table's own settings.
        let mut lines: Vec<TextLine> = Vec::new();
        let head_fields: Vec<(String, bool)> = Head::ALL
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let text = match h {
                    Head::Name => format!("table: {}", self.shown(&self.def.name, 0, i)),
                    Head::WithoutRowid => {
                        format!("WITHOUT ROWID: {}", yes_no(self.def.without_rowid))
                    }
                    Head::Strict => format!("STRICT: {}", yes_no(self.def.strict)),
                };
                (text, self.row == 0 && self.field == i)
            })
            .collect();
        lines.push(TextLine::from(
            head_fields
                .into_iter()
                .flat_map(|(text, sel)| {
                    [
                        Span::styled(text, self.cell_style(sel, t)),
                        Span::raw("   "),
                    ]
                })
                .collect::<Vec<_>>(),
        ));
        lines.push(TextLine::from(""));

        // Column grid.
        let mut header: Vec<Span> = vec![Span::styled(
            format!("{:>3} ", "#"),
            Style::default().fg(t.dim),
        )];
        for fld in Field::ALL {
            let (label, w) = fld.label();
            header.push(Span::styled(
                format!("{:<w$} ", label, w = w as usize),
                Style::default().fg(t.alt).add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(TextLine::from(header));

        let body_height = parts[0].height.saturating_sub(5) as usize;
        self.page = body_height.max(1);
        // Keep the cursor's row on screen.
        let cur = self.row.saturating_sub(1);
        if cur < self.scroll {
            self.scroll = cur;
        } else if body_height > 0 && cur >= self.scroll + body_height {
            self.scroll = cur + 1 - body_height;
        }

        for (i, c) in self
            .def
            .columns
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(body_height)
        {
            let mut spans: Vec<Span> = vec![Span::styled(
                format!("{:>3} ", i + 1),
                Style::default().fg(t.dim),
            )];
            for (fi, fld) in Field::ALL.iter().enumerate() {
                let (_, w) = fld.label();
                let sel = self.row == i + 1 && self.field == fi;
                let raw = match fld {
                    Field::Name => self.shown(&c.name, i + 1, fi),
                    Field::Type => self.shown(&c.ty, i + 1, fi),
                    Field::Pk => flag(c.pk),
                    Field::Ai => flag(c.autoincrement),
                    Field::NotNull => flag(c.not_null),
                    Field::Unique => flag(c.unique),
                    Field::Default => self.shown(&c.default, i + 1, fi),
                    Field::Collate => self.shown(&c.collate, i + 1, fi),
                    Field::Check => self.shown(&c.check, i + 1, fi),
                    Field::Fk => self.shown(&c.fk, i + 1, fi),
                };
                spans.push(Span::styled(
                    format!("{:<w$} ", clip(&raw, w as usize), w = w as usize),
                    self.cell_style(sel, t),
                ));
            }
            lines.push(TextLine::from(spans));
        }

        let what = match &self.original {
            Some(o) => format!("edit table {}", o.name),
            None => "new table".to_string(),
        };
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!(
                " {what} — arrows move · Enter edits · Space toggles · a add · d drop · \
                     J/K reorder · W write · Esc cancel "
            ))),
            parts[0],
        );

        let preview: Vec<TextLine> = sql
            .lines()
            .map(|l| TextLine::from(Span::styled(l.to_string(), Style::default().fg(t.dim))))
            .collect();
        f.render_widget(
            Paragraph::new(preview).block(Block::default().borders(Borders::ALL).title(" SQL ")),
            parts[1],
        );
    }

    /// A field's text, showing the in-progress buffer when it is the one being
    /// edited so typing is visible where it lands.
    fn shown(&self, value: &str, row: usize, field: usize) -> String {
        match &self.edit {
            Some(l) if self.row == row && self.field == field => format!("{}_", l.buf),
            _ => value.to_string(),
        }
    }

    fn cell_style(&self, selected: bool, t: &Theme) -> Style {
        if !selected {
            return Style::default();
        }
        if self.edit.is_some() {
            Style::default().bg(t.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }
}

/// DB Browser's Edit Index dialog.
pub struct IndexDesigner {
    pub original: Option<IndexDef>,
    pub def: IndexDef,
    /// Column names of the indexed table, offered as the term to add.
    available: Vec<String>,
    /// 0 name, 1 table, 2 UNIQUE, 3 WHERE, 4.. the terms.
    row: usize,
    field: usize,
    edit: Option<Line>,
}

/// Rows above the term list.
const IDX_HEAD_ROWS: usize = 4;

impl IndexDesigner {
    pub fn edit(def: IndexDef, available: Vec<String>) -> Self {
        IndexDesigner {
            original: Some(def.clone()),
            def,
            available,
            row: 0,
            field: 0,
            edit: None,
        }
    }

    /// A new index on `table`, named after it the way DB4S seeds the dialog.
    pub fn create(table: &str, available: Vec<String>) -> Self {
        let first = available.first().cloned().unwrap_or_default();
        IndexDesigner {
            original: None,
            def: IndexDef {
                name: format!("idx_{table}"),
                table: table.to_string(),
                columns: vec![IndexColumn {
                    expr: first,
                    ..Default::default()
                }],
                ..Default::default()
            },
            available,
            row: 0,
            field: 0,
            edit: None,
        }
    }

    fn term_idx(&self) -> Option<usize> {
        self.row
            .checked_sub(IDX_HEAD_ROWS)
            .filter(|i| *i < self.def.columns.len())
    }

    fn field_text(&self) -> Option<String> {
        match self.term_idx() {
            None => match self.row {
                0 => Some(self.def.name.clone()),
                1 => Some(self.def.table.clone()),
                3 => Some(self.def.where_clause.clone()),
                _ => None,
            },
            Some(i) => {
                let c = self.def.columns.get(i)?;
                match self.field {
                    0 => Some(c.expr.clone()),
                    1 => Some(c.collate.clone()),
                    _ => None,
                }
            }
        }
    }

    fn set_field(&mut self, text: String) {
        let text = text.trim().to_string();
        match self.term_idx() {
            None => match self.row {
                0 => self.def.name = text,
                1 => self.def.table = text,
                3 => self.def.where_clause = text,
                _ => {}
            },
            Some(i) => match self.field {
                0 => self.def.columns[i].expr = text,
                1 => self.def.columns[i].collate = text,
                _ => {}
            },
        }
    }

    fn toggle(&mut self) {
        match self.term_idx() {
            None => {
                if self.row == 2 {
                    self.def.unique = !self.def.unique;
                }
            }
            Some(i) => {
                if self.field == 2 {
                    self.def.columns[i].desc = !self.def.columns[i].desc;
                }
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if let Some(line) = self.edit.as_mut() {
            match line.on_key(key) {
                Edit::Commit => {
                    let text = line.buf.clone();
                    self.edit = None;
                    self.set_field(text);
                }
                Edit::Cancel => self.edit = None,
                _ => {}
            }
            return Action::None;
        }
        let last = IDX_HEAD_ROWS + self.def.columns.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Char('W') => return Action::Apply,
            KeyCode::Down | KeyCode::Char('j') => self.row = (self.row + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.row = self.row.saturating_sub(1),
            KeyCode::Left | KeyCode::BackTab => self.field = self.field.saturating_sub(1),
            KeyCode::Right | KeyCode::Tab => {
                let n = if self.term_idx().is_some() { 3 } else { 1 };
                self.field = (self.field + 1).min(n - 1);
            }
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Enter | KeyCode::Char('e') => {
                if self.row == 2 || (self.term_idx().is_some() && self.field == 2) {
                    self.toggle();
                } else if let Some(t) = self.field_text() {
                    self.edit = Some(Line::at_end(&t));
                }
            }
            KeyCode::Char('a') => {
                // The next column of the table that is not indexed yet is the
                // useful default; failing that, an empty term to type into.
                let next = self
                    .available
                    .iter()
                    .find(|c| !self.def.columns.iter().any(|t| &&t.expr == c))
                    .cloned()
                    .unwrap_or_default();
                let at = self
                    .term_idx()
                    .map(|i| i + 1)
                    .unwrap_or(self.def.columns.len());
                self.def.columns.insert(
                    at,
                    IndexColumn {
                        expr: next,
                        ..Default::default()
                    },
                );
                self.row = IDX_HEAD_ROWS + at;
                self.field = 0;
            }
            KeyCode::Char('d') => match self.term_idx() {
                Some(i) if self.def.columns.len() > 1 => {
                    self.def.columns.remove(i);
                    self.row = self.row.min(IDX_HEAD_ROWS + self.def.columns.len() - 1);
                }
                Some(_) => return Action::Note("an index needs at least one column".into()),
                None => return Action::Note("the cursor is not on a column".into()),
            },
            KeyCode::Char('J') => {
                if let Some(i) = self.term_idx() {
                    if i + 1 < self.def.columns.len() {
                        self.def.columns.swap(i, i + 1);
                        self.row += 1;
                    }
                }
            }
            KeyCode::Char('K') => {
                if let Some(i) = self.term_idx() {
                    if i > 0 {
                        self.def.columns.swap(i, i - 1);
                        self.row -= 1;
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    /// The statements this index would run. Editing one is a drop and a create:
    /// SQLite has no `ALTER INDEX`, and that is what DB4S does too.
    pub fn plan(&self) -> Result<crate::ddl::AlterPlan, String> {
        self.def.validate()?;
        let mut statements = Vec::new();
        if let Some(old) = &self.original {
            statements.push(format!("DROP INDEX {}", crate::ddl::quote(&old.name)));
        }
        statements.push(self.def.create_sql());
        Ok(crate::ddl::AlterPlan {
            statements,
            rebuild: false,
        })
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, t: &Theme) {
        let sql = self.def.create_sql();
        let parts = Layout::vertical([Constraint::Min(5), Constraint::Length(3)]).split(area);
        let mut lines: Vec<TextLine> = Vec::new();

        let head = [
            ("name", self.shown(&self.def.name, 0, 0)),
            ("table", self.shown(&self.def.table, 1, 0)),
            ("UNIQUE", yes_no(self.def.unique).to_string()),
            ("WHERE", {
                let w = self.shown(&self.def.where_clause, 3, 0);
                if w.is_empty() {
                    "(none)".to_string()
                } else {
                    w
                }
            }),
        ];
        for (i, (label, value)) in head.iter().enumerate() {
            lines.push(TextLine::from(vec![
                Span::styled(format!("{:>8}  ", label), Style::default().fg(t.alt)),
                Span::styled(value.clone(), self.cell_style(self.row == i, t)),
            ]));
        }
        lines.push(TextLine::from(""));
        lines.push(TextLine::from(Span::styled(
            format!(
                "{:>3} {:<28} {:<10} {}",
                "#", "column or expression", "collate", "desc"
            ),
            Style::default().fg(t.alt).add_modifier(Modifier::BOLD),
        )));
        for (i, c) in self.def.columns.iter().enumerate() {
            let row = IDX_HEAD_ROWS + i;
            lines.push(TextLine::from(vec![
                Span::styled(format!("{:>3} ", i + 1), Style::default().fg(t.dim)),
                Span::styled(
                    format!("{:<28} ", clip(&self.shown(&c.expr, row, 0), 28)),
                    self.cell_style(self.row == row && self.field == 0, t),
                ),
                Span::styled(
                    format!("{:<10} ", clip(&self.shown(&c.collate, row, 1), 10)),
                    self.cell_style(self.row == row && self.field == 1, t),
                ),
                Span::styled(
                    flag(c.desc),
                    self.cell_style(self.row == row && self.field == 2, t),
                ),
            ]));
        }

        let what = match &self.original {
            Some(o) => format!("edit index {}", o.name),
            None => "new index".to_string(),
        };
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!(
                " {what} — arrows move · Enter edits · Space toggles · a add · d drop · \
                 J/K reorder · W write · Esc cancel "
            ))),
            parts[0],
        );
        f.render_widget(
            Paragraph::new(TextLine::from(Span::styled(
                sql,
                Style::default().fg(t.dim),
            )))
            .block(Block::default().borders(Borders::ALL).title(" SQL ")),
            parts[1],
        );
    }

    fn shown(&self, value: &str, row: usize, field: usize) -> String {
        match &self.edit {
            Some(l) if self.row == row && self.field == field => format!("{}_", l.buf),
            _ => value.to_string(),
        }
    }

    fn cell_style(&self, selected: bool, t: &Theme) -> Style {
        if !selected {
            return Style::default();
        }
        if self.edit.is_some() {
            Style::default().bg(t.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }
}

fn flag(on: bool) -> String {
    if on {
        "*".into()
    } else {
        "·".into()
    }
}

fn yes_no(on: bool) -> &'static str {
    if on {
        "yes"
    } else {
        "no"
    }
}

/// Cut a field's text to the column width, marking that it was cut.
fn clip(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::parse_table;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
    }

    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::empty())
    }

    fn type_text(d: &mut TableDesigner, text: &str) {
        for c in text.chars() {
            d.on_key(key(c));
        }
        d.on_key(code(KeyCode::Enter));
    }

    #[test]
    fn editing_a_column_name_plans_a_native_rename() {
        let def = parse_table("CREATE TABLE t (a TEXT, b TEXT)").unwrap();
        let mut d = TableDesigner::edit(def);
        // Row 1 is the first column, field 0 is its name.
        d.on_key(code(KeyCode::Enter));
        // The editor opened on the existing text; clear it and type a new name.
        d.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(&mut d, "a2");
        assert_eq!(d.def.columns[0].name, "a2");

        let plan = d.plan(&[]).unwrap();
        assert!(!plan.rebuild);
        assert_eq!(
            plan.statements,
            vec!["ALTER TABLE \"t\" RENAME COLUMN \"a\" TO \"a2\""]
        );
    }

    #[test]
    fn esc_in_a_field_leaves_the_definition_alone() {
        let def = parse_table("CREATE TABLE t (a TEXT)").unwrap();
        let mut d = TableDesigner::edit(def.clone());
        d.on_key(code(KeyCode::Enter));
        d.on_key(key('x'));
        d.on_key(code(KeyCode::Esc));
        assert_eq!(d.def, def, "a cancelled field changes nothing");
        // And Esc now closes the designer rather than the field.
        assert_eq!(d.on_key(code(KeyCode::Esc)), Action::Cancel);
    }

    #[test]
    fn autoincrement_makes_the_column_the_only_integer_key() {
        let def = parse_table("CREATE TABLE t (a TEXT PRIMARY KEY, b TEXT)").unwrap();
        let mut d = TableDesigner::edit(def);
        // Move to column 2, field AI, and turn it on.
        d.on_key(code(KeyCode::Down));
        for _ in 0..3 {
            d.on_key(code(KeyCode::Right));
        }
        d.on_key(key(' '));
        assert!(d.def.columns[1].autoincrement && d.def.columns[1].pk);
        assert_eq!(d.def.columns[1].ty, "INTEGER");
        assert!(!d.def.columns[0].pk, "the old key was cleared");
        assert!(d.def.validate().is_ok());
    }

    #[test]
    fn reordering_columns_plans_a_rebuild_that_keeps_the_data() {
        let def = parse_table("CREATE TABLE t (a TEXT, b TEXT)").unwrap();
        let mut d = TableDesigner::edit(def);
        d.on_key(key('J'));
        assert_eq!(d.def.columns[0].name, "b");
        let plan = d.plan(&[]).unwrap();
        assert!(plan.rebuild);
        let insert = plan
            .statements
            .iter()
            .find(|s| s.starts_with("INSERT INTO"))
            .unwrap();
        assert!(
            insert.contains("(\"b\", \"a\") SELECT \"b\", \"a\""),
            "{insert}"
        );
    }

    #[test]
    fn a_column_can_be_added_and_dropped() {
        let def = parse_table("CREATE TABLE t (a TEXT)").unwrap();
        let mut d = TableDesigner::edit(def);
        d.on_key(key('a'));
        type_text(&mut d, "b");
        assert_eq!(d.def.columns.len(), 2);
        assert_eq!(d.def.columns[1].name, "b");
        d.on_key(key('d'));
        assert_eq!(d.def.columns.len(), 1);
        // The last column cannot go: SQLite has no table without one.
        assert!(matches!(d.on_key(key('d')), Action::Note(_)));
    }

    #[test]
    fn a_new_table_is_planned_as_a_create() {
        let mut d = TableDesigner::create();
        // The cursor opens on the table name.
        d.on_key(code(KeyCode::Enter));
        type_text(&mut d, "fresh");
        let plan = d.plan(&[]).unwrap();
        assert!(!plan.rebuild);
        assert!(plan.statements[0].starts_with("CREATE TABLE \"fresh\""));
    }

    #[test]
    fn an_invalid_definition_is_reported_instead_of_planned() {
        let mut d = TableDesigner::create();
        assert!(d.plan(&[]).unwrap_err().contains("name"));
        d.on_key(code(KeyCode::Enter));
        type_text(&mut d, "ok");
        d.on_key(code(KeyCode::Down));
        d.on_key(code(KeyCode::Enter));
        d.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        d.on_key(code(KeyCode::Enter)); // an empty column name
        assert!(d.plan(&[]).unwrap_err().contains("column 1"));
    }

    #[test]
    fn a_bare_reference_is_completed_into_a_clause() {
        let def = parse_table("CREATE TABLE t (a TEXT)").unwrap();
        let mut d = TableDesigner::edit(def);
        // Field 9 is REFERENCES.
        for _ in 0..9 {
            d.on_key(code(KeyCode::Right));
        }
        d.on_key(code(KeyCode::Enter));
        type_text(&mut d, "parent(id)");
        assert_eq!(d.def.columns[0].fk, "REFERENCES parent(id)");
        assert!(d.def.create_sql().contains("REFERENCES parent(id)"));
    }

    #[test]
    fn editing_an_index_drops_and_recreates_it() {
        let idx = crate::ddl::parse_index("CREATE INDEX ix ON t (a)").unwrap();
        let mut d = IndexDesigner::edit(idx, vec!["a".into(), "b".into()]);
        d.on_key(key('a')); // add the next unindexed column
        assert_eq!(d.def.columns[1].expr, "b");
        d.on_key(code(KeyCode::Right));
        d.on_key(code(KeyCode::Right));
        d.on_key(key(' ')); // DESC
        assert!(d.def.columns[1].desc);
        let plan = d.plan().unwrap();
        assert_eq!(
            plan.statements,
            vec![
                "DROP INDEX \"ix\"",
                "CREATE INDEX \"ix\" ON \"t\" (\"a\", \"b\" DESC)",
            ]
        );
    }

    #[test]
    fn a_new_index_is_seeded_from_the_table() {
        let mut d = IndexDesigner::create("orders", vec!["sku".into(), "qty".into()]);
        assert_eq!(d.def.name, "idx_orders");
        assert_eq!(d.def.columns[0].expr, "sku");
        // Row 2 is UNIQUE.
        d.on_key(code(KeyCode::Down));
        d.on_key(code(KeyCode::Down));
        d.on_key(key(' '));
        assert!(d.def.unique);
        assert_eq!(
            d.plan().unwrap().statements,
            vec!["CREATE UNIQUE INDEX \"idx_orders\" ON \"orders\" (\"sku\")"]
        );
    }
}
