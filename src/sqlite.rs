//! SQLite backend: full generic CRUD over any database file.
//!
//! Rows are addressed by `rowid` for edit/delete, which works for every
//! ordinary table. `WITHOUT ROWID` tables lose in-place edit/delete addressing
//! (the SELECT still lists them read-only).

use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

pub struct SqliteStore {
    conn: Connection,
    pub path: PathBuf,
    pub tables: Vec<String>,
}

/// Which column the row grid is ordered by, and in which direction. The display
/// order is the tuple `(column, rowid)` taken in `desc`'s direction, so paging,
/// ordinals and search all agree on one total order even when the sort column
/// holds duplicates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sort {
    pub column: String,
    pub desc: bool,
}

impl Sort {
    /// `ORDER BY` body for the display order.
    fn order_by(&self) -> String {
        let dir = self.dir();
        format!("\"{}\" {dir}, rowid {dir}", esc(&self.column))
    }

    /// Same, with both keys reversed — used to walk backwards.
    fn order_by_reversed(&self) -> String {
        let dir = if self.desc { "ASC" } else { "DESC" };
        format!("\"{}\" {dir}, rowid {dir}", esc(&self.column))
    }

    fn dir(&self) -> &'static str {
        if self.desc {
            "DESC"
        } else {
            "ASC"
        }
    }

    /// Row-value comparison against the row with `rowid = ?2`, in display order:
    /// `later` selects rows that come after it on screen.
    fn compare_to_marker(&self, table: &str, later: bool) -> String {
        // Ascending display order puts "later" rows above the marker tuple;
        // descending inverts it.
        let op = if later != self.desc { ">" } else { "<" };
        let col = esc(&self.column);
        format!(
            "(\"{col}\", rowid) {op} (SELECT \"{col}\", rowid FROM \"{t}\" WHERE rowid = ?2)",
            t = esc(table)
        )
    }
}

/// What one column looks like, for the statistics screen.
#[derive(Debug, Clone)]
pub struct ColumnStat {
    pub name: String,
    /// The type in the schema, which SQLite treats as an affinity hint only.
    pub declared: String,
    pub rows: i64,
    pub nulls: i64,
    pub distinct: i64,
    pub min: String,
    pub max: String,
    /// Mean of the cells actually stored as numbers, if any.
    pub avg: Option<f64>,
    /// How many cells are stored as a number, which is what says whether the
    /// declared type is being honoured.
    pub numeric: i64,
    pub longest: i64,
}

/// The three maintenance statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Maintenance {
    Vacuum,
    Analyze,
    Reindex,
}

impl Maintenance {
    pub fn label(self) -> &'static str {
        match self {
            Maintenance::Vacuum => "VACUUM",
            Maintenance::Analyze => "ANALYZE",
            Maintenance::Reindex => "REINDEX",
        }
    }
}

/// A parsed grid filter. A bare word matches any column; `name:value` restricts
/// the match to that column, which is DB Browser's per-column filter. Terms are
/// separated by whitespace and ANDed, so `cwd:zshrs echo` means "cwd contains
/// zshrs and some column contains echo".
///
/// `name:` is only read as a column when `name` is one — otherwise the whole
/// token stays a plain term, so filtering for `12:30` or `http://x` still works.
#[derive(Debug, Default, PartialEq)]
pub struct Filter {
    terms: Vec<Term>,
}

#[derive(Debug, PartialEq)]
enum Term {
    /// Matched against every column.
    Any(String),
    /// Matched against one named column.
    Column { column: String, value: String },
}

impl Filter {
    pub fn parse(text: &str, columns: &[String]) -> Self {
        let mut terms = Vec::new();
        for token in text.split_whitespace() {
            match token.split_once(':') {
                Some((name, value)) if !value.is_empty() => {
                    match columns.iter().find(|c| c.eq_ignore_ascii_case(name)) {
                        Some(column) => terms.push(Term::Column {
                            column: column.clone(),
                            value: value.to_string(),
                        }),
                        None => terms.push(Term::Any(token.to_string())),
                    }
                }
                _ => terms.push(Term::Any(token.to_string())),
            }
        }
        Filter { terms }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// The `WHERE` body for this filter and the patterns to bind, with parameters
    /// numbered from `first_param` so the same filter can sit beside a search
    /// pattern in one statement.
    fn sql(&self, columns: &[String], first_param: usize) -> (String, Vec<String>) {
        let mut parts = Vec::new();
        let mut binds = Vec::new();
        for term in &self.terms {
            let n = first_param + binds.len();
            match term {
                Term::Any(v) => {
                    if columns.is_empty() {
                        continue;
                    }
                    parts.push(format!("({})", SqliteStore::like_group(columns, n)));
                    binds.push(format!("%{}%", like_escape(v)));
                }
                Term::Column { column, value } => {
                    parts.push(format!(
                        "(CAST(\"{}\" AS TEXT) LIKE ?{n} ESCAPE '\\')",
                        esc(column)
                    ));
                    binds.push(format!("%{}%", like_escape(value)));
                }
            }
        }
        (parts.join(" AND "), binds)
    }
}

/// What a row search is looking for: the table and its columns, the term, and the
/// two things that decide which rows are eligible and in what order — the active
/// sort and the grid filter.
pub struct RowQuery<'a> {
    pub table: &'a str,
    pub columns: &'a [String],
    pub term: &'a str,
    pub sort: Option<&'a Sort>,
    pub filter: &'a str,
}

