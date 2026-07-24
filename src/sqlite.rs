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
    pub fn rows(&self, table: &str, limit: i64, offset: i64) -> Result<RowsView> {
        let columns = self.columns(table)?;
        let total = self.count(table)?;
        let ncols = columns.len();

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

        // `ORDER BY rowid` makes paging and search-positioning deterministic:
        // a matching rowid's ordinal position is then well-defined.
        let sql = if with_rowid {
            format!(
                "SELECT rowid, * FROM \"{}\" ORDER BY rowid LIMIT {} OFFSET {}",
                esc(table),
                limit,
                offset
            )
        } else {
            format!(
                "SELECT * FROM \"{}\" LIMIT {} OFFSET {}",
                esc(table),
                limit,
                offset
            )
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows_out = Vec::new();
        let mut rowids = Vec::new();
        let mut q = stmt.query([])?;
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
    ) -> Result<Option<i64>> {
        if columns.is_empty() {
            return Ok(None);
        }
        let likes = columns
            .iter()
            .map(|c| format!("CAST(\"{}\" AS TEXT) LIKE ?1 ESCAPE '\\'", esc(c)))
            .collect::<Vec<_>>()
            .join(" OR ");
        let (cmp, ord) = if forward { (">", "ASC") } else { ("<", "DESC") };
        let sql = format!(
            "SELECT rowid FROM \"{}\" WHERE rowid {} ?2 AND ({}) ORDER BY rowid {} LIMIT 1",
            esc(table),
            cmp,
            likes,
            ord
        );
        let pattern = format!("%{}%", like_escape(term));
        let mut stmt = self.conn.prepare(&sql)?;
        let rid = stmt
            .query_row(params![pattern, from_rowid], |r| r.get::<_, i64>(0))
            .optional()?;
        Ok(rid)
    }

    /// 1-based ordinal position of `rowid` within the `ORDER BY rowid` ordering.
    pub fn rowid_ordinal(&self, table: &str, rowid: i64) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM \"{}\" WHERE rowid <= ?1", esc(table)),
            params![rowid],
            |r| r.get(0),
        )?;
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
