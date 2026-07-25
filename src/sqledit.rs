//! The SQL editor: a multi-line editor with a transcript, history and Tab
//! completion, ported from zmax's REPL panel (`zmax-term/src/ui/repl.rs`).
//!
//! The old `:` prompt was a single line that reported only an affected-row count,
//! so a `SELECT` looked like it had done nothing. This is the same shape as zmax's
//! REPL instead: an input buffer that may span lines, a scrollback transcript of
//! what was run and what came back, and per-session history.
//!
//! Ported from that panel: `input: Vec<char>` with a character cursor, Enter
//! submits while `Alt-Enter` (or `Ctrl-j`, which every terminal delivers) inserts a
//! newline, `↑`/`↓` and `Ctrl-p`/`Ctrl-n` browse history with the in-progress line
//! stashed, `Ctrl-l` clears the transcript, `Ctrl-g` clears the input, the readline
//! chords (`Ctrl-a`/`e`/`b`/`f`/`k`/`u`/`w`) move and kill, `PgUp`/`PgDn` scroll
//! the transcript, and the caret is computed during render.
//!
//! Added here, because zmax gets it from an LSP and zdbview has the schema in
//! hand: `Tab` completes the word before the cursor against the table names, the
//! columns of the tables the statement mentions, and the SQL keywords — ranked in
//! that order, since a name is a likelier next token than a keyword.
//!
//! Execution stays with the caller: [`Action::Execute`] hands the statement out and
//! [`SqlEdit::push`] feeds the result back, so this file holds no database access.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::theme::Theme;

/// Transcript entries kept before the oldest are dropped.
const MAX_TRANSCRIPT: usize = 400;
/// Statements remembered across sessions.
const MAX_HISTORY: usize = 200;
/// Rows shown per result; the caller caps what it materialises to match.
pub const RESULT_ROWS: usize = 500;
/// Completion candidates offered at once.
const MAX_CANDIDATES: usize = 12;

/// The SQL keywords offered by completion, upper-case as SQL is conventionally
/// written. Not the full grammar — the ones actually typed at a prompt.
/// What the cursor can legally complete to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ctx {
    /// The start of a statement.
    Start,
    /// A table name is due (after `FROM`, `JOIN`, `INTO`, `UPDATE`, `TABLE`).
    Table,
    /// A column is due, from the tables in scope.
    Column(Scope),
    /// `table.` or `alias.` — that table's columns, by schema index.
    Qualified(usize),
    /// A pragma name is due.
    Pragma,
    /// Nothing can be completed here.
    Nothing,
}

/// The tables a statement has named, and any aliases for them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Scope {
    /// Indices into the schema, in the order the statement named them.
    tables: Vec<usize>,
    aliases: Vec<(String, usize)>,
}

impl Scope {
    /// The table a qualifier refers to: an alias, or a table named outright.
    fn resolve(&self, name: &str, schema: &[(String, Vec<String>)]) -> Option<usize> {
        if let Some((_, idx)) = self
            .aliases
            .iter()
            .find(|(a, _)| a.eq_ignore_ascii_case(name))
        {
            return Some(*idx);
        }
        schema
            .iter()
            .position(|(t, _)| t.eq_ignore_ascii_case(name))
    }
}

/// Words that open a clause: used to find the clause governing the cursor, and to
/// tell an alias from a keyword.
const CLAUSES: &[&str] = &[
    "SELECT", "FROM", "WHERE", "SET", "VALUES", "JOIN", "ON", "BY", "HAVING", "LIMIT", "OFFSET",
    "INTO", "UPDATE", "DELETE", "INSERT", "CREATE", "DROP", "ALTER", "PRAGMA", "WITH", "UNION",
];

/// Keywords a statement can begin with.
const STATEMENTS: &[&str] = &[
    "SELECT",
    "INSERT INTO",
    "UPDATE",
    "DELETE FROM",
    "CREATE TABLE",
    "CREATE INDEX",
    "CREATE VIEW",
    "DROP TABLE",
    "DROP INDEX",
    "DROP VIEW",
    "ALTER TABLE",
    "PRAGMA",
    "EXPLAIN",
    "EXPLAIN QUERY PLAN",
    "WITH",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "VACUUM",
    "ANALYZE",
    "ATTACH",
    "DETACH",
    "REINDEX",
];

/// Words that continue a clause once a column has been named.
const CLAUSE_WORDS: &[&str] = &[
    "AS",
    "AND",
    "OR",
    "NOT",
    "IS NULL",
    "IS NOT NULL",
    "IN",
    "LIKE",
    "GLOB",
    "BETWEEN",
    "NULL",
    "FROM",
    "WHERE",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "ASC",
    "DESC",
    "DISTINCT",
    "JOIN",
    "LEFT JOIN",
    "ON",
    "UNION ALL",
    "COLLATE NOCASE",
];

/// The aggregate and scalar functions worth offering.
const FUNCTIONS: &[&str] = &[
    "COUNT(*)",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "TOTAL",
    "GROUP_CONCAT",
    "LENGTH",
    "LOWER",
    "UPPER",
    "SUBSTR",
    "REPLACE",
    "COALESCE",
    "IFNULL",
    "CAST",
    "ROUND",
    "ABS",
    "HEX",
    "QUOTE",
    "DATETIME",
    "STRFTIME",
];

