//! Project files: DB Browser's `.sqbpr`, as a text file zdbview can read back.
//!
//! A project is everything about a session that is not in the database — which
//! file was open, what each table's grid was set to show, what was filtered and
//! sorted, and the statements left in the SQL editor. DB4S writes XML; this is a
//! line format instead, because a project has to survive being read by a person
//! and by the next version of this program, and neither wants a parser.
//!
//! The format is one directive per line, fields separated by tabs (written
//! `<TAB>` below), so a column called `order total` needs no quoting:
//!
//! ```text
//! zdbview-project<TAB>1
//! db<TAB>/path/to/file.db
//! table<TAB>orders
//! hidden<TAB>orders<TAB>note
//! frozen<TAB>orders<TAB>1
//! rowid<TAB>orders<TAB>on
//! format<TAB>orders<TAB>total<TAB>hex-number
//! rule<TAB>orders<TAB>total<TAB>> 100<TAB>red<TAB>bold
//! filter<TAB>orders<TAB>tag:keep
//! sort<TAB>orders<TAB>total<TAB>desc
//! sql<TAB>SELECT * FROM orders WHERE total > 100
//! ```
//!
//! Anything unrecognised is skipped rather than refused: a project written by a
//! later version still opens, minus what this one does not know about.

use crate::browse::{Browse, Format, Rule, RuleColor};
use std::path::{Path, PathBuf};

/// The line the file must start with, and the version of the format after it.
const HEADER: &str = "zdbview-project";
const VERSION: &str = "1";

/// What the grid is doing right now: the table in front, its filter, and its
/// sort as `(column, descending)`.
pub type Current<'a> = (&'a str, &'a str, Option<(&'a str, bool)>);

/// A session, as saved.
#[derive(Debug, Default, PartialEq)]
pub struct Project {
    /// The database the project is about.
    pub database: PathBuf,
    /// Per-table grid settings, in the same shape the app holds them.
    pub tables: Vec<TableSettings>,
    /// The statement in each SQL editor tab.
    pub statements: Vec<String>,
}

/// One table's saved settings.
#[derive(Debug, Default, PartialEq, Clone)]
pub struct TableSettings {
    pub name: String,
    pub hidden: Vec<String>,
    pub frozen: usize,
    pub show_rowid: bool,
    /// `(column, format)`, kept as a list so the file's order is the file's own.
    pub formats: Vec<(String, Format)>,
    /// `(column, rule)`, in the order the rules are tried.
    pub rules: Vec<(String, Rule)>,
    pub filter: String,
    /// `(column, descending)`.
    pub sort: Option<(String, bool)>,
}

impl TableSettings {
    fn entry(&mut self) -> &mut Self {
        self
    }
}

impl Project {
    /// The settings for `table`, if the project has any.
    pub fn table(&self, name: &str) -> Option<&TableSettings> {
        self.tables.iter().find(|t| t.name == name)
    }

    fn table_mut(&mut self, name: &str) -> &mut TableSettings {
        if let Some(i) = self.tables.iter().position(|t| t.name == name) {
            return self.tables[i].entry();
        }
        self.tables.push(TableSettings {
            name: name.to_string(),
            ..Default::default()
        });
        self.tables.last_mut().unwrap()
    }

    /// Build a project from what the app is holding.
    pub fn capture(
        database: &Path,
        browse: &Browse,
        tables: &[String],
        current: Option<Current<'_>>,
        statements: Vec<String>,
    ) -> Project {
        let mut project = Project {
            database: database.to_path_buf(),
            statements,
            ..Default::default()
        };
        for name in tables {
            let view = browse.view(name);
            // A table nobody touched has nothing worth writing down.
            if view.hidden.is_empty()
                && view.frozen == 0
                && !view.show_rowid
                && view.formats.is_empty()
                && view.rules.is_empty()
            {
                continue;
            }
            let mut settings = TableSettings {
                name: name.clone(),
                hidden: view.hidden.clone(),
                frozen: view.frozen,
                show_rowid: view.show_rowid,
                ..Default::default()
            };
            // Sorted, so the same session writes the same file twice running.
            let mut formats: Vec<_> = view.formats.iter().collect();
            formats.sort_by(|a, b| a.0.cmp(b.0));
            settings.formats = formats
                .into_iter()
                .map(|(c, f)| (c.clone(), f.clone()))
                .collect();
            let mut rules: Vec<_> = view.rules.iter().collect();
            rules.sort_by(|a, b| a.0.cmp(b.0));
            for (column, list) in rules {
                for rule in list {
                    settings.rules.push((column.clone(), rule.clone()));
                }
            }
            project.tables.push(settings);
        }
        if let Some((table, filter, sort)) = current {
            let entry = project.table_mut(table);
            entry.filter = filter.to_string();
            entry.sort = sort.map(|(c, d)| (c.to_string(), d));
        }
        project
    }