/// A page of rows for one table, stringified for display.
pub struct RowsView {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// `rowid` for each row, used to target edit/delete. `None` when the table
    /// has no accessible rowid (a `WITHOUT ROWID` table).
    pub rowids: Vec<Option<i64>>,
    pub total: i64,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("open sqlite {}", path.display()))?;
        let tables = list_tables(&conn)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            tables,
        })
    }

    /// `col LIKE ?N OR col LIKE ?N …` over every column, for the search term and
    /// for a filter's any-column terms.
    fn like_group(columns: &[String], param: usize) -> String {
        columns
            .iter()
            .map(|c| format!("CAST(\"{}\" AS TEXT) LIKE ?{param} ESCAPE '\\'", esc(c)))
            .collect::<Vec<_>>()
            .join(" OR ")
    }

    /// ` WHERE …` for a filter plus the patterns to bind, or `None` when the
    /// filter selects everything.
    fn filter_clause(columns: &[String], filter: &str) -> Option<(String, Vec<String>)> {
        let parsed = Filter::parse(filter, columns);
        if parsed.is_empty() {
            return None;
        }
        let (body, binds) = parsed.sql(columns, 1);
        if body.is_empty() {
            return None;
        }
        Some((format!(" WHERE {body}"), binds))
    }

    /// Rows matching `filter`, i.e. how many the filtered grid has in total.
    pub fn count_filtered(&self, table: &str, filter: &str) -> Result<i64> {
        let columns = self.columns(table)?;
        match Self::filter_clause(&columns, filter) {
            None => self.count(table),
            Some((where_sql, binds)) => {
                let sql = format!("SELECT COUNT(*) FROM \"{}\"{}", esc(table), where_sql);
                Ok(self
                    .conn
                    .query_row(&sql, rusqlite::params_from_iter(binds), |r| r.get(0))?)
            }
        }
    }

    pub fn count(&self, table: &str) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM \"{}\"", esc(table)),
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    pub fn columns(&self, table: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare(&format!("PRAGMA table_info(\"{}\")", esc(table)))?;
        let cols = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(cols)
    }

    /// Fetch one page of rows. Includes `rowid` for edit/delete addressing when
    /// the table exposes it.
    pub fn rows(
        &self,
        table: &str,
        limit: i64,
        offset: i64,
        sort: Option<&Sort>,
        filter: &str,
    ) -> Result<RowsView> {
        let columns = self.columns(table)?;
        let total = self.count_filtered(table, filter)?;
        let ncols = columns.len();
        let clause = Self::filter_clause(&columns, filter);
        let where_sql = clause.as_ref().map(|(w, _)| w.as_str()).unwrap_or("");

        // Try to select rowid alongside the real columns. Falls back to a
        // plain select for WITHOUT ROWID tables where `rowid` is not a column.
        let with_rowid = self
            .conn
            .prepare(&format!(
                "SELECT rowid, * FROM \"{}\" LIMIT {} OFFSET {}",
                esc(table),
                limit,
                offset
            ))
            .is_ok();

        // An explicit ORDER BY makes paging and search-positioning
        // deterministic: a matching rowid's ordinal position is then
        // well-defined. Without a chosen sort column that order is `rowid`.
        let sql = if with_rowid {
            let order = match sort {
                Some(s) if columns.contains(&s.column) => s.order_by(),
                _ => "rowid".to_string(),
            };
            format!(
                "SELECT rowid, * FROM \"{}\"{} ORDER BY {} LIMIT {} OFFSET {}",
                esc(table),
                where_sql,
                order,
                limit,
                offset
            )
        } else {
            // A WITHOUT ROWID table has no rowid tiebreaker; sort on the column
            // alone, which is still stable enough for display.
            let order = match sort {
                Some(s) if columns.contains(&s.column) => {
                    format!("\"{}\" {}", esc(&s.column), s.dir())
                }
                _ => "1".to_string(),
            };
            format!(
                "SELECT * FROM \"{}\"{} ORDER BY {} LIMIT {} OFFSET {}",
                esc(table),
                where_sql,
                order,
                limit,
                offset
            )
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows_out = Vec::new();
        let mut rowids = Vec::new();
        let mut q = match &clause {
            Some((_, binds)) => stmt.query(rusqlite::params_from_iter(binds))?,
            None => stmt.query([])?,
        };
        while let Some(row) = q.next()? {
            let (base, rid) = if with_rowid {
                (1usize, row.get::<_, i64>(0).ok())
            } else {
                (0usize, None)
            };
            rowids.push(rid);
            let mut cells = Vec::with_capacity(ncols);
            for i in 0..ncols {
                cells.push(value_to_string(row, base + i));
            }
            rows_out.push(cells);
        }

        Ok(RowsView {
            columns,
            rows: rows_out,
            rowids,
            total,
        })
    }

    /// Whole-table search: find the next/previous rowid (relative to
    /// `from_rowid`) whose textified columns contain `term`. Case-insensitive.
    pub fn find_row(&self, q: &RowQuery, from_rowid: i64, forward: bool) -> Result<Option<i64>> {
        let RowQuery {
            table,
            columns,
            term,
            sort,
            filter,
        } = *q;
        if columns.is_empty() {
            return Ok(None);
        }
        let likes = Self::like_group(columns, 1);
        // A filtered grid lists a subset, so the search has to walk that subset —
        // otherwise `n` lands on a row the filter hides and the ordinal that
        // positions it counts rows nobody can see. The search pattern is `?1` and
        // the marker rowid `?2`, so the filter's parameters start at `?3`.
        let (keep, keep_binds) = Self::and_filter(columns, filter, 3);
        // Search must step through the rows in the order they are displayed, so
        // both the comparison and the ORDER BY follow the active sort.
        let sql = match sort {
            Some(s) if columns.contains(&s.column) => format!(
                "SELECT rowid FROM \"{}\" WHERE {} AND ({}){} ORDER BY {} LIMIT 1",
                esc(table),
                s.compare_to_marker(table, forward),
                likes,
                keep,
                if forward {
                    s.order_by()
                } else {
                    s.order_by_reversed()
                }
            ),
            _ => {
                let (cmp, ord) = if forward { (">", "ASC") } else { ("<", "DESC") };
                format!(
                    "SELECT rowid FROM \"{}\" WHERE rowid {} ?2 AND ({}){} ORDER BY rowid {} LIMIT 1",
                    esc(table),
                    cmp,
                    likes,
                    keep,
                    ord
                )
            }
        };
        let pattern = format!("%{}%", like_escape(term));
        let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&pattern, &from_rowid];
        binds.extend(keep_binds.iter().map(|b| b as &dyn rusqlite::ToSql));
        let mut stmt = self.conn.prepare(&sql)?;
        let rid = stmt
            .query_row(binds.as_slice(), |r| r.get::<_, i64>(0))
            .optional()?;
        Ok(rid)
    }

    /// ` AND (filter…)` with parameters numbered from `first_param`, plus the
    /// patterns to bind. Empty when no filter is active.
    fn and_filter(columns: &[String], filter: &str, first_param: usize) -> (String, Vec<String>) {
        let parsed = Filter::parse(filter, columns);
        let (body, binds) = parsed.sql(columns, first_param);
        if body.is_empty() {
            (String::new(), Vec::new())
        } else {
            (format!(" AND {body}"), binds)
        }
    }

    /// First (or last, when `forward` is false) matching row in display order —
    /// the wrap-around step of a search, and the entry point when nothing is
    /// selected yet. A marker-relative comparison cannot express this, because
    /// with a sort active there is no synthetic rowid that sits before every row.
    pub fn find_row_edge(&self, q: &RowQuery, forward: bool) -> Result<Option<i64>> {
        let RowQuery {
            table,
            columns,
            term,
            sort,
            filter,
        } = *q;
        if columns.is_empty() {
            return Ok(None);
        }
        let likes = Self::like_group(columns, 1);
        // No marker row in this form, so the filter's parameters start at `?2`.
        let (keep, keep_binds) = Self::and_filter(columns, filter, 2);
        let order = match sort {
            Some(s) if columns.contains(&s.column) => {
                if forward {
                    s.order_by()
                } else {
                    s.order_by_reversed()
                }
            }
            _ => format!("rowid {}", if forward { "ASC" } else { "DESC" }),
        };
        let sql = format!(
            "SELECT rowid FROM \"{}\" WHERE ({}){} ORDER BY {} LIMIT 1",
            esc(table),
            likes,
            keep,
            order
        );
        let pattern = format!("%{}%", like_escape(term));
        let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&pattern];
        binds.extend(keep_binds.iter().map(|b| b as &dyn rusqlite::ToSql));
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_row(binds.as_slice(), |r| r.get::<_, i64>(0))
            .optional()?)
    }

    /// 1-based ordinal position of `rowid` within the displayed ordering — which
    /// is the active sort when there is one, else `rowid`.
    pub fn rowid_ordinal(
        &self,
        table: &str,
        rowid: i64,
        sort: Option<&Sort>,
        filter: &str,
    ) -> Result<i64> {
        let columns = self.columns(table)?;
        // The ordinal has to count the rows the grid actually lists, or a filtered
        // search scrolls to a page the row is not on. `?1` and `?2` are both the
        // marker rowid here, so the filter's parameters start at `?3`.
        let (keep, keep_binds) = Self::and_filter(&columns, filter, 3);
        let sql = match sort {
            // Everything not strictly after the marker row is at or before it.
            Some(s) => format!(
                "SELECT COUNT(*) FROM \"{}\" WHERE NOT ({}){}",
                esc(table),
                s.compare_to_marker(table, true),
                keep
            ),
            None => format!(
                "SELECT COUNT(*) FROM \"{}\" WHERE rowid <= ?2{}",
                esc(table),
                keep
            ),
        };
        let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&rowid, &rowid];
        binds.extend(keep_binds.iter().map(|b| b as &dyn rusqlite::ToSql));
        let n: i64 = self.conn.query_row(&sql, binds.as_slice(), |r| r.get(0))?;
        Ok(n)
    }

    /// Raw bytes of a single cell (blob or text) for the detail/value view.
    pub fn cell_bytes(&self, table: &str, rowid: i64, col: &str) -> Result<Vec<u8>> {
        let v: Vec<u8> = self.conn.query_row(
            &format!(
                "SELECT \"{}\" FROM \"{}\" WHERE rowid = ?1",
                esc(col),
                esc(table)
            ),
            params![rowid],
            |r| {
                use rusqlite::types::ValueRef;
                Ok(match r.get_ref(0)? {
                    ValueRef::Blob(b) => b.to_vec(),
                    ValueRef::Text(t) => t.to_vec(),
                    ValueRef::Integer(i) => i.to_string().into_bytes(),
                    ValueRef::Real(f) => f.to_string().into_bytes(),
                    ValueRef::Null => Vec::new(),
                })
            },
        )?;
        Ok(v)
    }

    /// Schema objects: `(type, name, sql)` for tables, views, and indexes.
    pub fn schema(&self) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )?;
        let out = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub fn update_cell(&self, table: &str, rowid: i64, col: &str, val: &str) -> Result<()> {
        self.conn.execute(
            &format!(
                "UPDATE \"{}\" SET \"{}\" = ?1 WHERE rowid = ?2",
                esc(table),
                esc(col)
            ),
            params![val, rowid],
        )?;
        Ok(())
    }

    pub fn delete_row(&self, table: &str, rowid: i64) -> Result<()> {
        self.conn.execute(
            &format!("DELETE FROM \"{}\" WHERE rowid = ?1", esc(table)),
            params![rowid],
        )?;
        Ok(())
    }

    /// Insert one row using each column's default value.
    pub fn insert_blank(&self, table: &str) -> Result<()> {
        self.conn.execute(
            &format!("INSERT INTO \"{}\" DEFAULT VALUES", esc(table)),
            [],
        )?;
        Ok(())
    }

    /// Run an arbitrary statement (the `:` command line). Returns rows affected.
    pub fn exec(&self, sql: &str) -> Result<usize> {
        Ok(self.conn.execute(sql, [])?)
    }

    /// Run one arbitrary statement. A statement that returns columns comes back
    /// as [`Outcome::Rows`] — the old path only reported an affected count, so
    /// `SELECT` looked like it did nothing — and anything else as the number of
    /// rows it changed. `limit` caps what is materialised for display.
    pub fn run(&self, sql: &str, limit: usize) -> Result<Outcome> {
        let mut stmt = self.conn.prepare(sql)?;
        if stmt.column_count() == 0 {
            // No result set: DML/DDL. `execute` reports the change count.
            drop(stmt);
            return Ok(Outcome::Changed(self.exec(sql)?));
        }
        let columns: Vec<String> = stmt
            .column_names()
            .into_iter()
            .map(|c| c.to_string())
            .collect();
        let ncols = columns.len();
        let mut rows = Vec::new();
        let mut truncated = false;
        let mut q = stmt.query([])?;
        while let Some(row) = q.next()? {
            if rows.len() >= limit {
                truncated = true;
                break;
            }
            rows.push((0..ncols).map(|i| value_to_string(row, i)).collect());
        }
        Ok(Outcome::Rows {
            columns,
            rows,
            truncated,
        })
    }

    /// The facts the sqlite3 shell prints for `.dbinfo`, plus the journal mode and
    /// the two user-visible versions. Each is a pragma, so this is cheap.
    pub fn db_info(&self) -> Vec<(String, String)> {
        let scalar = |sql: &str| -> String {
            self.conn
                .query_row(sql, [], |r| r.get::<_, rusqlite::types::Value>(0))
                .map(|v| match v {
                    rusqlite::types::Value::Integer(i) => i.to_string(),
                    rusqlite::types::Value::Text(t) => t,
                    rusqlite::types::Value::Real(f) => f.to_string(),
                    rusqlite::types::Value::Null => "—".into(),
                    rusqlite::types::Value::Blob(b) => format!("<{} bytes>", b.len()),
                })
                .unwrap_or_else(|e| format!("? ({e})"))
        };
        let mut out = vec![
            ("page size".into(), scalar("PRAGMA page_size")),
            ("page count".into(), scalar("PRAGMA page_count")),
            ("freelist pages".into(), scalar("PRAGMA freelist_count")),
            ("encoding".into(), scalar("PRAGMA encoding")),
            ("journal mode".into(), scalar("PRAGMA journal_mode")),
            ("synchronous".into(), scalar("PRAGMA synchronous")),
            ("auto vacuum".into(), scalar("PRAGMA auto_vacuum")),
            ("schema version".into(), scalar("PRAGMA schema_version")),
            ("user version".into(), scalar("PRAGMA user_version")),
            ("application id".into(), scalar("PRAGMA application_id")),
            ("foreign keys".into(), scalar("PRAGMA foreign_keys")),
        ];
        // Sizes the shell derives rather than reads.
        if let (Ok(ps), Ok(pc)) = (
            scalar("PRAGMA page_size").parse::<u64>(),
            scalar("PRAGMA page_count").parse::<u64>(),
        ) {
            out.push(("data size".into(), format!("{} bytes", ps * pc)));
        }
        let counts = |what: &str, sql: &str| -> (String, String) {
            (
                what.into(),
                self.conn
                    .query_row(sql, [], |r| r.get::<_, i64>(0))
                    .map(|n| n.to_string())
                    .unwrap_or_else(|_| "?".into()),
            )
        };
        out.push(counts(
            "tables",
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
        ));
        out.push(counts(
            "indexes",
            "SELECT count(*) FROM sqlite_master WHERE type='index'",
        ));
        out.push(counts(
            "views",
            "SELECT count(*) FROM sqlite_master WHERE type='view'",
        ));
        out.push(counts(
            "triggers",
            "SELECT count(*) FROM sqlite_master WHERE type='trigger'",
        ));
        out
    }

    /// `PRAGMA integrity_check` (or the cheaper `quick_check`), as the shell's
    /// `.intck` runs it. `Ok` reports the lines SQLite returned; a healthy database
    /// answers with the single line `ok`.
    pub fn integrity_check(&self, quick: bool) -> Result<Vec<String>> {
        let sql = if quick {
            "PRAGMA quick_check"
        } else {
            "PRAGMA integrity_check"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Foreign keys with no index to serve them, which is what the shell's
    /// `.lint fkey-indexes` reports: without one, every parent-row change scans
    /// the child table.
    pub fn missing_fk_indexes(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for table in &self.tables {
            let mut fks = self
                .conn
                .prepare(&format!("PRAGMA foreign_key_list(\"{}\")", esc(table)))?;
            let keys: Vec<(String, String)> = fks
                .query_map([], |r| Ok((r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?
                .filter_map(|r| r.ok())
                .collect();
            if keys.is_empty() {
                continue;
            }
            // Every index on the child table, with its first column.
            let mut idx = self
                .conn
                .prepare(&format!("PRAGMA index_list(\"{}\")", esc(table)))?;
            let indexes: Vec<String> = idx
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .collect();
            let mut first_cols: Vec<String> = Vec::new();
            for i in &indexes {
                let mut info = self
                    .conn
                    .prepare(&format!("PRAGMA index_info(\"{}\")", esc(i)))?;
                let cols: Vec<Option<String>> = info
                    .query_map([], |r| r.get::<_, Option<String>>(2))?
                    .filter_map(|r| r.ok())
                    .collect();
                if let Some(Some(c)) = cols.into_iter().next() {
                    first_cols.push(c);
                }
            }
            for (parent, child_col) in keys {
                if !first_cols
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(&child_col))
                {
                    out.push(format!(
                        "{table}.{child_col} -> {parent}: no index on the child column"
                    ));
                }
            }
        }
        Ok(out)
    }

    /// The query plan for `sql`, drawn as the shell's `.eqp` draws it: a
    /// `QUERY PLAN` header over an ASCII tree built from each step's id/parent.
    pub fn explain_plan(&self, sql: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        // id, parent, detail — the shell ignores the third column.
        let steps: Vec<(i64, i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(3)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(plan_tree(&steps))
    }

    /// Every object's `CREATE` plus its rows as `INSERT`s: the shell's `.dump`.
    /// `table` restricts it to one table.
    ///
    /// Two details decide whether the output can actually be replayed, and both
    /// were learned by diffing against `sqlite3 .dump`:
    ///
    /// * A **virtual table** must not be created with `CREATE VIRTUAL TABLE`,
    ///   because that runs the module's constructor and builds its shadow tables,
    ///   which the dump then tries to create again. The shell instead registers it
    ///   by inserting the row straight into `sqlite_schema` under
    ///   `PRAGMA writable_schema=ON`, and emits the shadow tables itself with
    ///   `CREATE TABLE IF NOT EXISTS`.
    /// * Values must be written as SQL **literals**, not as the display strings the
    ///   grid shows: a blob has to come out as `x'…'` or the data is lost.
    pub fn dump(&self, table: Option<&str>) -> Result<String> {
        let filter = match table {
            Some(t) => format!(" AND tbl_name = '{}'", t.replace('\'', "''")),
            None => String::new(),
        };
        let sql = format!(
            "SELECT type, name, sql FROM sqlite_master \
             WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'{filter} \
             ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 ELSE 2 END, name"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let objects: Vec<(String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(|r| r.ok())
            .collect();

        let virtuals: Vec<&String> = objects
            .iter()
            .filter(|(_, _, sql)| {
                sql.trim_start()
                    .to_uppercase()
                    .starts_with("CREATE VIRTUAL")
            })
            .map(|(_, name, _)| name)
            .collect();
        let is_shadow = |name: &str| {
            virtuals
                .iter()
                .any(|v| name.len() > v.len() + 1 && name.starts_with(&format!("{v}_")))
        };

        let mut out = String::from("PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\n");
        if !virtuals.is_empty() {
            out.push_str("PRAGMA writable_schema=ON;\n");
        }
        for (kind, name, create) in &objects {
            if kind != "table" {
                out.push_str(create.trim_end_matches(';'));
                out.push_str(";\n");
                continue;
            }
            if virtuals.contains(&name) {
                // Register the virtual table without running its constructor.
                out.push_str(&format!(
                    "INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES('table','{}','{}',0,'{}');\n",
                    name.replace('\'', "''"),
                    name.replace('\'', "''"),
                    create.trim_end_matches(';').replace('\'', "''")
                ));
                // Its data lives in the shadow tables, which follow.
                continue;
            }
            if is_shadow(name) {
                // The shadow table may already exist once the module sees its
                // schema row, so create it only if absent.
                let create = create.trim_end_matches(';');
                let created = create.replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1);
                out.push_str(&created);
                out.push_str(";\n");
            } else {
                out.push_str(create.trim_end_matches(';'));
                out.push_str(";\n");
            }
            for row in self.literal_rows(name)? {
                out.push_str(&format!(
                    "INSERT INTO {} VALUES({});\n",
                    quoted_name(name),
                    row.join(",")
                ));
            }
        }
        if !virtuals.is_empty() {
            out.push_str("PRAGMA writable_schema=OFF;\n");
        }
        out.push_str("COMMIT;\n");
        Ok(out)
    }

    /// Every row of `table` as SQL literals — the form a dump needs, where a blob
    /// is `x'…'` rather than a description of itself.
    fn literal_rows(&self, table: &str) -> Result<Vec<Vec<String>>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT * FROM {}", quoted_name(table)))?;
        let ncols = stmt.column_count();
        let mut out = Vec::new();
        let mut q = stmt.query([])?;
        while let Some(row) = q.next()? {
            let mut cells = Vec::with_capacity(ncols);
            for i in 0..ncols {
                cells.push(value_to_literal(row, i));
            }
            out.push(cells);
        }
        Ok(out)
    }

    /// `VACUUM INTO` — the shell's `.backup` in one statement, and the only way to
    /// copy a live database consistently without stopping writers.
    pub fn backup_to(&self, path: &std::path::Path) -> Result<()> {
        let target = path.to_string_lossy().replace('\'', "''");
        self.conn
            .execute_batch(&format!("VACUUM INTO '{target}'"))?;
        Ok(())
    }

    /// Attach another database file under `alias`, the shell's `.open`-adjacent
    /// `ATTACH`. Cross-database queries then work in the SQL editor.
    pub fn attach(&self, path: &Path, alias: &str) -> Result<()> {
        self.conn.execute(
            &format!("ATTACH DATABASE ?1 AS \"{}\"", esc(alias)),
            params![path.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn detach(&self, alias: &str) -> Result<()> {
        self.conn
            .execute_batch(&format!("DETACH DATABASE \"{}\"", esc(alias)))?;
        Ok(())
    }

    /// `(alias, file)` for every attached database — the shell's `.databases`.
    pub fn databases(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("PRAGMA database_list")?;
        let out = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
            })?
            .filter_map(|r| r.ok())
            .map(|(alias, file)| (alias, file.unwrap_or_else(|| "(temporary)".into())))
            .collect();
        Ok(out)
    }

    /// Indexes on `table` with their columns, in index order — the shell's
    /// `.indexes`, which the schema view does not break out per table.
    pub fn indexes(&self, table: &str) -> Result<Vec<(String, Vec<String>)>> {
        let mut stmt = self
            .conn
            .prepare(&format!("PRAGMA index_list(\"{}\")", esc(table)))?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .collect();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let mut info = self
                .conn
                .prepare(&format!("PRAGMA index_info(\"{}\")", esc(&name)))?;
            let cols: Vec<String> = info
                .query_map([], |r| r.get::<_, Option<String>>(2))?
                .filter_map(|r| r.ok())
                .flatten()
                .collect();
            out.push((name, cols));
        }
        Ok(out)
    }

    /// Index advice for `sql`, in the spirit of the shell's `.expert`: every table
    /// the planner chose to scan in full, with the columns that statement compares
    /// it on.
    ///
    /// This is not sqlite3_expert — that builds candidate indexes and re-plans
    /// against them, which rusqlite exposes no binding for. It reads the plan the
    /// planner actually produced and the statement's own column references, which
    /// covers the case that matters: a full scan of a table whose filter column
    /// could be indexed. Columns are attributed to a table only when the name is
    /// unambiguous, so a join on two tables that share a column name is reported
    /// without that column rather than with a guess.
    pub fn index_advice(&self, sql: &str) -> Result<Vec<String>> {
        let plan = self.explain_plan(sql)?;
        let scanned: Vec<String> = plan
            .iter()
            .filter_map(|line| {
                let rest = line.trim_start_matches(['|', '`', '-', ' ']);
                let name = rest.strip_prefix("SCAN ")?.split_whitespace().next()?;
                // A scan of a subquery or a materialized CTE names no real table.
                self.tables.iter().find(|t| t.as_str() == name).cloned()
            })
            .collect();
        if scanned.is_empty() {
            return Ok(Vec::new());
        }
        let mentioned = compared_columns(sql);
        let mut out = Vec::new();
        for table in scanned {
            let columns = self.columns(&table)?;
            // Only names this table has, and that no other table in the statement
            // also has — otherwise the attribution would be a guess.
            let mut useful: Vec<String> = Vec::new();
            for name in &mentioned {
                if !columns.iter().any(|c| c.eq_ignore_ascii_case(name)) {
                    continue;
                }
                let ambiguous = self
                    .tables
                    .iter()
                    .filter(|t| *t != &table)
                    .filter_map(|t| self.columns(t).ok())
                    .any(|cols| cols.iter().any(|c| c.eq_ignore_ascii_case(name)));
                if !ambiguous && !useful.iter().any(|c| c.eq_ignore_ascii_case(name)) {
                    useful.push(name.clone());
                }
            }
            let already: Vec<Vec<String>> = self
                .indexes(&table)?
                .into_iter()
                .map(|(_, cols)| cols)
                .collect();
            useful.retain(|c| {
                !already
                    .iter()
                    .any(|cols| cols.first().is_some_and(|f| f.eq_ignore_ascii_case(c)))
            });
            if useful.is_empty() {
                out.push(format!(
                    "{table}: full scan, and no unindexed column of it is compared here"
                ));
            } else {
                out.push(format!(
                    "{table}: full scan — CREATE INDEX \"{table}_{}\" ON \"{table}\"({});",
                    useful.join("_"),
                    useful
                        .iter()
                        .map(|c| format!("\"{}\"", esc(c)))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        Ok(out)
    }

    /// The foreign keys of `table`: `(child column, parent table, parent column)`.
    /// `PRAGMA foreign_key_list` leaves the parent column empty when the key
    /// targets the parent's primary key, so that case is resolved here — a caller
    /// wanting to follow the key needs a column name either way.
    pub fn foreign_keys(&self, table: &str) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare(&format!("PRAGMA foreign_key_list(\"{}\")", esc(table)))?;
        let raw: Vec<(String, String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(3)?, r.get(2)?, r.get(4)?)))?
            .filter_map(|r| r.ok())
            .collect();
        let mut out = Vec::with_capacity(raw.len());
        for (child, parent, parent_col) in raw {
            let col = match parent_col {
                Some(c) => c,
                None => self.primary_key(&parent)?.unwrap_or_else(|| "rowid".into()),
            };
            out.push((child, parent, col));
        }
        Ok(out)
    }

    /// The single-column primary key of `table`, if it has one.
    fn primary_key(&self, table: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare(&format!("PRAGMA table_info(\"{}\")", esc(table)))?;
        let keys: Vec<String> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))?
            .filter_map(|r| r.ok())
            .filter(|(_, pk)| *pk > 0)
            .map(|(name, _)| name)
            .collect();
        Ok(if keys.len() == 1 {
            keys.into_iter().next()
        } else {
            None
        })
    }

    /// The rowid of the row in `table` whose `column` equals `value`, which is how
    /// a foreign key is followed to the row it points at. Compared as text so the
    /// grid's display string can be used directly.
    pub fn rowid_where(&self, table: &str, column: &str, value: &str) -> Result<Option<i64>> {
        let sql = format!(
            "SELECT rowid FROM \"{}\" WHERE CAST(\"{}\" AS TEXT) = ?1 LIMIT 1",
            esc(table),
            esc(column)
        );
        Ok(self
            .conn
            .query_row(&sql, params![value], |r| r.get::<_, i64>(0))
            .optional()?)
    }

    /// One column's shape: what a `describe` in VisiData or `analyze-tables` in
    /// sqlite-utils reports. `min`/`max` come back as the display strings the grid
    /// would show, so a blob reads as its size rather than as bytes.
    pub fn column_stats(&self, table: &str) -> Result<Vec<ColumnStat>> {
        let columns = self.columns(table)?;
        let mut out = Vec::with_capacity(columns.len());
        let types: std::collections::HashMap<String, String> = self
            .conn
            .prepare(&format!("PRAGMA table_info(\"{}\")", esc(table)))?
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .filter_map(|r| r.ok())
            .collect();
        for c in &columns {
            // One pass per column. `avg` and the numeric count look only at cells
            // SQLite actually stores as numbers, so a text column of digits is not
            // averaged into nonsense.
            let sql = format!(
                "SELECT count(*), count(\"{c}\"), count(DISTINCT \"{c}\"), \
                 min(\"{c}\"), max(\"{c}\"), \
                 avg(CASE WHEN typeof(\"{c}\") IN ('integer','real') THEN \"{c}\" END), \
                 sum(typeof(\"{c}\") IN ('integer','real')), \
                 max(length(\"{c}\")) \
                 FROM \"{t}\"",
                c = esc(c),
                t = esc(table)
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let mut q = stmt.query([])?;
            let row = match q.next()? {
                Some(r) => r,
                None => continue,
            };
            let rows: i64 = row.get(0)?;
            let non_null: i64 = row.get(1)?;
            out.push(ColumnStat {
                name: c.clone(),
                declared: types.get(c).cloned().unwrap_or_default(),
                rows,
                nulls: rows - non_null,
                distinct: row.get(2)?,
                min: value_to_string(row, 3),
                max: value_to_string(row, 4),
                avg: row.get::<_, Option<f64>>(5)?,
                numeric: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                longest: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
            });
        }
        Ok(out)
    }

    /// The most common values in one column with their counts — VisiData's
    /// frequency table. Ties break by value so the list is stable between runs.
    pub fn frequency(&self, table: &str, column: &str, limit: i64) -> Result<Vec<(String, i64)>> {
        let sql = format!(
            "SELECT \"{c}\", count(*) AS n FROM \"{t}\" \
             GROUP BY \"{c}\" ORDER BY n DESC, \"{c}\" LIMIT ?1",
            c = esc(column),
            t = esc(table)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut q = stmt.query([limit])?;
        let mut out = Vec::new();
        while let Some(row) = q.next()? {
            out.push((value_to_string(row, 0), row.get(1)?));
        }
        Ok(out)
    }

    /// Write a cell as a blob. `update_cell` binds a string, which cannot express
    /// arbitrary bytes; this is what the hex editor commits through.
    pub fn update_cell_blob(&self, table: &str, rowid: i64, col: &str, bytes: &[u8]) -> Result<()> {
        self.conn.execute(
            &format!(
                "UPDATE \"{}\" SET \"{}\" = ?1 WHERE rowid = ?2",
                esc(table),
                esc(col)
            ),
            params![bytes, rowid],
        )?;
        Ok(())
    }

    /// Whether a cell is stored as a blob, which is what decides between the text
    /// editor and the hex editor.
    pub fn cell_is_blob(&self, table: &str, rowid: i64, col: &str) -> Result<bool> {
        let sql = format!(
            "SELECT typeof(\"{}\") FROM \"{}\" WHERE rowid = ?1",
            esc(col),
            esc(table)
        );
        let t: String = self.conn.query_row(&sql, params![rowid], |r| r.get(0))?;
        Ok(t == "blob")
    }

    /// `VACUUM`, `ANALYZE` or `REINDEX` — the maintenance DB Browser calls
    /// "Compact Database" and sqlite-utils exposes as its own subcommands. Returns
    /// the change in file size, which is the only visible result of a vacuum.
    pub fn maintain(&self, op: Maintenance) -> Result<i64> {
        let before = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0) as i64;
        self.conn.execute_batch(match op {
            Maintenance::Vacuum => "VACUUM",
            Maintenance::Analyze => "ANALYZE",
            Maintenance::Reindex => "REINDEX",
        })?;
        let after = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0) as i64;
        Ok(after - before)
    }

    /// Insert rows into `table` from parsed CSV, the shell's `.import`. The header
    /// row names the columns, so a file whose columns are ordered differently from
    /// the table still lands correctly; a header naming a column the table does
    /// not have is an error rather than a silent drop.
    ///
    /// Everything is inserted as text and left to SQLite's own affinity rules,
    /// which is what the shell does.
    pub fn import_rows(
        &self,
        table: &str,
        header: &[String],
        rows: &[Vec<String>],
    ) -> Result<usize> {
        let columns = self.columns(table)?;
        for h in header {
            if !columns.iter().any(|c| c == h) {
                return Err(anyhow::anyhow!("{table} has no column {h:?}"));
            }
        }
        let cols = header
            .iter()
            .map(|c| format!("\"{}\"", esc(c)))
            .collect::<Vec<_>>()
            .join(", ");
        let marks = (1..=header.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO \"{}\" ({}) VALUES ({})",
            esc(table),
            cols,
            marks
        );
        // One transaction, or a thousand-row file is a thousand fsyncs.
        self.conn.execute_batch("BEGIN")?;
        let mut done = 0usize;
        let result = (|| -> Result<()> {
            let mut stmt = self.conn.prepare(&sql)?;
            for row in rows {
                if row.len() != header.len() {
                    return Err(anyhow::anyhow!(
                        "row {} has {} fields, the header has {}",
                        done + 1,
                        row.len(),
                        header.len()
                    ));
                }
                stmt.execute(rusqlite::params_from_iter(row.iter()))?;
                done += 1;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(done)
            }
            Err(e) => {
                self.conn.execute_batch("ROLLBACK")?;
                Err(e)
            }
        }
    }

    /// Table and view names with their columns, for the SQL editor's completion.
    pub fn schema_names(&self) -> Vec<(String, Vec<String>)> {
        self.tables
            .iter()
            .map(|t| (t.clone(), self.columns(t).unwrap_or_default()))
            .collect()
    }
}

/// What running a statement produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A result set (`SELECT`, `PRAGMA`, `EXPLAIN`, a CTE …).
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        /// More rows were available than `limit` allowed.
        truncated: bool,
    },
    /// Rows changed by a statement that returns nothing.
    Changed(usize),
}

/// Render an `EXPLAIN QUERY PLAN` result the way the sqlite3 shell does: each
/// step under its parent, `|--` while siblings follow and `` `-- `` for the last
/// one, with `|  ` carried down for every ancestor that still has siblings.
fn plan_tree(steps: &[(i64, i64, String)]) -> Vec<String> {
    if steps.is_empty() {
        return Vec::new();
    }
    let mut out = vec!["QUERY PLAN".to_string()];
    fn walk(steps: &[(i64, i64, String)], parent: i64, prefix: &str, out: &mut Vec<String>) {
        let kids: Vec<&(i64, i64, String)> =
            steps.iter().filter(|(_, p, _)| *p == parent).collect();
        for (i, (id, _, detail)) in kids.iter().enumerate() {
            let last = i + 1 == kids.len();
            out.push(format!(
                "{prefix}{}{detail}",
                if last { "`--" } else { "|--" }
            ));
            walk(
                steps,
                *id,
                &format!("{prefix}{}", if last { "   " } else { "|  " }),
                out,
            );
        }
    }
    walk(steps, 0, "", &mut out);
    out
}

/// A name as SQL: quoted only when it needs to be, which is how the shell writes
/// it (`INSERT INTO history …`, but `CREATE TABLE 'history_fts_data'`).
fn quoted_name(name: &str) -> String {
    if !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.chars().next().unwrap().is_ascii_digit()
    {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// One cell as a SQL literal, for a dump: `NULL`, a bare number, a quoted string,
/// or `x'…'` for a blob.
fn value_to_literal(row: &rusqlite::Row, idx: usize) -> String {
    match row.get_ref(idx) {
        Ok(ValueRef::Null) => "NULL".into(),
        Ok(ValueRef::Integer(i)) => i.to_string(),
        Ok(ValueRef::Real(f)) => {
            // A float has to round-trip, and needs a decimal point to stay a float.
            let s = format!("{f:?}");
            if s.contains(['.', 'e', 'E', 'n']) {
                s
            } else {
                format!("{s}.0")
            }
        }
        Ok(ValueRef::Text(t)) => format!("'{}'", String::from_utf8_lossy(t).replace('\'', "''")),
        Ok(ValueRef::Blob(b)) => {
            let mut out = String::with_capacity(b.len() * 2 + 3);
            out.push_str("x'");
            for byte in b {
                out.push_str(&format!("{byte:02x}"));
            }
            out.push('\'');
            out
        }
        Err(_) => "NULL".into(),
    }
}

fn value_to_string(row: &rusqlite::Row, idx: usize) -> String {
    match row.get_ref(idx) {
        Ok(ValueRef::Null) => "NULL".into(),
        Ok(ValueRef::Integer(i)) => i.to_string(),
        Ok(ValueRef::Real(f)) => f.to_string(),
        Ok(ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        Ok(ValueRef::Blob(b)) => format!("<blob {} bytes>", b.len()),
        Err(_) => "?".into(),
    }
}

fn list_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master \
         WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names)
}

/// Escape a double-quoted SQL identifier.
/// Identifiers a statement compares on: what follows `WHERE`, `AND`, `OR`, `ON`,
/// `ORDER BY` or `GROUP BY`. Deliberately shallow — a real parser is not needed to
/// notice `WHERE cwd = ?`, and anything it misses simply produces no advice.
fn compared_columns(sql: &str) -> Vec<String> {
    let lowered = sql.to_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        .filter(|w| !w.is_empty())
        .collect();
    const AFTER: &[&str] = &["where", "and", "or", "on", "by", "having"];
    let mut out = Vec::new();
    for (i, w) in words.iter().enumerate() {
        if !AFTER.contains(w) {
            continue;
        }
        if let Some(next) = words.get(i + 1) {
            // `t.col` names the column; the qualifier is resolved by the caller,
            // which knows which tables are in play.
            let name = next.rsplit('.').next().unwrap_or(next);
            if name.chars().next().is_some_and(|c| c.is_alphabetic())
                && !AFTER.contains(&name)
                && !out.iter().any(|o: &String| o == name)
            {
                out.push(name.to_string());
            }
        }
    }
    out
}

fn esc(ident: &str) -> String {
    ident.replace('"', "\"\"")
}

/// Escape LIKE metacharacters so a search term is matched literally (paired
/// with `ESCAPE '\'` in the query).
fn like_escape(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