/// The pragmas typed at a prompt often enough to offer.
const PRAGMAS: &[&str] = &[
    "table_info",
    "table_list",
    "index_list",
    "index_info",
    "foreign_key_list",
    "journal_mode",
    "wal_checkpoint",
    "synchronous",
    "page_size",
    "page_count",
    "freelist_count",
    "cache_size",
    "integrity_check",
    "quick_check",
    "optimize",
    "user_version",
    "schema_version",
    "auto_vacuum",
    "encoding",
    "busy_timeout",
    "compile_options",
    "database_list",
];

/// Every keyword as a single word, for telling an alias from a keyword.
const KEYWORD_WORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IN",
    "LIKE",
    "GLOB",
    "BETWEEN",
    "ORDER",
    "GROUP",
    "BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "ASC",
    "DESC",
    "DISTINCT",
    "AS",
    "ON",
    "JOIN",
    "LEFT",
    "INNER",
    "CROSS",
    "UNION",
    "ALL",
    "EXCEPT",
    "INTERSECT",
    "SET",
    "VALUES",
    "INTO",
    "UPDATE",
    "DELETE",
    "INSERT",
    "CREATE",
    "DROP",
    "ALTER",
    "TABLE",
    "INDEX",
    "VIEW",
    "PRAGMA",
    "EXPLAIN",
    "WITH",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "VACUUM",
    "ANALYZE",
    "USING",
];

/// The identifier-ish words of `text`, in order.
fn words_before(text: &[char]) -> Vec<String> {
    let s: String = text.iter().collect();
    s.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '*')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect()
}

/// What one key asked the host to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Stay in the editor.
    None,
    /// Leave it.
    Close,
    /// Run this statement and report back with [`SqlEdit::push`].
    Execute(String),
    /// Show this statement's query plan instead of running it (Alt-e / F5, the
    /// shell's `.eqp`).
    Explain(String),
}

/// One transcript entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// The statement as it was submitted.
    Sql(String),
    /// A result set.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        truncated: bool,
    },
    /// Rows changed by a statement that returns none.
    Changed(usize),
    /// How long the last statement took, as the shell's `.timer` reports it.
    Timing(std::time::Duration),
    /// A query plan, one step per line.
    Plan(Vec<String>),
    /// The message SQLite refused with.
    Error(String),
}

/// Candidates offered for the word under the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Completion {
    /// Char index where the completed word starts.
    at: usize,
    /// The prefix being completed, as typed.
    prefix: String,
    items: Vec<String>,
    selected: usize,
}

/// The editor: input, transcript, history, completion, and the schema it
/// completes against.
pub struct SqlEdit {
    input: Vec<char>,
    cursor: usize,
    transcript: Vec<Entry>,
    scroll: usize,
    /// Stick to the newest transcript output as it arrives.
    follow: bool,
    history: Vec<String>,
    /// `None` = editing fresh input; `Some(i)` = browsing history at `i`.
    hist_idx: Option<usize>,
    /// In-progress input stashed while browsing history.
    stash: Vec<char>,
    completion: Option<Completion>,
    /// `(table, columns)` for every table and view, for completion.
    schema: Vec<(String, Vec<String>)>,
    /// Set while a statement is out with the host.
    pub running: bool,
}

impl SqlEdit {
    pub fn new(schema: Vec<(String, Vec<String>)>) -> Self {
        SqlEdit {
            input: Vec::new(),
            cursor: 0,
            transcript: Vec::new(),
            scroll: 0,
            follow: true,
            history: load_history(),
            hist_idx: None,
            stash: Vec::new(),
            completion: None,
            schema,
            running: false,
        }
    }

    /// The statement as typed.
    pub fn text(&self) -> String {
        self.input.iter().collect()
    }

    /// Record what running the last statement produced.
    pub fn push(&mut self, entry: Entry) {
        self.transcript.push(entry);
        while self.transcript.len() > MAX_TRANSCRIPT {
            self.transcript.remove(0);
        }
        self.running = false;
        self.follow = true;
    }