    /// The file's text.
    pub fn to_text(&self) -> String {
        let mut out = format!("{HEADER}\t{VERSION}\n");
        out.push_str(&format!("db\t{}\n", self.database.display()));
        for t in &self.tables {
            out.push_str(&format!("table\t{}\n", t.name));
            for h in &t.hidden {
                out.push_str(&format!("hidden\t{}\t{}\n", t.name, h));
            }
            if t.frozen > 0 {
                out.push_str(&format!("frozen\t{}\t{}\n", t.name, t.frozen));
            }
            if t.show_rowid {
                out.push_str(&format!("rowid\t{}\ton\n", t.name));
            }
            for (column, f) in &t.formats {
                // A custom format carries an expression rather than a name, so
                // it gets its own directive and everything after the column is
                // the expression.
                match f {
                    Format::Custom(expr) => out.push_str(&format!(
                        "format-custom\t{}\t{}\t{}\n",
                        t.name, column, expr
                    )),
                    other => out.push_str(&format!(
                        "format\t{}\t{}\t{}\n",
                        t.name,
                        column,
                        format_token(other)
                    )),
                }
            }
            for (column, rule) in &t.rules {
                out.push_str(&format!(
                    "rule\t{}\t{}\t{}\t{}\t{}\n",
                    t.name,
                    column,
                    rule.condition,
                    rule.color.label(),
                    if rule.bold { "bold" } else { "plain" }
                ));
            }
            if !t.filter.is_empty() {
                out.push_str(&format!("filter\t{}\t{}\n", t.name, t.filter));
            }
            if let Some((column, desc)) = &t.sort {
                out.push_str(&format!(
                    "sort\t{}\t{}\t{}\n",
                    t.name,
                    column,
                    if *desc { "desc" } else { "asc" }
                ));
            }
        }
        for sql in &self.statements {
            if !sql.trim().is_empty() {
                // A statement can span lines; the file cannot, so newlines are
                // escaped the way the SQL history file escapes them.
                out.push_str(&format!(
                    "sql\t{}\n",
                    sql.replace('\\', "\\\\").replace('\n', "\\n")
                ));
            }
        }
        out
    }

    /// Parse a project file. `None` when the text is not one.
    pub fn parse(text: &str) -> Option<Project> {
        let mut lines = text.lines();
        let first = lines.next()?;
        if first.split('\t').next()? != HEADER {
            return None;
        }
        let mut project = Project::default();
        for line in lines {
            let mut f = line.split('\t');
            let (kind, rest) = (f.next().unwrap_or(""), f.collect::<Vec<_>>());
            match (kind, rest.as_slice()) {
                ("db", [path]) => project.database = PathBuf::from(path),
                ("table", [name]) => {
                    project.table_mut(name);
                }
                ("hidden", [table, column]) => {
                    project.table_mut(table).hidden.push(column.to_string())
                }
                ("frozen", [table, n]) => {
                    project.table_mut(table).frozen = n.parse().unwrap_or(0);
                }
                ("rowid", [table, on]) => {
                    project.table_mut(table).show_rowid = *on == "on";
                }
                ("format", [table, column, token]) => {
                    if let Some(f) = format_from_token(token) {
                        project
                            .table_mut(table)
                            .formats
                            .push((column.to_string(), f));
                    }
                }
                // A custom format's expression may itself hold tabs; everything
                // after the marker is the expression.
                ("format-custom", [table, column, rest @ ..]) => {
                    project
                        .table_mut(table)
                        .formats
                        .push((column.to_string(), Format::Custom(rest.join("\t"))));
                }
                ("rule", [table, column, condition, color, weight]) => {
                    let rule = Rule {
                        condition: condition.to_string(),
                        color: color_from_label(color),
                        bold: *weight == "bold",
                    };
                    project
                        .table_mut(table)
                        .rules
                        .push((column.to_string(), rule));
                }
                ("filter", [table, filter]) => {
                    project.table_mut(table).filter = filter.to_string();
                }
                ("sort", [table, column, dir]) => {
                    project.table_mut(table).sort = Some((column.to_string(), *dir == "desc"));
                }
                ("sql", rest) => {
                    let joined = rest.join("\t");
                    project.statements.push(unescape(&joined));
                }
                // Anything this version does not know about.
                _ => {}
            }
        }
        Some(project)
    }
}

