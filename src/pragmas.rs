//! The pragmas DB Browser for SQLite puts in its "Edit Pragmas" tab, editable.
//!
//! The database report (`D`) already prints the pragmas that describe a file.
//! These are the other kind: the eighteen settings DB4S lets you change, which
//! decide how the database is written rather than what is in it — durability,
//! locking, journalling, page size, foreign-key enforcement.
//!
//! The list is exactly DB4S's, taken from its own UI. Each row knows what shape
//! its value has, which is what makes a terminal form possible at all: a flag
//! toggles, a named set cycles, a number opens an editor.
//!
//! Everything here is state and keys; the store reads and writes the values, and
//! reports what SQLite actually kept — which is not always what was asked for. A
//! `journal_mode` change is refused inside a transaction, `page_size` needs a
//! `VACUUM` to take effect, and `max_page_count` clamps to the pages already
//! used. Reading the value back after setting it is the only honest way to show
//! what happened.

use crate::input::{Edit, Line};
use crate::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// What shape one pragma's value has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `0` or `1`, shown as off / on.
    Flag,
    /// One of a named set. The pairs are `(what is written, what is shown)`,
    /// because half of these read back as integers and read far better as words.
    Choice(&'static [(&'static str, &'static str)]),
    /// A number, typed in.
    Int,
}

/// One editable pragma.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub name: &'static str,
    pub kind: Kind,
    /// What it does, in one line — DB4S shows the same as a tooltip.
    pub note: &'static str,
    /// The value cannot be read back: `PRAGMA case_sensitive_like` has no query
    /// form at all, so what is shown is what this session last set.
    pub write_only: bool,
    /// The change only takes effect after the file is rewritten.
    pub needs_vacuum: bool,
}

const FLAG: Kind = Kind::Flag;

const AUTO_VACUUM: &[(&str, &str)] = &[("0", "none"), ("1", "full"), ("2", "incremental")];
const JOURNAL_MODE: &[(&str, &str)] = &[
    ("delete", "delete"),
    ("truncate", "truncate"),
    ("persist", "persist"),
    ("memory", "memory"),
    ("wal", "wal"),
    ("off", "off"),
];
const LOCKING_MODE: &[(&str, &str)] = &[("normal", "normal"), ("exclusive", "exclusive")];
const PAGE_SIZE: &[(&str, &str)] = &[
    ("512", "512"),
    ("1024", "1024"),
    ("2048", "2048"),
    ("4096", "4096"),
    ("8192", "8192"),
    ("16384", "16384"),
    ("32768", "32768"),
    ("65536", "65536"),
];
const SECURE_DELETE: &[(&str, &str)] = &[("0", "off"), ("1", "on"), ("2", "fast")];
const SYNCHRONOUS: &[(&str, &str)] =
    &[("0", "off"), ("1", "normal"), ("2", "full"), ("3", "extra")];
const TEMP_STORE: &[(&str, &str)] = &[("0", "default"), ("1", "file"), ("2", "memory")];

/// The eighteen pragmas DB Browser's Edit Pragmas tab exposes, in its order.
pub const EDITABLE: &[Spec] = &[
    Spec {
        name: "auto_vacuum",
        kind: Kind::Choice(AUTO_VACUUM),
        note: "reclaim freed pages on commit, or only on incremental_vacuum",
        write_only: false,
        needs_vacuum: true,
    },
    Spec {
        name: "automatic_index",
        kind: FLAG,
        note: "let the planner build a transient index for a scan it would repeat",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "case_sensitive_like",
        kind: FLAG,
        note: "make LIKE case-sensitive for ASCII (no query form: shows what this session set)",
        write_only: true,
        needs_vacuum: false,
    },
    Spec {
        name: "checkpoint_fullfsync",
        kind: FLAG,
        note: "a full fsync on every WAL checkpoint",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "foreign_keys",
        kind: FLAG,
        note: "enforce foreign keys — off is SQLite's default, for compatibility",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "fullfsync",
        kind: FLAG,
        note: "a full fsync on every commit (macOS: F_FULLFSYNC)",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "ignore_check_constraints",
        kind: FLAG,
        note: "skip CHECK constraints on write",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "journal_mode",
        kind: Kind::Choice(JOURNAL_MODE),
        note: "how the rollback journal is kept; wal lets a reader and a writer overlap",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "journal_size_limit",
        kind: Kind::Int,
        note: "bytes the journal or WAL is truncated to after a commit (-1 = no limit)",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "locking_mode",
        kind: Kind::Choice(LOCKING_MODE),
        note: "exclusive keeps the file lock between transactions",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "max_page_count",
        kind: Kind::Int,
        note: "hard ceiling on the file's pages; cannot go below what is in use",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "page_size",
        kind: Kind::Choice(PAGE_SIZE),
        note: "bytes per page",
        write_only: false,
        needs_vacuum: true,
    },
    Spec {
        name: "recursive_triggers",
        kind: FLAG,
        note: "let a trigger's own writes fire triggers",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "secure_delete",
        kind: Kind::Choice(SECURE_DELETE),
        note: "overwrite deleted content with zeroes; fast only does the free pages",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "synchronous",
        kind: Kind::Choice(SYNCHRONOUS),
        note: "how hard a commit is flushed — off risks the file on a power loss",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "temp_store",
        kind: Kind::Choice(TEMP_STORE),
        note: "where a sort or a materialised subquery goes",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "user_version",
        kind: Kind::Int,
        note: "a number the application owns; SQLite never reads it",
        write_only: false,
        needs_vacuum: false,
    },
    Spec {
        name: "wal_autocheckpoint",
        kind: Kind::Int,
        note: "WAL pages before an automatic checkpoint (0 = never)",
        write_only: false,
        needs_vacuum: false,
    },
];