    /// Transcript entries, oldest first. Only the tests read it — the editor
    /// renders from the field directly.
    #[cfg(test)]
    pub fn transcript(&self) -> &[Entry] {
        &self.transcript
    }

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += 1;
        self.hist_idx = None;
        // Any edit invalidates the offered candidates.
        self.completion = None;
    }

    fn submit(&mut self) -> Action {
        let sql = self.text().trim().to_string();
        if sql.is_empty() {
            return Action::None;
        }
        self.transcript.push(Entry::Sql(sql.clone()));
        // Newest last, no duplicate of the previous statement.
        if self.history.last().map(String::as_str) != Some(sql.as_str()) {
            self.history.push(sql.clone());
            while self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
            save_history(&self.history);
        }
        self.input.clear();
        self.cursor = 0;
        self.hist_idx = None;
        self.completion = None;
        self.running = true;
        self.follow = true;
        Action::Execute(sql)
    }

    /// Browse history, stashing whatever was being typed (zmax's `history_move`).
    fn history_move(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        match (self.hist_idx, back) {
            (None, true) => {
                self.stash = std::mem::take(&mut self.input);
                self.hist_idx = Some(self.history.len() - 1);
            }
            (Some(0), true) => return,
            (Some(i), true) => self.hist_idx = Some(i - 1),
            (Some(i), false) if i + 1 < self.history.len() => self.hist_idx = Some(i + 1),
            (Some(_), false) => {
                // Past the newest entry: back to what was being typed.
                self.hist_idx = None;
                self.input = std::mem::take(&mut self.stash);
                self.cursor = self.input.len();
                return;
            }
            (None, false) => return,
        }
        if let Some(i) = self.hist_idx {
            self.input = self.history[i].chars().collect();
            self.cursor = self.input.len();
        }
        self.completion = None;
    }

    /// Kill the word before the cursor (`Ctrl-w`).
    fn kill_word(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.input[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.input[i - 1].is_whitespace() {
            i -= 1;
        }
        self.input.drain(i..self.cursor);
        self.cursor = i;
        self.hist_idx = None;
        self.completion = None;
    }

    /// Offer completions for the word before the cursor, or advance through the
    /// ones already offered.
    /// Ask for the current statement's query plan. Nothing typed, nothing to
    /// explain.
    fn explain(&mut self) -> Action {
        let sql = self.text().trim().to_string();
        if sql.is_empty() {
            return Action::None;
        }
        self.transcript
            .push(Entry::Sql(format!("EXPLAIN QUERY PLAN {sql}")));
        self.follow = true;
        Action::Explain(sql)
    }

    fn complete(&mut self, back: bool) {
        if let Some(c) = self.completion.as_mut() {
            let n = c.items.len();
            c.selected = if back {
                (c.selected + n - 1) % n
            } else {
                (c.selected + 1) % n
            };
            return;
        }
        let (at, prefix) = self.word_before_cursor();
        let items = self.candidates_at(&prefix, at);
        if items.is_empty() {
            return;
        }
        // A single candidate needs no menu: take it.
        if items.len() == 1 {
            self.replace_word(at, &items[0]);
            return;
        }
        self.completion = Some(Completion {
            at,
            prefix,
            items,
            selected: 0,
        });
    }

    /// Accept the highlighted candidate.
    fn accept_completion(&mut self) {
        if let Some(c) = self.completion.take() {
            let pick = c.items[c.selected].clone();
            self.replace_word(c.at, &pick);
        }
    }

    /// Swap the word starting at `at` for `word`, leaving the cursor after it.
    fn replace_word(&mut self, at: usize, word: &str) {
        self.input.drain(at..self.cursor);
        for (i, ch) in word.chars().enumerate() {
            self.input.insert(at + i, ch);
        }
        self.cursor = at + word.chars().count();
        self.completion = None;
        self.hist_idx = None;
    }

    /// `(start, text)` of the identifier the cursor sits at the end of.
    fn word_before_cursor(&self) -> (usize, String) {
        let mut at = self.cursor;
        while at > 0 {
            let c = self.input[at - 1];
            if c.is_alphanumeric() || c == '_' {
                at -= 1;
            } else {
                break;
            }
        }
        (at, self.input[at..self.cursor].iter().collect())
    }

    /// Candidates for `prefix`, chosen by what SQL allows at the cursor. The
    /// editor itself goes through [`Self::candidates_at`], which already knows
    /// where the word starts; this is the form the tests read.
    #[cfg(test)]
    fn candidates(&self, prefix: &str) -> Vec<String> {
        let at = self.cursor - prefix.chars().count();
        self.candidates_at(prefix, at)
    }

    /// [`Self::candidates`] with the word's start given, for the completion path
    /// that has already computed it.
    fn candidates_at(&self, prefix: &str, at: usize) -> Vec<String> {
        let p = prefix.to_lowercase();
        let mut out: Vec<String> = Vec::new();
        let push = |s: &str, out: &mut Vec<String>| {
            if (p.is_empty() || s.to_lowercase().starts_with(&p))
                && !out.iter().any(|o| o.eq_ignore_ascii_case(s))
            {
                out.push(s.to_string());
            }
        };
        let tables = |out: &mut Vec<String>| {
            for (t, _) in &self.schema {
                push(t, out);
            }
        };

        match self.context(at) {
            // Nothing legal can follow an unknown qualifier.
            Ctx::Nothing => {}
            // Only a statement can start here.
            Ctx::Start => {
                for k in STATEMENTS {
                    push(k, &mut out);
                }
            }
            Ctx::Pragma => {
                for k in PRAGMAS {
                    push(k, &mut out);
                }
            }
            Ctx::Table => tables(&mut out),
            Ctx::Qualified(t) => {
                for c in &self.schema[t].1 {
                    push(c.as_str(), &mut out);
                }
            }
            Ctx::Column(scope) => {
                // The columns of the tables this statement named, in that order.
                for &t in &scope.tables {
                    for c in &self.schema[t].1 {
                        push(c.as_str(), &mut out);
                    }
                }
                for (alias, _) in &scope.aliases {
                    push(alias, &mut out);
                }
                // With no table named yet, any column is fair game.
                if scope.tables.is_empty() {
                    for (_, cols) in &self.schema {
                        for c in cols {
                            push(c.as_str(), &mut out);
                        }
                    }
                }
                // Then what continues the clause, then table names, then functions:
                // a column is what belongs here, so it leads.
                for k in CLAUSE_WORDS {
                    push(k, &mut out);
                }
                tables(&mut out);
                for k in FUNCTIONS {
                    push(k, &mut out);
                }
            }
        }
        out.truncate(MAX_CANDIDATES);
        out
    }

    /// What the cursor is positioned to complete. SQL says which kind of name can
    /// legally come next, so the candidates follow the clause rather than a fixed
    /// ranking: `FROM ` can only take a table, `WHERE ` can only take a column of
    /// the tables in scope, `t.` only that table's columns, and the start of a
    /// statement only a statement keyword.
    fn context(&self, at: usize) -> Ctx {
        // `t.` or `alias.` — only that table's columns are possible.
        if at > 0 && self.input[at - 1] == '.' {
            let mut start = at - 1;
            while start > 0 {
                let c = self.input[start - 1];
                if c.is_alphanumeric() || c == '_' {
                    start -= 1;
                } else {
                    break;
                }
            }
            let name: String = self.input[start..at - 1].iter().collect();
            let scope = self.scope(at);
            if let Some(t) = scope.resolve(&name, &self.schema) {
                return Ctx::Qualified(t);
            }
            // An unknown qualifier means no candidate can be right.
            return Ctx::Nothing;
        }

        let words = words_before(&self.input[..at]);
        let last = words.last().map(|w| w.to_ascii_uppercase());
        let last = last.as_deref().unwrap_or("");
        // The clause keyword governing the position, scanning back.
        let clause = words
            .iter()
            .rev()
            .map(|w| w.to_ascii_uppercase())
            .find(|w| CLAUSES.contains(&w.as_str()));

        // Nothing typed yet, or right after a statement separator: a statement
        // keyword is the only thing that can start here.
        let after_semicolon = self.input[..at]
            .iter()
            .rev()
            .find(|c| !c.is_whitespace())
            .map(|&c| c == ';')
            .unwrap_or(true);
        if words.is_empty() || after_semicolon {
            return Ctx::Start;
        }
        if last == "PRAGMA" {
            return Ctx::Pragma;
        }
        if matches!(last, "FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE") {
            return Ctx::Table;
        }
        let scope = self.scope(at);
        match clause.as_deref() {
            // A column belongs everywhere else in these clauses.
            Some("SELECT") | Some("WHERE") | Some("SET") | Some("ON") | Some("BY")
            | Some("HAVING") | Some("VALUES") | None => Ctx::Column(scope),
            _ => Ctx::Column(scope),
        }
    }

    /// The tables (and their aliases) a statement has brought into scope, read from
    /// its `FROM`/`JOIN`/`UPDATE`/`INTO` clauses. Columns are then offered from
    /// these, in the order they were named, rather than from every table in the
    /// database.
    fn scope(&self, at: usize) -> Scope {
        let words = words_before(&self.input[..at]);
        let mut scope = Scope::default();
        let mut expect = false;
        for (i, w) in words.iter().enumerate() {
            let upper = w.to_ascii_uppercase();
            if matches!(upper.as_str(), "FROM" | "JOIN" | "UPDATE" | "INTO") {
                expect = true;
                continue;
            }
            if !expect {
                continue;
            }
            expect = false;
            if let Some(idx) = self
                .schema
                .iter()
                .position(|(t, _)| t.eq_ignore_ascii_case(w))
            {
                if !scope.tables.contains(&idx) {
                    scope.tables.push(idx);
                }
                // `FROM t alias` or `FROM t AS alias`.
                let mut n = i + 1;
                if words.get(n).map(|w| w.eq_ignore_ascii_case("AS")) == Some(true) {
                    n += 1;
                }
                if let Some(alias) = words.get(n) {
                    let upper = alias.to_ascii_uppercase();
                    if !CLAUSES.contains(&upper.as_str())
                        && !KEYWORD_WORDS.contains(&upper.as_str())
                        && !self
                            .schema
                            .iter()
                            .any(|(t, _)| t.eq_ignore_ascii_case(alias))
                    {
                        scope.aliases.push((alias.to_string(), idx));
                    }
                }
            }
        }
        scope
    }

    /// Handle one key. `page` is a screenful of transcript, for `PgUp`/`PgDn`.
    pub fn on_key(&mut self, key: KeyEvent, page: usize) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // While candidates are on screen they take the keys that drive them.
        if self.completion.is_some() {
            match key.code {
                KeyCode::Tab => {
                    self.complete(false);
                    return Action::None;
                }
                KeyCode::BackTab => {
                    self.complete(true);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.complete(false);
                    return Action::None;
                }
                KeyCode::Up => {
                    self.complete(true);
                    return Action::None;
                }
                KeyCode::Enter => {
                    self.accept_completion();
                    return Action::None;
                }
                KeyCode::Esc => {
                    self.completion = None;
                    return Action::None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Char('c') if ctrl => return Action::Close,
            // Abort the line without leaving.
            KeyCode::Char('g') if ctrl => {
                self.input.clear();
                self.cursor = 0;
                self.hist_idx = None;
                self.completion = None;
            }
            KeyCode::Tab => self.complete(false),
            KeyCode::BackTab => self.complete(true),
            // Alt-e (or F5, for terminals that eat Meta) asks for the plan
            // instead of the result. `^e` stays readline's end-of-line.
            KeyCode::Char('e') if alt => return self.explain(),
            KeyCode::F(5) => return self.explain(),
            // Alt-Enter is zmax's newline; Ctrl-j is the one every terminal sends.
            KeyCode::Enter if alt => self.insert_char('\n'),
            KeyCode::Char('j') if ctrl => self.insert_char('\n'),
            KeyCode::Enter => return self.submit(),
            KeyCode::Up => self.history_move(true),
            KeyCode::Down => self.history_move(false),
            KeyCode::Char('p') if ctrl => self.history_move(true),
            KeyCode::Char('n') if ctrl => self.history_move(false),
            KeyCode::Char('l') if ctrl => {
                self.transcript.clear();
                self.scroll = 0;
                self.follow = true;
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            // The readline chords need the ctrl guard, or a plain `b` would move
            // the cursor instead of being typed.
            KeyCode::Char('b') if ctrl => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            KeyCode::Char('f') if ctrl => self.cursor = (self.cursor + 1).min(self.input.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Char('e') if ctrl => self.cursor = self.input.len(),
            KeyCode::Char('k') if ctrl => {
                self.input.truncate(self.cursor);
                self.hist_idx = None;
            }
            KeyCode::Char('u') if ctrl => {
                self.input.drain(0..self.cursor);
                self.cursor = 0;
                self.hist_idx = None;
            }
            KeyCode::Char('w') if ctrl => self.kill_word(),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.input.remove(self.cursor);
                    self.hist_idx = None;
                    self.completion = None;
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                    self.hist_idx = None;
                    self.completion = None;
                }
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(page);
                self.follow = false;
            }
            KeyCode::PageDown => {
                self.scroll += page;
                self.follow = true;
            }
            KeyCode::Char(c) if !ctrl => self.insert_char(c),
            _ => {}
        }
        Action::None
    }

    /// Transcript as display lines, so scrolling and rendering agree.
    fn transcript_lines(&self, t: &Theme, width: usize) -> Vec<Line<'static>> {
        let mut out: Vec<Line> = Vec::new();
        for e in &self.transcript {
            match e {
                Entry::Sql(sql) => {
                    for (i, l) in sql.lines().enumerate() {
                        out.push(Line::from(vec![
                            Span::styled(
                                if i == 0 { "sql> " } else { "   . " }.to_string(),
                                Style::default().fg(t.accent),
                            ),
                            Span::styled(l.to_string(), Style::default().fg(t.primary)),
                        ]));
                    }
                }
                Entry::Changed(n) => out.push(Line::from(Span::styled(
                    format!("     {} row{} changed", n, if *n == 1 { "" } else { "s" }),
                    Style::default().fg(t.label),
                ))),
                Entry::Timing(d) => out.push(Line::from(Span::styled(
                    format!("     {:.3?}", d),
                    Style::default().fg(t.dim),
                ))),
                Entry::Plan(steps) => {
                    for s in steps {
                        out.push(Line::from(Span::styled(
                            format!("     {s}"),
                            Style::default().fg(t.label),
                        )));
                    }
                }
                Entry::Error(msg) => {
                    for l in msg.lines() {
                        out.push(Line::from(Span::styled(
                            format!("     {l}"),
                            Style::default().fg(t.alt).add_modifier(Modifier::BOLD),
                        )));
                    }
                }
                Entry::Rows {
                    columns,
                    rows,
                    truncated,
                } => {
                    // A compact fixed-width grid: every column the same share of
                    // the width, so headers and cells line up without a Table.
                    let per = (width.saturating_sub(6) / columns.len().max(1)).clamp(6, 28);
                    let head: String = columns
                        .iter()
                        .map(|c| format!("{:<per$}", crate::app::truncate(c, per), per = per))
                        .collect::<Vec<_>>()
                        .join(" ");
                    out.push(Line::from(Span::styled(
                        format!("     {head}"),
                        Style::default().fg(t.label).add_modifier(Modifier::BOLD),
                    )));
                    for r in rows {
                        let line: String = r
                            .iter()
                            .map(|c| format!("{:<per$}", crate::app::truncate(c, per), per = per))
                            .collect::<Vec<_>>()
                            .join(" ");
                        out.push(Line::from(Span::styled(
                            format!("     {line}"),
                            Style::default().fg(t.primary),
                        )));
                    }
                    out.push(Line::from(Span::styled(
                        format!(
                            "     {} row{}{}",
                            rows.len(),
                            if rows.len() == 1 { "" } else { "s" },
                            if *truncated {
                                format!(" (first {RESULT_ROWS})")
                            } else {
                                String::new()
                            }
                        ),
                        Style::default().fg(t.dim),
                    )));
                }
            }
        }
        out
    }

    /// Draw the editor: transcript above, input below, candidates over the input.
    pub fn render(&mut self, f: &mut Frame, area: Rect, t: &Theme) {
        // The input grows with the statement; the transcript takes the rest.
        let input_lines = (self.input.iter().filter(|&&c| c == '\n').count() + 1).min(8) as u16;
        let rows =
            Layout::vertical([Constraint::Min(3), Constraint::Length(input_lines + 2)]).split(area);

        let lines = self.transcript_lines(t, rows[0].width as usize);
        let view = rows[0].height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(view);
        if self.follow {
            self.scroll = max_scroll;
        }
        self.scroll = self.scroll.min(max_scroll);
        let shown: Vec<Line> = lines
            .into_iter()
            .skip(self.scroll)
            .take(view.max(1))
            .collect();
        f.render_widget(
            Paragraph::new(shown).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.accent))
                    .title(format!(
                        " SQL — {} statement{} · Tab completes · ^j newline · Enter runs · Esc back ",
                        self.history.len(),
                        if self.history.len() == 1 { "" } else { "s" }
                    )),
            ),
            rows[0],
        );

        // Input, with the caret drawn as a reversed cell so it is visible without
        // asking the terminal to move its own cursor.
        let text: String = self.input.iter().collect();
        let mut body: Vec<Line> = Vec::new();
        let mut idx = 0usize;
        for (n, l) in text.split('\n').enumerate() {
            let len = l.chars().count();
            let mut spans = vec![Span::styled(
                if n == 0 { "sql> " } else { "   . " }.to_string(),
                Style::default().fg(t.accent),
            )];
            if self.cursor >= idx && self.cursor <= idx + len {
                let at = self.cursor - idx;
                let (before, rest) =
                    l.split_at(l.char_indices().nth(at).map_or(l.len(), |(i, _)| i));
                let mut chars = rest.chars();
                let under = chars.next();
                spans.push(Span::raw(before.to_string()));
                spans.push(Span::styled(
                    under.map(|c| c.to_string()).unwrap_or_else(|| " ".into()),
                    Style::default().add_modifier(Modifier::REVERSED),
                ));
                spans.push(Span::raw(chars.as_str().to_string()));
            } else {
                spans.push(Span::raw(l.to_string()));
            }
            body.push(Line::from(spans));
            idx += len + 1;
        }
        f.render_widget(
            Paragraph::new(body).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if self.running { t.label } else { t.dim }))
                    .title(if self.running {
                        " running… ".to_string()
                    } else {
                        format!(" input · {} chars ", self.input.len())
                    }),
            ),
            rows[1],
        );

        // Candidates float above the input line.
        if let Some(c) = &self.completion {
            let h = (c.items.len() as u16 + 2).min(area.height.saturating_sub(3));
            let w = c
                .items
                .iter()
                .map(|i| i.chars().count())
                .max()
                .unwrap_or(10)
                .clamp(12, 40) as u16
                + 4;
            let x = area.x + 5;
            // One row clear of the input pane's border, so the menu never paints
            // over the frame it sits above.
            let y = rows[1].y.saturating_sub(h + 1);
            let popup = Rect::new(x, y, w.min(area.width.saturating_sub(5)), h.max(3));
            f.render_widget(Clear, popup);
            let items: Vec<ListItem> = c.items.iter().map(|i| ListItem::new(i.clone())).collect();
            let mut st = ListState::default();
            st.select(Some(c.selected));
            f.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(t.accent))
                            .title(format!(" {} ", c.prefix)),
                    )
                    .highlight_style(
                        Style::default()
                            .fg(t.accent)
                            .add_modifier(Modifier::REVERSED),
                    ),
                popup,
                &mut st,
            );
        }
    }

    /// A screenful of transcript, for paging.
    pub fn page_rows(area: Rect) -> usize {
        (area.height as usize).saturating_sub(6).max(1)
    }
}

