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

    /// `WHERE` body matching `term` against every column as text, plus the bound
    /// pattern. `None` when there is no filter.
    fn filter_clause(columns: &[String], term: &str) -> Option<(String, String)> {
        if term.is_empty() || columns.is_empty() {
            return None;
        }
        let likes = columns
            .iter()
            .map(|c| format!("CAST(\"{}\" AS TEXT) LIKE ?1 ESCAPE '\\'", esc(c)))
            .collect::<Vec<_>>()
            .join(" OR ");
        Some((
            format!(" WHERE ({likes})"),
            format!("%{}%", like_escape(term)),
        ))
    }

    /// Rows matching `filter`, i.e. how many the filtered grid has in total.
    pub fn count_filtered(&self, table: &str, filter: &str) -> Result<i64> {
        let columns = self.columns(table)?;
        match Self::filter_clause(&columns, filter) {
            None => self.count(table),
            Some((where_sql, pattern)) => {
                let sql = format!("SELECT COUNT(*) FROM \"{}\"{}", esc(table), where_sql);
                Ok(self.conn.query_row(&sql, params![pattern], |r| r.get(0))?)
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
            Some((_, pattern)) => stmt.query(params![pattern])?,
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
    pub fn find_row(
        &self,
        table: &str,
        columns: &[String],
        term: &str,
        from_rowid: i64,
        forward: bool,
        sort: Option<&Sort>,
    ) -> Result<Option<i64>> {
        if columns.is_empty() {
            return Ok(None);
        }
        let likes = columns
            .iter()
            .map(|c| format!("CAST(\"{}\" AS TEXT) LIKE ?1 ESCAPE '\\'", esc(c)))
            .collect::<Vec<_>>()
            .join(" OR ");
        // Search must step through the rows in the order they are displayed, so
        // both the comparison and the ORDER BY follow the active sort.
        let sql = match sort {
            Some(s) if columns.contains(&s.column) => format!(
                "SELECT rowid FROM \"{}\" WHERE {} AND ({}) ORDER BY {} LIMIT 1",
                esc(table),
                s.compare_to_marker(table, forward),
                likes,
                if forward {
                    s.order_by()
                } else {
                    s.order_by_reversed()
                }
            ),
            _ => {
                let (cmp, ord) = if forward { (">", "ASC") } else { ("<", "DESC") };
                format!(
                    "SELECT rowid FROM \"{}\" WHERE rowid {} ?2 AND ({}) ORDER BY rowid {} LIMIT 1",
                    esc(table),
                    cmp,
                    likes,
                    ord
                )
            }
        };
        let pattern = format!("%{}%", like_escape(term));
        let mut stmt = self.conn.prepare(&sql)?;
        let rid = stmt
            .query_row(params![pattern, from_rowid], |r| r.get::<_, i64>(0))
            .optional()?;
        Ok(rid)
    }

    /// First (or last, when `forward` is false) matching row in display order —
    /// the wrap-around step of a search, and the entry point when nothing is
    /// selected yet. A marker-relative comparison cannot express this, because
    /// with a sort active there is no synthetic rowid that sits before every row.
    pub fn find_row_edge(
        &self,
        table: &str,
        columns: &[String],
        term: &str,
        forward: bool,
        sort: Option<&Sort>,
    ) -> Result<Option<i64>> {
        if columns.is_empty() {
            return Ok(None);
        }
        let likes = columns
            .iter()
            .map(|c| format!("CAST(\"{}\" AS TEXT) LIKE ?1 ESCAPE '\\'", esc(c)))
            .collect::<Vec<_>>()
            .join(" OR ");
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
            "SELECT rowid FROM \"{}\" WHERE ({}) ORDER BY {} LIMIT 1",
            esc(table),
            likes,
            order
        );
        let pattern = format!("%{}%", like_escape(term));
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_row(params![pattern], |r| r.get::<_, i64>(0))
            .optional()?)
    }

    /// 1-based ordinal position of `rowid` within the displayed ordering — which
    /// is the active sort when there is one, else `rowid`.
    pub fn rowid_ordinal(&self, table: &str, rowid: i64, sort: Option<&Sort>) -> Result<i64> {
        let sql = match sort {
            // Everything not strictly after the marker row is at or before it.
            Some(s) => format!(
                "SELECT COUNT(*) FROM \"{}\" WHERE NOT ({})",
                esc(table),
                s.compare_to_marker(table, true)
            ),
            None => format!("SELECT COUNT(*) FROM \"{}\" WHERE rowid <= ?2", esc(table)),
        };
        let n: i64 = self
            .conn
            .query_row(&sql, params![rowid, rowid], |r| r.get(0))?;
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