/// Undo the newline escaping `to_text` applies.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A display format as one word, so the file stays greppable. A custom format
/// has no name; it is written on its own directive instead.
fn format_token(f: &Format) -> String {
    f.label().replace(' ', "-")
}

fn format_from_token(token: &str) -> Option<Format> {
    Format::CYCLE
        .iter()
        .find(|f| f.label().replace(' ', "-") == token)
        .cloned()
}

fn color_from_label(label: &str) -> RuleColor {
    RuleColor::ALL
        .iter()
        .copied()
        .find(|c| c.label() == label)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Project {
        Project {
            database: PathBuf::from("/tmp/x.db"),
            tables: vec![TableSettings {
                name: "orders".into(),
                hidden: vec!["note".into()],
                frozen: 2,
                show_rowid: true,
                formats: vec![
                    ("total".into(), Format::HexNumber),
                    ("when".into(), Format::UnixEpochLocal),
                ],
                rules: vec![
                    (
                        "total".into(),
                        Rule {
                            condition: "> 100".into(),
                            color: RuleColor::Red,
                            bold: true,
                        },
                    ),
                    (
                        "total".into(),
                        Rule {
                            condition: "null".into(),
                            color: RuleColor::Gray,
                            bold: false,
                        },
                    ),
                ],
                filter: "tag:keep".into(),
                sort: Some(("total".into(), true)),
            }],
            statements: vec!["SELECT 1".into(), "SELECT 2\nFROM t".into()],
        }
    }

    #[test]
    fn a_project_survives_a_round_trip() {
        let p = sample();
        let text = p.to_text();
        let back = Project::parse(&text).expect("parses");
        assert_eq!(back, p);
    }

    #[test]
    fn the_text_is_readable_and_greppable() {
        let text = sample().to_text();
        assert!(text.starts_with("zdbview-project\t1\n"));
        assert!(text.contains("db\t/tmp/x.db\n"));
        assert!(text.contains("frozen\torders\t2\n"));
        assert!(text.contains("format\torders\ttotal\thex-number\n"));
        assert!(text.contains("rule\torders\ttotal\t> 100\tred\tbold\n"));
        assert!(text.contains("sort\torders\ttotal\tdesc\n"));
        // A multi-line statement stays one line in the file.
        assert!(text.contains("sql\tSELECT 2\\nFROM t\n"));
    }

    #[test]
    fn a_custom_format_keeps_its_expression() {
        let mut p = Project::default();
        p.table_mut("t")
            .formats
            .push(("a".into(), Format::Custom("substr(%1, 1, 3)".into())));
        let back = Project::parse(&p.to_text()).unwrap();
        assert_eq!(
            back.table("t").unwrap().formats[0].1,
            Format::Custom("substr(%1, 1, 3)".into())
        );
    }

    #[test]
    fn a_directive_from_a_later_version_is_skipped_not_refused() {
        let text = "zdbview-project\t2\ndb\t/tmp/x.db\nsomething-new\tfoo\tbar\ntable\tt\n";
        let p = Project::parse(text).expect("still parses");
        assert_eq!(p.database, PathBuf::from("/tmp/x.db"));
        assert!(p.table("t").is_some());
    }

    #[test]
    fn text_that_is_not_a_project_is_refused() {
        assert!(Project::parse("").is_none());
        assert!(Project::parse("CREATE TABLE t (a);").is_none());
    }

    #[test]
    fn capture_leaves_out_the_tables_nobody_touched() {
        let mut browse = Browse::default();
        browse.view_mut("touched").frozen = 1;
        let p = Project::capture(
            Path::new("/tmp/x.db"),
            &browse,
            &["touched".to_string(), "untouched".to_string()],
            None,
            Vec::new(),
        );
        assert_eq!(p.tables.len(), 1);
        assert_eq!(p.tables[0].name, "touched");
    }

    #[test]
    fn capture_records_what_the_grid_is_doing_right_now() {
        let browse = Browse::default();
        let p = Project::capture(
            Path::new("/tmp/x.db"),
            &browse,
            &["t".to_string()],
            Some(("t", "tag:keep", Some(("total", true)))),
            vec!["SELECT 1".into()],
        );
        let t = p
            .table("t")
            .expect("the current table is recorded even if untouched");
        assert_eq!(t.filter, "tag:keep");
        assert_eq!(t.sort, Some(("total".to_string(), true)));
        assert_eq!(p.statements, vec!["SELECT 1".to_string()]);
    }
}