/// `$XDG_CACHE_HOME/zdbview/sql_history`, beside the other appdata.
fn history_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))?;
    Some(base.join("zdbview").join("sql_history"))
}

/// Statements from previous sessions, oldest first. Newlines are stored escaped so
/// one statement stays one line.
fn load_history() -> Vec<String> {
    let path = match history_file() {
        Some(p) => p,
        None => return Vec::new(),
    };
    std::fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.replace("\\n", "\n"))
                .collect()
        })
        .unwrap_or_default()
}

fn save_history(history: &[String]) {
    let path = match history_file() {
        Some(p) => p,
        None => return,
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body: String = history
        .iter()
        .map(|s| format!("{}\n", s.replace('\n', "\\n")))
        .collect();
    // Temp plus rename, like every other appdata file here.
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeName;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn schema() -> Vec<(String, Vec<String>)> {
        vec![
            (
                "users".into(),
                vec!["user_id".into(), "email".into(), "created_at".into()],
            ),
            ("orders".into(), vec!["order_id".into(), "total".into()]),
        ]
    }

    fn ed() -> SqlEdit {
        let mut e = SqlEdit::new(schema());
        // Keep the developer's real history out of the assertions.
        e.history.clear();
        e
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::from(KeyCode::Char(c))
    }

    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::from(c)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(e: &mut SqlEdit, s: &str) {
        for c in s.chars() {
            e.on_key(key(c), 10);
        }
    }

    fn rows_of(e: &mut SqlEdit, w: u16, h: u16) -> Vec<String> {
        let theme = Theme::from_name(ThemeName::NeonSprawl);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| e.render(f, f.area(), &theme)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Enter submits the statement for the host to run; the editor keeps the text
    /// in its transcript and clears the input.
    #[test]
    fn enter_submits_and_the_result_comes_back() {
        let mut e = ed();
        type_str(&mut e, "select 1");
        assert_eq!(e.text(), "select 1");
        let action = e.on_key(code(KeyCode::Enter), 10);
        assert_eq!(action, Action::Execute("select 1".into()));
        assert!(e.running, "the host has it now");
        assert!(e.text().is_empty(), "input cleared for the next statement");
        assert_eq!(e.transcript()[0], Entry::Sql("select 1".into()));

        e.push(Entry::Rows {
            columns: vec!["1".into()],
            rows: vec![vec!["1".into()]],
            truncated: false,
        });
        assert!(!e.running);
        assert_eq!(e.transcript().len(), 2);

        // An empty submit does nothing at all.
        assert_eq!(e.on_key(code(KeyCode::Enter), 10), Action::None);
        assert_eq!(e.transcript().len(), 2);
    }

    /// A statement may span lines: Ctrl-j (and Alt-Enter) insert one, Enter runs.
    #[test]
    fn multi_line_statements() {
        let mut e = ed();
        type_str(&mut e, "select *");
        e.on_key(ctrl('j'), 10);
        type_str(&mut e, "from users");
        assert_eq!(e.text(), "select *\nfrom users");
        e.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), 10);
        type_str(&mut e, "where 1");
        assert_eq!(e.text(), "select *\nfrom users\nwhere 1");
        assert_eq!(
            e.on_key(code(KeyCode::Enter), 10),
            Action::Execute("select *\nfrom users\nwhere 1".into())
        );
    }

    /// Completion follows the clause: what SQL allows at the cursor is what is
    /// offered. Ranking alone was not enough — after `from users where na` the old
    /// version offered the `named_dirs` table before the `name` column.
    #[test]
    fn completion_follows_the_clause() {
        // A statement can only start with a statement keyword.
        let mut e = ed();
        assert_eq!(e.candidates(""), {
            let mut v: Vec<String> = STATEMENTS.iter().map(|s| s.to_string()).collect();
            v.truncate(MAX_CANDIDATES);
            v
        });
        type_str(&mut e, "sel");
        e.on_key(code(KeyCode::Tab), 10);
        assert_eq!(e.text(), "SELECT", "the only statement starting with sel");

        // After FROM only a table can follow.
        let mut e = ed();
        type_str(&mut e, "select * from ");
        assert_eq!(e.candidates(""), vec!["users".to_string(), "orders".into()]);
        type_str(&mut e, "us");
        e.on_key(code(KeyCode::Tab), 10);
        assert_eq!(e.text(), "select * from users");

        // In WHERE, the scoped table's columns lead — this is the reported bug.
        type_str(&mut e, " where us");
        let cands = e.candidates("us");
        assert_eq!(
            cands.first().map(String::as_str),
            Some("user_id"),
            "the scoped column must come before the table name: {cands:?}"
        );
        e.on_key(code(KeyCode::Tab), 10);
        // Two candidates (user_id, users) so a menu opens with the column first.
        let c = e.completion.as_ref().expect("menu");
        assert_eq!(c.items[0], "user_id");
        e.on_key(code(KeyCode::Enter), 10);
        assert_eq!(e.text(), "select * from users where user_id");

        // A column with no table named yet can come from any table.
        let mut e = ed();
        type_str(&mut e, "select tot");
        e.on_key(code(KeyCode::Tab), 10);
        assert_eq!(
            e.text(),
            "select total",
            "orders.total, table not yet named"
        );
    }

    /// `t.` and `alias.` complete only that table's columns.
    #[test]
    fn qualified_completion_uses_the_alias() {
        let mut e = ed();
        type_str(&mut e, "select * from orders o where o.");
        let cands = e.candidates("");
        assert_eq!(
            cands,
            vec!["order_id".to_string(), "total".into()],
            "{cands:?}"
        );
        type_str(&mut e, "tot");
        e.on_key(code(KeyCode::Tab), 10);
        assert_eq!(e.text(), "select * from orders o where o.total");

        // The table's own name works as a qualifier too.
        let mut e = ed();
        type_str(&mut e, "select users.");
        assert!(e.candidates("").contains(&"email".to_string()));

        // An unknown qualifier offers nothing rather than guessing.
        let mut e = ed();
        type_str(&mut e, "select nosuch.");
        assert!(e.candidates("").is_empty());
        e.on_key(code(KeyCode::Tab), 10);
        assert_eq!(e.text(), "select nosuch.", "nothing was inserted");
    }

    /// A join brings both tables into scope, in the order they were named.
    #[test]
    fn a_join_scopes_both_tables() {
        let mut e = ed();
        type_str(
            &mut e,
            "select * from users u join orders o on u.user_id = o.",
        );
        assert_eq!(
            e.candidates(""),
            vec!["order_id".to_string(), "total".into()],
            "the qualifier decides"
        );
        // Unqualified, both tables' columns are in scope, users first.
        let mut e = ed();
        type_str(&mut e, "select * from users join orders where ");
        let cands = e.candidates("");
        let users_first = cands.iter().position(|c| c == "user_id").unwrap();
        let orders_next = cands.iter().position(|c| c == "order_id").unwrap();
        assert!(users_first < orders_next, "FROM order preserved: {cands:?}");
    }

    /// `PRAGMA` completes pragma names, not tables or keywords.
    #[test]
    fn pragma_completes_pragma_names() {
        let mut e = ed();
        type_str(&mut e, "pragma jour");
        e.on_key(code(KeyCode::Tab), 10);
        assert_eq!(e.text(), "pragma journal_mode");
        let mut e = ed();
        type_str(&mut e, "pragma ");
        let cands = e.candidates("");
        assert!(cands.contains(&"table_info".to_string()), "{cands:?}");
        assert!(
            !cands.iter().any(|c| c == "users"),
            "no tables here: {cands:?}"
        );
    }

    /// Completion only replaces the word under the cursor, mid-statement too.
    #[test]
    fn completion_replaces_only_the_current_word() {
        let mut e = ed();
        type_str(&mut e, "select ema from users");
        // Move back to just after `ema`.
        for _ in 0.."  from users".len() - 1 {
            e.on_key(code(KeyCode::Left), 10);
        }
        e.on_key(code(KeyCode::Tab), 10);
        assert_eq!(e.text(), "select email from users");
    }

    /// History browses with the in-progress line stashed, as in zmax's REPL.
    #[test]
    fn history_browses_and_restores_the_stash() {
        let mut e = ed();
        for sql in ["select 1", "select 2"] {
            type_str(&mut e, sql);
            e.on_key(code(KeyCode::Enter), 10);
            e.push(Entry::Changed(0));
        }
        type_str(&mut e, "half typed");
        e.on_key(code(KeyCode::Up), 10);
        assert_eq!(e.text(), "select 2", "newest first");
        e.on_key(ctrl('p'), 10);
        assert_eq!(e.text(), "select 1");
        e.on_key(ctrl('p'), 10);
        assert_eq!(e.text(), "select 1", "stops at the oldest");
        e.on_key(code(KeyCode::Down), 10);
        assert_eq!(e.text(), "select 2");
        e.on_key(ctrl('n'), 10);
        assert_eq!(e.text(), "half typed", "past the newest restores the stash");

        // Re-running the newest statement does not grow the history; a different
        // one does.
        let before = e.history.len();
        e.on_key(ctrl('g'), 10);
        type_str(&mut e, "select 2");
        e.on_key(code(KeyCode::Enter), 10);
        assert_eq!(
            e.history.len(),
            before,
            "a repeat of the newest entry is not recorded again"
        );
        e.push(Entry::Changed(0));
        type_str(&mut e, "select 3");
        e.on_key(code(KeyCode::Enter), 10);
        assert_eq!(e.history.len(), before + 1, "a new statement is recorded");
    }

    /// The readline chords and Ctrl-g/Ctrl-l behave as the ported panel does.
    #[test]
    fn editing_chords() {
        let mut e = ed();
        type_str(&mut e, "select * from users");
        e.on_key(ctrl('w'), 10);
        assert_eq!(e.text(), "select * from ");
        e.on_key(ctrl('a'), 10);
        assert_eq!(e.cursor, 0);
        e.on_key(ctrl('e'), 10);
        assert_eq!(e.cursor, e.text().chars().count());
        e.on_key(ctrl('u'), 10);
        assert!(e.text().is_empty(), "^u killed to the start");

        type_str(&mut e, "abc");
        e.on_key(code(KeyCode::Left), 10);
        e.on_key(ctrl('k'), 10);
        assert_eq!(e.text(), "ab", "^k killed to the end");
        e.on_key(code(KeyCode::Backspace), 10);
        assert_eq!(e.text(), "a");

        e.on_key(ctrl('g'), 10);
        assert!(e.text().is_empty(), "^g cleared the line");
        e.push(Entry::Changed(1));
        assert!(!e.transcript().is_empty());
        e.on_key(ctrl('l'), 10);
        assert!(e.transcript().is_empty(), "^l cleared the transcript");
    }

    /// The transcript shows a result set as a grid, a change count, and an error.
    #[test]
    fn render_shows_results_and_errors() {
        let mut e = ed();
        e.push(Entry::Sql("select * from users".into()));
        e.push(Entry::Rows {
            columns: vec!["user_id".into(), "email".into()],
            rows: vec![
                vec!["1".into(), "a@example.com".into()],
                vec!["2".into(), "b@example.com".into()],
            ],
            truncated: true,
        });
        let r = rows_of(&mut e, 100, 18);
        assert!(r.iter().any(|l| l.contains("sql> select * from users")));
        assert!(r.iter().any(|l| l.contains("user_id")), "header missing");
        assert!(r.iter().any(|l| l.contains("a@example.com")), "row missing");
        assert!(
            r.iter().any(|l| l.contains("2 rows (first 500)")),
            "count/truncation missing: {r:#?}"
        );
        assert!(
            r.iter().any(|l| l.contains("Tab completes")),
            "no key hints"
        );

        e.push(Entry::Changed(3));
        assert!(rows_of(&mut e, 100, 18)
            .iter()
            .any(|l| l.contains("3 rows changed")));
        e.push(Entry::Error("no such table: nope".into()));
        assert!(rows_of(&mut e, 100, 18)
            .iter()
            .any(|l| l.contains("no such table: nope")));

        // The input pane shows what is being typed, and the caret sits in it.
        type_str(&mut e, "select");
        let r = rows_of(&mut e, 100, 18);
        assert!(r.iter().any(|l| l.contains("sql> select")), "{r:#?}");
        assert!(r.iter().any(|l| l.contains("6 chars")));
    }

    /// The candidate menu is drawn above the input, titled with the prefix.
    #[test]
    fn render_shows_the_completion_menu() {
        let mut e = ed();
        // In WHERE, `us` matches the user_id column and the users table.
        type_str(&mut e, "select * from users where us");
        e.on_key(code(KeyCode::Tab), 10);
        assert!(e.completion.is_some(), "a menu should be open");
        let r = rows_of(&mut e, 100, 18);
        assert!(
            r.iter().any(|l| l.contains("user_id")),
            "the menu is not on screen: {r:#?}"
        );
        assert!(r.iter().any(|l| l.contains(" us ")), "prefix not titled");
    }

    /// Rendering must survive a terminal too small for the panes.
    #[test]
    fn render_survives_a_tiny_terminal() {
        let mut e = ed();
        e.push(Entry::Changed(1));
        type_str(&mut e, "select 1");
        e.on_key(code(KeyCode::Tab), 10);
        for (w, h) in [(20u16, 6u16), (8, 4), (1, 1), (200, 60)] {
            rows_of(&mut e, w, h);
        }
        assert!(SqlEdit::page_rows(Rect::new(0, 0, 80, 24)) >= 1);
        assert_eq!(SqlEdit::page_rows(Rect::new(0, 0, 80, 2)), 1, "never zero");
    }

    /// The transcript is bounded, so a long session cannot grow without limit.
    #[test]
    fn transcript_is_bounded() {
        let mut e = ed();
        for i in 0..MAX_TRANSCRIPT + 50 {
            e.push(Entry::Changed(i));
        }
        assert_eq!(e.transcript().len(), MAX_TRANSCRIPT);
        // The oldest entries are the ones dropped.
        assert_eq!(e.transcript()[0], Entry::Changed(50));
    }
}