/// What a key asked the caller to do.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Action {
    None,
    /// Esc — leave the screen.
    Cancel,
    /// Write `PRAGMA name = value` and report what came back.
    Set(&'static str, String),
    Note(String),
}

/// The Edit Pragmas form.
pub struct PragmaEditor {
    /// Current value of each spec in [`EDITABLE`], in the same order.
    values: Vec<String>,
    sel: usize,
    edit: Option<Line>,
    /// Rows the last frame had space for, which is what paging moves by.
    page: usize,
}

impl PragmaEditor {
    /// Build the form from the values just read out of the database. A pragma
    /// that read back nothing shows as empty, which is what a write-only one
    /// does before it is set.
    pub fn new(values: Vec<String>) -> Self {
        PragmaEditor {
            values,
            sel: 0,
            edit: None,
            page: 10,
        }
    }

    /// Replace one row's value with what the database reported after a write.
    pub fn update(&mut self, name: &str, value: String) {
        if let Some(i) = EDITABLE.iter().position(|s| s.name == name) {
            self.values[i] = value;
        }
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        EDITABLE
            .iter()
            .position(|s| s.name == name)
            .map(|i| self.values[i].as_str())
    }

    fn spec(&self) -> &'static Spec {
        &EDITABLE[self.sel.min(EDITABLE.len() - 1)]
    }

    /// The value one step along for a flag or a choice — Space cycles, so a
    /// setting is reachable without typing.
    fn next_value(&self, back: bool) -> Option<String> {
        let cur = self.values[self.sel].trim();
        match self.spec().kind {
            Kind::Flag => Some(if cur == "1" { "0".into() } else { "1".into() }),
            Kind::Choice(set) => {
                let at = set
                    .iter()
                    .position(|(v, _)| v.eq_ignore_ascii_case(cur))
                    .unwrap_or(0);
                let n = set.len();
                let next = if back { (at + n - 1) % n } else { (at + 1) % n };
                Some(set[next].0.to_string())
            }
            Kind::Int => None,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if let Some(line) = self.edit.as_mut() {
            return match line.on_key(key) {
                Edit::Commit => {
                    let text = line.buf.trim().to_string();
                    self.edit = None;
                    if text.is_empty() {
                        Action::None
                    } else {
                        Action::Set(self.spec().name, text)
                    }
                }
                Edit::Cancel => {
                    self.edit = None;
                    Action::None
                }
                _ => Action::None,
            };
        }
        let last = EDITABLE.len() - 1;
        match key.code {
            KeyCode::Esc | KeyCode::Char('p') | KeyCode::Char('P') => Action::Cancel,
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(last);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = self.sel.saturating_sub(1);
                Action::None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.sel = 0;
                Action::None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.sel = last;
                Action::None
            }
            KeyCode::PageDown => {
                self.sel = (self.sel + self.page).min(last);
                Action::None
            }
            KeyCode::PageUp => {
                self.sel = self.sel.saturating_sub(self.page);
                Action::None
            }
            KeyCode::Char(' ') | KeyCode::Right => match self.next_value(false) {
                Some(v) => Action::Set(self.spec().name, v),
                None => Action::Note(format!(
                    "{} is a number — Enter types one",
                    self.spec().name
                )),
            },
            KeyCode::Left => match self.next_value(true) {
                Some(v) => Action::Set(self.spec().name, v),
                None => Action::None,
            },
            KeyCode::Enter | KeyCode::Char('e') => match self.spec().kind {
                Kind::Int => {
                    self.edit = Some(Line::at_end(self.values[self.sel].trim()));
                    Action::None
                }
                _ => match self.next_value(false) {
                    Some(v) => Action::Set(self.spec().name, v),
                    None => Action::None,
                },
            },
            _ => Action::None,
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, t: &Theme) {
        let height = area.height.saturating_sub(4) as usize;
        self.page = height.max(1);
        let mut lines: Vec<TextLine> = Vec::new();
        lines.push(TextLine::from(Span::styled(
            format!("  {:<26} {:<14} {}", "pragma", "value", "what it does"),
            Style::default().fg(t.alt).add_modifier(Modifier::BOLD),
        )));
        // Keep the cursor on screen when the list is taller than the frame.
        let first = self.sel.saturating_sub(height.saturating_sub(2));
        for (i, spec) in EDITABLE.iter().enumerate().skip(first).take(height) {
            let selected = i == self.sel;
            let shown = match &self.edit {
                Some(l) if selected => format!("{}_", l.buf),
                _ => self.display(i),
            };
            lines.push(TextLine::from(vec![
                Span::styled(
                    format!("{} ", if selected { "▸" } else { " " }),
                    Style::default().fg(t.accent),
                ),
                Span::styled(
                    format!("{:<26} ", spec.name),
                    if selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(
                    format!("{:<14} ", shown),
                    if selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default().fg(t.primary)
                    },
                ),
                Span::styled(spec.note.to_string(), Style::default().fg(t.dim)),
            ]));
        }
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default().borders(Borders::ALL).title(
                    " pragmas — j/k select · Space cycles · Enter edits a number · Esc back ",
                ),
            ),
            area,
        );
    }

    /// One row's value as it is shown: the label for a choice, off/on for a flag,
    /// and a marker for a value the database will not report.
    fn display(&self, i: usize) -> String {
        let raw = self.values[i].trim();
        let spec = &EDITABLE[i];
        if raw.is_empty() {
            return if spec.write_only {
                "—".into()
            } else {
                "?".into()
            };
        }
        match spec.kind {
            Kind::Flag => if raw == "1" { "on" } else { "off" }.to_string(),
            Kind::Choice(set) => set
                .iter()
                .find(|(v, _)| v.eq_ignore_ascii_case(raw))
                .map(|(_, label)| label.to_string())
                .unwrap_or_else(|| raw.to_string()),
            Kind::Int => raw.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
    }

    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::empty())
    }

    /// Every value starts as whatever the database reported, in spec order.
    fn editor() -> PragmaEditor {
        PragmaEditor::new(
            EDITABLE
                .iter()
                .map(|s| match s.kind {
                    Kind::Flag => "0".to_string(),
                    Kind::Choice(set) => set[0].0.to_string(),
                    Kind::Int => "0".to_string(),
                })
                .collect(),
        )
    }

    #[test]
    fn the_list_is_the_one_db4s_edits() {
        // Eighteen, and each named exactly as the pragma is.
        assert_eq!(EDITABLE.len(), 18);
        for s in EDITABLE {
            assert!(
                s.name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{}",
                s.name
            );
            assert!(!s.note.is_empty(), "{} has no note", s.name);
        }
        assert!(EDITABLE.iter().any(|s| s.name == "journal_mode"));
        assert!(EDITABLE.iter().any(|s| s.name == "foreign_keys"));
        assert!(EDITABLE.iter().any(|s| s.name == "case_sensitive_like"));
    }

    #[test]
    fn space_flips_a_flag_and_cycles_a_choice() {
        let mut e = editor();
        // auto_vacuum is the first row and a choice.
        assert_eq!(
            e.on_key(key(' ')),
            Action::Set("auto_vacuum", "1".to_string())
        );
        // Move to automatic_index, a flag.
        e.on_key(code(KeyCode::Down));
        assert_eq!(
            e.on_key(key(' ')),
            Action::Set("automatic_index", "1".to_string())
        );
        e.update("automatic_index", "1".into());
        assert_eq!(
            e.on_key(key(' ')),
            Action::Set("automatic_index", "0".to_string())
        );
    }

    #[test]
    fn a_choice_wraps_in_both_directions() {
        let mut e = editor();
        e.update("auto_vacuum", "2".into()); // the last of three
        assert_eq!(
            e.on_key(code(KeyCode::Right)),
            Action::Set("auto_vacuum", "0".to_string())
        );
        e.update("auto_vacuum", "0".into());
        assert_eq!(
            e.on_key(code(KeyCode::Left)),
            Action::Set("auto_vacuum", "2".to_string())
        );
    }

    #[test]
    fn a_number_is_typed_rather_than_cycled() {
        let mut e = editor();
        // user_version is an Int.
        let at = EDITABLE
            .iter()
            .position(|s| s.name == "user_version")
            .unwrap();
        for _ in 0..at {
            e.on_key(code(KeyCode::Down));
        }
        assert!(matches!(e.on_key(key(' ')), Action::Note(_)));
        e.on_key(code(KeyCode::Enter));
        e.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "42".chars() {
            e.on_key(key(c));
        }
        assert_eq!(
            e.on_key(code(KeyCode::Enter)),
            Action::Set("user_version", "42".to_string())
        );
    }

    #[test]
    fn esc_in_a_number_leaves_the_value_alone() {
        let mut e = editor();
        let at = EDITABLE
            .iter()
            .position(|s| s.name == "user_version")
            .unwrap();
        for _ in 0..at {
            e.on_key(code(KeyCode::Down));
        }
        e.on_key(code(KeyCode::Enter));
        e.on_key(key('9'));
        assert_eq!(
            e.on_key(code(KeyCode::Esc)),
            Action::None,
            "the field closes"
        );
        assert_eq!(e.value("user_version"), Some("0"));
        // And Esc again leaves the screen.
        assert_eq!(e.on_key(code(KeyCode::Esc)), Action::Cancel);
    }

    #[test]
    fn values_read_as_words_not_as_numbers() {
        let mut e = editor();
        e.update("synchronous", "2".into());
        let i = EDITABLE
            .iter()
            .position(|s| s.name == "synchronous")
            .unwrap();
        assert_eq!(e.display(i), "full");
        e.update("temp_store", "2".into());
        let i = EDITABLE
            .iter()
            .position(|s| s.name == "temp_store")
            .unwrap();
        assert_eq!(e.display(i), "memory");
        // A value SQLite reports that is not in the set is shown as it came.
        e.update("journal_mode", "wal2".into());
        let i = EDITABLE
            .iter()
            .position(|s| s.name == "journal_mode")
            .unwrap();
        assert_eq!(e.display(i), "wal2");
        // And one that cannot be read at all says so rather than lying.
        e.update("case_sensitive_like", String::new());
        let i = EDITABLE
            .iter()
            .position(|s| s.name == "case_sensitive_like")
            .unwrap();
        assert_eq!(e.display(i), "—");
    }

    /// Every listed pragma has to be a pragma SQLite actually has, and every
    /// choice a value it actually accepts — a typo here is a row in the editor
    /// that silently does nothing. SQLite is the arbiter, so each one is set.
    #[test]
    fn every_pragma_and_every_choice_is_one_sqlite_accepts() {
        let path =
            std::env::temp_dir().join(format!("zdbview_pragmas_{}_specs.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // A file rather than :memory:, since journal_mode and page_size mean
        // nothing to a database with no file behind it.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE t (a TEXT)", []).unwrap();

        for spec in EDITABLE {
            if !spec.write_only {
                conn.query_row(&format!("PRAGMA {}", spec.name), [], |r| {
                    r.get::<_, rusqlite::types::Value>(0)
                })
                .unwrap_or_else(|e| panic!("PRAGMA {} is not readable: {e}", spec.name));
            }
            let values: Vec<String> = match spec.kind {
                Kind::Flag => vec!["0".into(), "1".into()],
                Kind::Choice(pairs) => pairs.iter().map(|(v, _)| (*v).to_string()).collect(),
                // A number pragma takes any; one that is plainly in range says
                // the statement parses.
                Kind::Int => vec!["1000".into()],
            };
            for value in values {
                let sql = format!("PRAGMA {} = {}", spec.name, value);
                conn.execute_batch(&sql)
                    .unwrap_or_else(|e| panic!("{sql} rejected: {e}"));
            }
        }
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    /// The editor writes one value and shows another: half of these read back as
    /// integers that mean nothing on screen. Every choice must map both ways.
    #[test]
    fn a_choice_shows_a_word_and_writes_what_sqlite_wants() {
        for spec in EDITABLE {
            let Kind::Choice(pairs) = spec.kind else {
                continue;
            };
            assert!(!pairs.is_empty(), "{} has no choices", spec.name);
            for (written, shown) in pairs {
                assert!(!written.is_empty() && !shown.is_empty(), "{}", spec.name);
            }
            // The written values are what identifies a choice, so they cannot
            // repeat — cycling would stall on the duplicate.
            let mut seen: Vec<&str> = pairs.iter().map(|(w, _)| *w).collect();
            seen.sort_unstable();
            let before = seen.len();
            seen.dedup();
            assert_eq!(before, seen.len(), "{} repeats a value", spec.name);
        }
    }

    /// A pragma whose value cannot be read back is the one case where the editor
    /// is the only record of what was set, so it has to be marked.
    #[test]
    fn the_unreadable_pragma_is_the_one_that_is_marked_write_only() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for spec in EDITABLE {
            let readable = conn
                .query_row(&format!("PRAGMA {}", spec.name), [], |r| {
                    r.get::<_, rusqlite::types::Value>(0)
                })
                .is_ok();
            assert_eq!(
                readable, !spec.write_only,
                "{} is marked write_only = {} but reads back = {readable}",
                spec.name, spec.write_only
            );
        }
    }
}
