//! SQLite backend: full generic CRUD over any database file.
//!
//! Rows are addressed by `rowid` for edit/delete, which works for every
//! ordinary table. `WITHOUT ROWID` tables lose in-place edit/delete addressing
//! (the SELECT still lists them read-only).

use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub struct SqliteStore {
    /// Boxed for the same reason as `cache`: a `Connection` carries a prepared-
    /// statement cache inline, and `Store` holds a `SqliteStore` by value.
    conn: Box<Connection>,
    pub path: PathBuf,
    pub tables: Vec<String>,
    /// What has been learned about this database so far. Boxed to keep the store
    /// itself small, since [`crate::store::Store`] holds one by value.
    cache: Box<Cache>,
    /// Installed on every connection this store owns, see [`Self::set_cancel`].
    cancel: Option<(i32, Cancel)>,
    /// Whether the edit savepoint is open — there are changes not yet written.
    /// See [`Self::begin_edit`].
    pending: std::cell::Cell<bool>,
}

/// Name of the savepoint every unwritten change sits inside. A savepoint rather
/// than a transaction because it nests: a schema edit and a bulk import each run
/// their own, inside whatever the session already has open.
const EDIT_SAVEPOINT: &str = "zdbview_edit";

/// Everything a store keeps between queries so a page does not have to re-derive
/// it. All of it is dropped by [`SqliteStore::invalidate`].
#[derive(Default)]
struct Cache {
    /// Per-table facts that hold until the schema changes: the column names,
    /// whether the table exposes `rowid`, and the primary key. Every page used
    /// to re-read all three, which on a filtered grid meant three extra
    /// statements per keystroke.
    meta: RefCell<HashMap<String, Arc<TableMeta>>>,
    /// Connections for counting in parallel, opened on first use and kept.
    /// Opening one is not cheap on a large database — a 23 GB file with a 300 MB
    /// WAL costs ~4.3 s of WAL replay — so they are never per-query.
    shards: RefCell<Vec<Connection>>,
    /// Cached rowid ranges per table, see [`SqliteStore::rowid_ranges`].
    ranges: RefCell<HashMap<String, RowidRanges>>,
}

/// Half-open `(lo, hi]` rowid ranges covering one table, shared out to the
/// counting threads.
type RowidRanges = Arc<Vec<(i64, i64)>>;

/// Asked periodically while a statement runs: `true` abandons it. This is how an
/// abandoned scan stops costing anything the moment the user types the next key.
pub type Cancel = Arc<dyn Fn() -> bool + Send + Sync>;

/// What one table's shape is, read once and cached.
#[derive(Debug)]
pub struct TableMeta {
    pub columns: Vec<String>,
    /// The table exposes `rowid`, so a row can be addressed and paged by it.
    /// False for a `WITHOUT ROWID` table.
    pub has_rowid: bool,
    /// Primary-key columns in key order — the row handle when there is no rowid.
    pub primary_key: Vec<String>,
}

/// Pragmas that decide how a large database reads. Every connection this module
/// opens gets them, because the counting threads matter as much as the UI's.
///
/// * `mmap_size` — pages arrive by mmap instead of `read()`, which is what makes
///   re-scanning a multi-gigabyte file cheap. Asking for more than the build
///   allows is not an error, SQLite clamps it (a default build stops at 2 GiB),
///   so ask for far more and take whatever is given.
/// * `cache_size` — negative means KiB, so 256 MiB of pages per connection
///   instead of the 2 MiB default. A filtered scan walks the same interior
///   b-tree pages over and over.
/// * `temp_store` — a sort or a materialised subquery stays out of the filesystem.
///
/// Best-effort by design: a build that rejects one of these must still open the
/// file, so the result is dropped.
fn tune(conn: &Connection, cache_kib: i64) {
    let _ = conn.execute_batch(&format!(
        "PRAGMA mmap_size = 1099511627776;
         PRAGMA cache_size = -{cache_kib};
         PRAGMA temp_store = MEMORY;"
    ));
    // The hot statements — one page, one range count — are otherwise re-prepared
    // on every keystroke.
    conn.set_prepared_statement_cache_capacity(64);
}

/// Page cache for a connection that answers whatever the user does next, and so
/// benefits from keeping the b-tree it last walked.
const CACHE_INTERACTIVE_KIB: i64 = 262_144;
/// Page cache for a counting shard. A shard walks its range once and never looks
/// back, so a large cache buys nothing and there is one of these per core.
const CACHE_SHARD_KIB: i64 = 8_192;

/// How many connections count in parallel. SQLite runs one statement on one
/// thread, so the only way to use the machine is to give each core its own
/// connection and its own slice of the table. Two cores are left for the UI and
/// the reader thread.
/// Wire `cancel` into `conn` as SQLite's progress handler.
fn install_cancel(conn: &Connection, check_ops: i32, cancel: &Cancel) {
    let cancel = Arc::clone(cancel);
    let _ = conn.progress_handler(check_ops, Some(move || cancel()));
}

fn shard_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2))
        .unwrap_or(2)
        .clamp(2, 16)
}

/// Below this wide a rowid span a parallel count is not worth its setup, so one
/// statement does it.
const PARALLEL_COUNT_FLOOR: i64 = 200_000;

/// Ranges per worker. Equal-width rowid ranges are wildly unbalanced on a table
/// that has been rewritten for years — in Ableton's file index the lowest eighth
/// of the rows starts at rowid 105,596,921 of 111,378,050, so an eighth of the
/// width would hold everything — and finding balanced cut points means walking
/// the whole index, which cost 5.8 s of cold reads on that file: more than the
/// count it was meant to speed up.
///
/// Cutting far finer than the worker count fixes the balance without reading
/// anything. Rowids are unique integers, so a range of width `w` holds at most
/// `w` rows: with this many ranges per worker no single range can hold more than
/// its share unless the rowids are spread over more than this multiple of the row
/// count, and an empty range costs one index seek.
const RANGES_PER_WORKER: i64 = 64;

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
        self.compare_to_marker_at(table, later, 2)
    }

    /// The same against the row named by `?1`, which is where a page fetch binds
    /// its marker.
    fn compare_to_marker_param(&self, table: &str, later: bool) -> String {
        self.compare_to_marker_at(table, later, 1)
    }

    fn compare_to_marker_at(&self, table: &str, later: bool, param: usize) -> String {
        // Ascending display order puts "later" rows above the marker tuple;
        // descending inverts it.
        let op = if later != self.desc { ">" } else { "<" };
        let col = esc(&self.column);
        format!(
            "(\"{col}\", rowid) {op} (SELECT \"{col}\", rowid FROM \"{t}\" WHERE rowid = ?{param})",
            t = esc(table)
        )
    }
}

/// One page request: where in which table, in what order, and what is already
/// known about it.
#[derive(Debug, Clone, Copy)]
pub struct PageQuery<'a> {
    pub table: &'a str,
    /// Rows per page.
    pub limit: i64,
    /// Position of the page's first row in the display order.
    pub offset: i64,
    pub sort: Option<&'a Sort>,
    pub filter: &'a str,
    /// The page on screen, which is what lets this one be fetched by cursor.
    pub hint: Option<&'a PageHint>,
    /// A counted total for this table and filter, when one is known. Without it
    /// the view reports the bound this page proves.
    pub known_total: Option<i64>,
    /// Per-column display formats, applied in the `SELECT` — the grid receives
    /// cells already turned into display strings, so a format has to happen in
    /// SQL to see a blob's bytes at all. Empty leaves the select as `*`.
    pub formats: &'a std::collections::HashMap<String, crate::browse::Format>,
}

/// The empty format map, for every query that shows a table plainly.
pub static NO_FORMATS: std::sync::LazyLock<
    std::collections::HashMap<String, crate::browse::Format>,
> = std::sync::LazyLock::new(Default::default);

impl<'a> PageQuery<'a> {
    /// The whole of `table`, unordered and unfiltered — what an export wants.
    pub fn all(table: &'a str, limit: i64) -> Self {
        PageQuery {
            table,
            limit,
            offset: 0,
            sort: None,
            filter: "",
            hint: None,
            known_total: None,
            formats: &NO_FORMATS,
        }
    }
}

/// The page the grid currently shows, so the next fetch can be expressed
/// relative to it instead of by position.
///
/// `LIMIT n OFFSET k` makes SQLite walk and discard `k` matching rows, so on a
/// filtered scan the cost of a page grows with how far in it is: measured on a
/// 6.5M-row table with a filter matching 270k rows, the first page took 26 ms
/// and `OFFSET 269000` took 6.2 s. Stepping to the neighbouring page instead
/// asks for the rows after (or before) a known one, which is one index seek
/// whatever the page number, and the last page is read backwards from the end.
#[derive(Debug, Clone, Copy)]
pub struct PageHint {
    /// Offset the loaded page starts at.
    pub offset: i64,
    /// `rowid` of its first row.
    pub first: i64,
    /// `rowid` of its last row.
    pub last: i64,
    /// How many rows it holds.
    pub len: i64,
}

/// How one page fetch addresses its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Walk from the start of the ordering — also what an absolute jump does,
    /// since only a position can express it.
    Offset,
    /// The rows after the loaded page's last row.
    After(i64),
    /// The rows before its first row, read backwards.
    Before(i64),
    /// The final rows, read backwards from the end of the ordering.
    Last,
}

impl Plan {
    /// Pick the cheapest form that lands on `offset`.
    fn pick(
        hint: Option<&PageHint>,
        offset: i64,
        limit: i64,
        known_total: Option<i64>,
        with_rowid: bool,
    ) -> Plan {
        // Without a rowid there is no marker to compare against.
        if !with_rowid {
            return Plan::Offset;
        }
        // The last page is cheap from the other end, but only a counted total says
        // which page that is.
        if known_total.is_some_and(|t| offset > 0 && offset + limit >= t) {
            return Plan::Last;
        }
        match hint {
            Some(h) if h.len > 0 && offset == h.offset + h.len => Plan::After(h.last),
            Some(h) if offset >= 0 && offset + limit == h.offset => Plan::Before(h.first),
            _ => Plan::Offset,
        }
    }

    /// Whether the rows come back in reverse display order.
    fn reads_backwards(self) -> bool {
        matches!(self, Plan::Before(_) | Plan::Last)
    }

    /// The marker rowid this plan compares against, if any.
    fn marker(self) -> Option<i64> {
        match self {
            Plan::After(r) | Plan::Before(r) => Some(r),
            Plan::Offset | Plan::Last => None,
        }
    }

    /// `LIMIT` for the statement. A last page may be shorter than a full one, and
    /// reading it backwards means asking for exactly what is left.
    fn take(self, probe: i64, offset: i64, known_total: Option<i64>) -> i64 {
        match (self, known_total) {
            (Plan::Last, Some(total)) => (total - offset).clamp(1, probe),
            _ => probe,
        }
    }

    /// `OFFSET` for the statement — zero for every plan that uses a marker,
    /// which is the whole point of them.
    fn sql_offset(self, offset: i64) -> i64 {
        match self {
            Plan::Offset => offset,
            _ => 0,
        }
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

/// How a row is addressed for an in-place write.
///
/// An ordinary table has a `rowid`, which is stable and unique whatever the row
/// holds. A `WITHOUT ROWID` table has none, so its primary key is the handle — and
/// that key can be several columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKey {
    Rowid(i64),
    /// `(column, value as text)` pairs that together identify one row.
    Primary(Vec<(String, String)>),
}

/// A page of rows for one table, stringified for display.
pub struct RowsView {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// `rowid` for each row, used to target edit/delete. `None` when the table
    /// has no accessible rowid (a `WITHOUT ROWID` table), where the primary key
    /// below is the handle instead.
    pub rowids: Vec<Option<i64>>,
    /// Primary-key columns, in key order, for a table with no rowid. Empty when
    /// the table has one, since then the rowid is the better handle.
    pub primary_key: Vec<String>,
    /// Rows the filter leaves. A lower bound unless `total_exact`, since the exact
    /// figure costs a full scan: a page only proves what its one extra fetched row
    /// shows, and the real total comes from [`SqliteStore::count_exact`].
    pub total: i64,
    /// `total` is the real total, not "at least this many".
    pub total_exact: bool,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("open sqlite {}", path.display()))?;
        Self::from_conn(conn, path)
    }

    /// A second store over the same file that can only read. This is what the
    /// query thread and the counting shards use: a reader cannot be asked to
    /// checkpoint a WAL, so browsing a database another process is writing —
    /// Ableton's file index, a browser profile — cannot disturb it.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open sqlite read-only {}", path.display()))?;
        Self::from_conn(conn, path)
    }

    fn from_conn(conn: Connection, path: &Path) -> Result<Self> {
        tune(&conn, CACHE_INTERACTIVE_KIB);
        let tables = list_tables(&conn)?;
        Ok(Self {
            conn: Box::new(conn),
            path: path.to_path_buf(),
            tables,
            cache: Box::default(),
            cancel: None,
            pending: std::cell::Cell::new(false),
        })
    }

    /// Let `cancel` abandon whatever this store is running, checked every
    /// `check_ops` SQLite instructions. It is installed on this connection and on
    /// every counting shard, including ones opened later — the shards are where
    /// the long scans happen, so cancelling only the main connection would leave
    /// them burning cores for a filter the user has already retyped.
    pub fn set_cancel(&mut self, check_ops: i32, cancel: Cancel) {
        install_cancel(&self.conn, check_ops, &cancel);
        for conn in self.cache.shards.borrow().iter() {
            install_cancel(conn, check_ops, &cancel);
        }
        self.cancel = Some((check_ops, cancel));
    }

    /// Forget the cached schema facts. Any statement the user runs may have
    /// changed the shape of a table, and a stale column list would then address
    /// the wrong cell.
    pub fn invalidate(&mut self) {
        self.cache.meta.borrow_mut().clear();
        self.cache.ranges.borrow_mut().clear();
        if let Ok(t) = list_tables(&self.conn) {
            self.tables = t;
        }
    }

    // ----- the edit buffer (DB Browser's Write / Revert Changes) -------------

    /// Whether anything has been changed and not yet written.
    pub fn has_pending(&self) -> bool {
        self.pending.get()
    }

    /// Open the edit savepoint if it is not open already. Every write goes
    /// through here, so nothing reaches the file until [`Self::write_changes`].
    ///
    /// This is what DB Browser does: an edit is a change to the session, not to
    /// the database, until it is written. The cost is that the change is visible
    /// only on this connection — a reader has no way to see another connection's
    /// open transaction — which is why the grid reads its pages here rather than
    /// off the query threads while anything is pending.
    fn begin_edit(&self) -> Result<()> {
        if !self.pending.get() {
            self.conn
                .execute_batch(&format!("SAVEPOINT {EDIT_SAVEPOINT}"))?;
            self.pending.set(true);
        }
        Ok(())
    }

    /// Commit every unwritten change. Returns false when there was nothing to
    /// write, so the caller can say so rather than claim a write happened.
    pub fn write_changes(&self) -> Result<bool> {
        if !self.pending.get() {
            return Ok(false);
        }
        self.conn
            .execute_batch(&format!("RELEASE {EDIT_SAVEPOINT}"))?;
        self.pending.set(false);
        Ok(true)
    }

    /// Throw every unwritten change away. The savepoint is rolled back and
    /// released, which leaves the database exactly as it was when the first
    /// unwritten edit was made.
    pub fn revert_changes(&mut self) -> Result<bool> {
        if !self.pending.get() {
            return Ok(false);
        }
        self.conn.execute_batch(&format!(
            "ROLLBACK TO {EDIT_SAVEPOINT}; RELEASE {EDIT_SAVEPOINT}"
        ))?;
        self.pending.set(false);
        // A reverted schema edit puts the old columns back, so everything
        // derived from the schema is stale.
        self.invalidate();
        Ok(true)
    }

    /// How many rows have been changed and what schema version is current — the
    /// pair that says whether an arbitrary statement actually wrote anything.
    fn write_marks(&self) -> (i64, i64) {
        let schema: i64 = self
            .conn
            .query_row("PRAGMA schema_version", [], |r| r.get(0))
            .unwrap_or(0);
        (self.conn.total_changes() as i64, schema)
    }

    /// Cached [`TableMeta`] for `table`.
    fn meta(&self, table: &str) -> Result<Arc<TableMeta>> {
        if let Some(m) = self.cache.meta.borrow().get(table) {
            return Ok(Arc::clone(m));
        }
        let columns = self.read_columns(table)?;
        // `WITHOUT ROWID` tables have no `rowid` column, so preparing the
        // statement that selects it is the test for one.
        let has_rowid = self
            .conn
            .prepare(&format!("SELECT rowid FROM \"{}\" LIMIT 0", esc(table)))
            .is_ok();
        let primary_key = if has_rowid {
            Vec::new()
        } else {
            self.primary_key_columns(table)?
        };
        let m = Arc::new(TableMeta {
            columns,
            has_rowid,
            primary_key,
        });
        self.cache
            .meta
            .borrow_mut()
            .insert(table.to_string(), Arc::clone(&m));
        Ok(m)
    }

    /// Whether `table` can be read at all. A table whose virtual-table module or
    /// tokenizer this binary does not have — Ableton's `search_aggregation` is
    /// FTS4 with a custom `AbletonTokenizer` — cannot be queried by anyone, and
    /// saying so in the list beats surfacing `unknown tokenizer` as if the
    /// selection had failed.
    pub fn unreadable_reason(&self, table: &str) -> Option<String> {
        match self
            .conn
            .prepare(&format!("SELECT 1 FROM \"{}\" LIMIT 0", esc(table)))
        {
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        }
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

    /// The exact number of rows the filter leaves, counted on every core.
    ///
    /// One SQLite statement runs on one thread, so the table is cut into rowid
    /// ranges (`rowid_ranges`) and each range counted on its own connection. On a
    /// 6.5M-row table with a filter that matches 270k rows this takes the count
    /// from 9.2 s to 1.6 s; a table whose rowid span is under
    /// `PARALLEL_COUNT_FLOOR`, or one with no rowid to cut on, is counted by a
    /// single statement because the setup would cost more than the scan.
    pub fn count_exact(&self, table: &str, filter: &str) -> Result<i64> {
        let meta = self.meta(table)?;
        // The filter's parameters are `?1..`, so the two range bounds take the
        // numbers after them.
        let (keep, binds) = Self::and_filter(&meta.columns, filter, 1);

        let params: Vec<&(dyn rusqlite::ToSql + Sync)> = binds
            .iter()
            .map(|b| b as &(dyn rusqlite::ToSql + Sync))
            .collect();
        match self.count_sharded(table, meta.has_rowid, &keep, &params)? {
            Some(n) => Ok(n),
            None => self.count_filtered(table, filter),
        }
    }

    /// Count the rows of `table` that satisfy `keep` (an already-built
    /// ` AND …` body whose parameters are `?1..`, or empty), split across cores by
    /// rowid range. `None` means the table cannot be split — no rowid, or too few
    /// rows for it to pay — and the caller should count it with one statement.
    fn count_sharded(
        &self,
        table: &str,
        has_rowid: bool,
        keep: &str,
        binds: &[&(dyn rusqlite::ToSql + Sync)],
    ) -> Result<Option<i64>> {
        if !has_rowid {
            return Ok(None);
        }
        let Some(ranges) = self.rowid_ranges(table)? else {
            return Ok(None);
        };
        // The caller's parameters are `?1..`, so the range bounds take the next
        // two numbers.
        let lo = binds.len() + 1;
        let hi = binds.len() + 2;
        let sql = format!(
            "SELECT COUNT(*) FROM \"{}\" WHERE rowid > ?{lo} AND rowid <= ?{hi}{keep}",
            esc(table)
        );

        let workers = shard_count().min(ranges.len());
        let mut pool = self.cache.shards.borrow_mut();
        self.fill_pool(&mut pool, workers)?;
        // Ranges outnumber workers, so they are taken one at a time rather than
        // dealt out in advance: a range that turns out to hold most of the rows
        // then delays only itself, and the empty ones cost an index seek each.
        let next = AtomicUsize::new(0);
        let totals: Vec<Result<i64>> = std::thread::scope(|scope| {
            let handles: Vec<_> = pool
                .iter_mut()
                .take(workers)
                .map(|conn| {
                    let (sql, ranges, next) = (&sql, &ranges, &next);
                    scope.spawn(move || -> Result<i64> {
                        let mut total = 0i64;
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&(lo_v, hi_v)) = ranges.get(i) else {
                                return Ok(total);
                            };
                            let mut params: Vec<&dyn rusqlite::ToSql> =
                                binds.iter().map(|b| *b as &dyn rusqlite::ToSql).collect();
                            params.push(&lo_v);
                            params.push(&hi_v);
                            let mut stmt = conn.prepare_cached(sql)?;
                            total += stmt.query_row(params.as_slice(), |r| r.get::<_, i64>(0))?;
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("count thread panicked")))
                })
                .collect()
        });
        let mut total = 0i64;
        for t in totals {
            total += t?;
        }
        Ok(Some(total))
    }

    /// Open pool connections until there are `want` of them.
    fn fill_pool(&self, pool: &mut Vec<Connection>, want: usize) -> Result<()> {
        while pool.len() < want {
            let conn = Connection::open_with_flags(
                &self.path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            tune(&conn, CACHE_SHARD_KIB);
            if let Some((ops, cancel)) = &self.cancel {
                install_cancel(&conn, *ops, cancel);
            }
            pool.push(conn);
        }
        Ok(())
    }

    /// Half-open rowid ranges `(lo, hi]` covering `table`, cached, or `None` when
    /// the table is too narrow for splitting the work to pay off.
    ///
    /// Only `MIN(rowid)` and `MAX(rowid)` are read — two index seeks — and the
    /// cuts are then arithmetic. See [`RANGES_PER_WORKER`] for why equal widths
    /// are enough despite rowids being unevenly spread.
    fn rowid_ranges(&self, table: &str) -> Result<Option<RowidRanges>> {
        if let Some(r) = self.cache.ranges.borrow().get(table) {
            return Ok(Some(Arc::clone(r)));
        }
        let (min, max): (i64, i64) = self.conn.query_row(
            &format!(
                "SELECT COALESCE(MIN(rowid), 0), COALESCE(MAX(rowid), 0) FROM \"{}\"",
                esc(table)
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // The span bounds the row count from above, so a narrow one means a small
        // table whatever the filter.
        let span = max.saturating_sub(min).saturating_add(1);
        if span < PARALLEL_COUNT_FLOOR {
            return Ok(None);
        }
        let count = (shard_count() as i64 * RANGES_PER_WORKER).min(span);
        let mut ranges = Vec::with_capacity(count as usize);
        // The low bound is exclusive, so the first range starts below `min`.
        let mut lo = min - 1;
        for k in 1..=count {
            let hi = if k == count {
                max
            } else {
                min - 1 + span * k / count
            };
            if hi > lo {
                ranges.push((lo, hi));
                lo = hi;
            }
        }
        let ranges = Arc::new(ranges);
        self.cache
            .ranges
            .borrow_mut()
            .insert(table.to_string(), Arc::clone(&ranges));
        Ok(Some(ranges))
    }

    pub fn columns(&self, table: &str) -> Result<Vec<String>> {
        Ok(self.meta(table)?.columns.clone())
    }

    fn read_columns(&self, table: &str) -> Result<Vec<String>> {
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
    ///
    /// `q.hint` describes the page currently on screen, which is what lets a step
    /// to the next or previous page skip the `OFFSET` scan; see [`PageHint`].
    pub fn rows(&self, q: &PageQuery) -> Result<RowsView> {
        let PageQuery {
            table,
            limit,
            offset,
            sort,
            filter,
            hint,
            known_total,
            formats,
        } = *q;
        let meta = self.meta(table)?;
        let columns = &meta.columns;
        let ncols = columns.len();
        let with_rowid = meta.has_rowid;
        // `total` is only needed for two things: knowing whether a next page
        // exists, and a number for the title. Asking SQLite to count is the wrong
        // way to learn the first — even a capped count has to scan as far as the
        // cap, so on a filtered grid it costs the whole table again. Fetching one
        // row more than the page needs answers it for free, and the exact total
        // arrives separately from `count_exact`.
        let probe = limit.saturating_add(1);

        let sorted_by = match sort {
            Some(s) if columns.contains(&s.column) => Some(s),
            _ => None,
        };
        let plan = Plan::pick(hint, offset, limit, known_total, with_rowid);

        // An explicit ORDER BY makes paging and search-positioning
        // deterministic: a matching rowid's ordinal position is then
        // well-defined. Without a chosen sort column that order is `rowid`.
        let order = match (with_rowid, sorted_by) {
            (true, Some(s)) => s.order_by(),
            (true, None) => "rowid".to_string(),
            // A WITHOUT ROWID table has no rowid tiebreaker; sort on the column
            // alone, which is still stable enough for display.
            (false, Some(s)) => format!("\"{}\" {}", esc(&s.column), s.dir()),
            (false, None) => "1".to_string(),
        };
        // A backwards or last-page fetch walks the ordering from the other end,
        // so it selects in reverse and the rows are flipped afterwards.
        let reversed = plan.reads_backwards();
        let order = if reversed {
            match (with_rowid, sorted_by) {
                (true, Some(s)) => s.order_by_reversed(),
                (true, None) => "rowid DESC".to_string(),
                (false, Some(s)) => format!(
                    "\"{}\" {}",
                    esc(&s.column),
                    if s.desc { "ASC" } else { "DESC" }
                ),
                (false, None) => "1 DESC".to_string(),
            }
        } else {
            order
        };

        // The marker comparison is `?1`, so a filter's parameters start at `?2`
        // whenever there is one; with no marker they start at `?1`.
        let marker = plan.marker();
        let first_param = if marker.is_some() { 2 } else { 1 };
        let (keep, binds) = Self::and_filter(columns, filter, first_param);
        let where_sql = match (marker, keep.is_empty()) {
            (Some(_), _) => {
                let cmp = match sorted_by {
                    // `compare_to_marker` reads the marker row's sort value with
                    // a subquery on `?1`, so both forms bind the same parameter.
                    Some(s) => s.compare_to_marker_param(table, !reversed),
                    None => format!("rowid {} ?1", if reversed { "<" } else { ">" }),
                };
                format!(" WHERE {cmp}{keep}")
            }
            (None, false) => format!(" WHERE {}", keep.trim_start_matches(" AND ")),
            (None, true) => String::new(),
        };
        let take = plan.take(probe, offset, known_total);
        // `*` unless a column has a display format, in which case every column is
        // named so the formatted ones can be expressions. The names are kept as
        // the columns' own, so nothing downstream has to know a format was used.
        let select = if formats.is_empty() {
            if with_rowid {
                "rowid, *".to_string()
            } else {
                "*".to_string()
            }
        } else {
            let list = columns
                .iter()
                .map(|c| {
                    let quoted = format!("\"{}\"", esc(c));
                    match formats.get(c) {
                        Some(f) => format!("{} AS {}", f.expression(&quoted), quoted),
                        None => quoted,
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            if with_rowid {
                format!("rowid, {list}")
            } else {
                list
            }
        };
        let sql = format!(
            "SELECT {select} FROM \"{}\"{where_sql} ORDER BY {order} LIMIT {take} OFFSET {}",
            esc(table),
            plan.sql_offset(offset)
        );

        let mut stmt = self.conn.prepare_cached(&sql)?;
        let mut rows_out = Vec::new();
        let mut rowids = Vec::new();
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::new();
        if let Some(m) = marker.as_ref() {
            params.push(m);
        }
        params.extend(binds.iter().map(|b| b as &dyn rusqlite::ToSql));
        let mut q = stmt.query(params.as_slice())?;
        while let Some(row) = q.next()? {
            let (base, rid) = if with_rowid {
                (1usize, row.get::<_, i64>(0).ok())
            } else {
                (0usize, None)
            };
            rowids.push(rid);
            let mut cells = Vec::with_capacity(ncols);
            for (i, name) in columns.iter().enumerate().take(ncols) {
                let cell = value_to_string(row, base + i);
                // A format that reads the bytes in another encoding asked for
                // hex; this is where it becomes text.
                cells.push(match formats.get(name) {
                    Some(f) if f.decodes_bytes() => f.decode(&cell),
                    _ => cell,
                });
            }
            rows_out.push(cells);
        }
        if reversed {
            rows_out.reverse();
            rowids.reverse();
        }
        // The extra row was only asked for to prove there is a next page. A
        // backwards read took it from the far end, so it is already at the front.
        let more = rows_out.len() as i64 > limit;
        if more {
            if reversed {
                rows_out.remove(0);
                rowids.remove(0);
            } else {
                rows_out.pop();
                rowids.pop();
            }
        }

        // Best total available: the counted one when it describes this grid,
        // otherwise what this page proves — which is exact once the page ends the
        // table.
        let (total, total_exact) = match known_total {
            Some(n) => (n, true),
            None if more => (offset + rows_out.len() as i64 + 1, false),
            None => (offset + rows_out.len() as i64, true),
        };

        Ok(RowsView {
            columns: columns.clone(),
            rows: rows_out,
            rowids,
            // A table with a rowid needs no key columns; one without them is
            // addressed by its primary key, so the view carries it.
            primary_key: meta.primary_key.clone(),
            total,
            total_exact,
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
        let meta = self.meta(table)?;
        // The ordinal has to count the rows the grid actually lists, or a filtered
        // search scrolls to a page the row is not on. The marker rowid is `?1` and
        // the filter's parameters start at `?2`.
        let (filter_body, filter_binds) = Self::and_filter(&meta.columns, filter, 2);
        let position = match sort {
            // Everything not strictly after the marker row is at or before it.
            Some(s) => format!("NOT ({})", s.compare_to_marker_param(table, true)),
            None => "rowid <= ?1".to_string(),
        };
        let mut binds: Vec<&(dyn rusqlite::ToSql + Sync)> = vec![&rowid];
        binds.extend(
            filter_binds
                .iter()
                .map(|b| b as &(dyn rusqlite::ToSql + Sync)),
        );

        // Like the total, this is a full scan of the filtered set, so it is split
        // across cores when the table is big enough to be worth it.
        let keep = format!(" AND {position}{filter_body}");
        if let Some(n) = self.count_sharded(table, meta.has_rowid, &keep, &binds)? {
            return Ok(n);
        }
        let sql = format!(
            "SELECT COUNT(*) FROM \"{}\" WHERE {position}{filter_body}",
            esc(table)
        );
        let params: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|b| *b as &dyn rusqlite::ToSql).collect();
        let n: i64 = self.conn.query_row(&sql, params.as_slice(), |r| r.get(0))?;
        Ok(n)
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

    /// ` WHERE …` addressing one row by `key`, with the values to bind. Text
    /// comparison, because the caller has the row as the grid displays it — which
    /// is also why a key column holding a blob cannot be addressed this way.
    fn where_key(key: &RowKey) -> (String, Vec<String>) {
        match key {
            RowKey::Rowid(id) => ("rowid = ?1".to_string(), vec![id.to_string()]),
            RowKey::Primary(pairs) => {
                let clause = pairs
                    .iter()
                    .enumerate()
                    .map(|(i, (c, _))| format!("CAST(\"{}\" AS TEXT) = ?{}", esc(c), i + 1))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                (clause, pairs.iter().map(|(_, v)| v.clone()).collect())
            }
        }
    }

    /// Update one cell of the row `key` addresses. Values bind as parameters, so a
    /// quote or a newline in the data is not a syntax problem.
    pub fn update_cell_keyed(
        &self,
        table: &str,
        key: &RowKey,
        col: &str,
        val: &str,
    ) -> Result<usize> {
        let (clause, mut binds) = Self::where_key(key);
        let sql = format!(
            "UPDATE \"{}\" SET \"{}\" = ?{} WHERE {}",
            esc(table),
            esc(col),
            binds.len() + 1,
            clause
        );
        binds.push(val.to_string());
        self.begin_edit()?;
        Ok(self.conn.execute(&sql, rusqlite::params_from_iter(binds))?)
    }

    /// The same, writing bytes — which `update_cell_keyed` cannot express.
    pub fn update_cell_blob_keyed(
        &self,
        table: &str,
        key: &RowKey,
        col: &str,
        bytes: &[u8],
    ) -> Result<usize> {
        let (clause, binds) = Self::where_key(key);
        let sql = format!(
            "UPDATE \"{}\" SET \"{}\" = ?{} WHERE {}",
            esc(table),
            esc(col),
            binds.len() + 1,
            clause
        );
        let mut params: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        params.push(&bytes);
        self.begin_edit()?;
        Ok(self.conn.execute(&sql, params.as_slice())?)
    }

    /// Put `NULL` in one cell — DB Browser's "Set to NULL". The other update
    /// paths bind the new value as text, which has no way to say NULL, so this
    /// writes the keyword into the statement instead.
    pub fn update_cell_null(&self, table: &str, key: &RowKey, col: &str) -> Result<usize> {
        let (clause, binds) = Self::where_key(key);
        let sql = format!(
            "UPDATE \"{}\" SET \"{}\" = NULL WHERE {}",
            esc(table),
            esc(col),
            clause
        );
        self.begin_edit()?;
        Ok(self.conn.execute(&sql, rusqlite::params_from_iter(binds))?)
    }

    pub fn delete_row_keyed(&self, table: &str, key: &RowKey) -> Result<usize> {
        let (clause, binds) = Self::where_key(key);
        let sql = format!("DELETE FROM \"{}\" WHERE {}", esc(table), clause);
        self.begin_edit()?;
        Ok(self.conn.execute(&sql, rusqlite::params_from_iter(binds))?)
    }

    /// One cell's bytes, addressed by `key` — the read side of a keyed edit.
    pub fn cell_bytes_keyed(&self, table: &str, key: &RowKey, col: &str) -> Result<Vec<u8>> {
        let (clause, binds) = Self::where_key(key);
        let sql = format!(
            "SELECT \"{}\" FROM \"{}\" WHERE {} LIMIT 1",
            esc(col),
            esc(table),
            clause
        );
        Ok(self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(binds), |r| {
                Ok(match r.get_ref(0)? {
                    ValueRef::Blob(b) => b.to_vec(),
                    ValueRef::Text(t) => t.to_vec(),
                    ValueRef::Integer(i) => i.to_string().into_bytes(),
                    ValueRef::Real(f) => f.to_string().into_bytes(),
                    ValueRef::Null => Vec::new(),
                })
            })?)
    }

    /// Whether the cell `key` addresses holds a blob.
    pub fn cell_is_blob_keyed(&self, table: &str, key: &RowKey, col: &str) -> Result<bool> {
        let (clause, binds) = Self::where_key(key);
        let sql = format!(
            "SELECT typeof(\"{}\") FROM \"{}\" WHERE {} LIMIT 1",
            esc(col),
            esc(table),
            clause
        );
        let t: String = self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(binds), |r| r.get(0))?;
        Ok(t == "blob")
    }

    /// Insert one row using each column's default value.
    pub fn insert_blank(&self, table: &str) -> Result<()> {
        self.begin_edit()?;
        self.conn.execute(
            &format!("INSERT INTO \"{}\" DEFAULT VALUES", esc(table)),
            [],
        )?;
        Ok(())
    }

    /// Run an arbitrary statement (the `:` command line). Returns rows affected.
    /// Run one statement that returns nothing.
    ///
    /// Whether it writes is not knowable from the text, so the edit savepoint is
    /// opened around it and dropped again when the statement turns out to have
    /// changed neither a row nor the schema. A `SELECT` typed into the editor
    /// must not leave the session looking dirty.
    pub fn exec(&self, sql: &str) -> Result<usize> {
        let opened = !self.pending.get();
        let before = self.write_marks();
        self.begin_edit()?;
        let n = self.conn.execute(sql, []);
        if opened && self.write_marks() == before {
            let _ = self.write_changes();
        }
        Ok(n?)
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
        let mut literals = Vec::new();
        let mut truncated = false;
        let mut q = stmt.query([])?;
        while let Some(row) = q.next()? {
            if rows.len() >= limit {
                truncated = true;
                break;
            }
            // Both forms in one pass: the grid needs the display strings and any
            // SQL output needs the literals, and re-running the query to get the
            // other would not even be the same rows.
            rows.push((0..ncols).map(|i| value_to_string(row, i)).collect());
            literals.push((0..ncols).map(|i| value_to_literal(row, i)).collect());
        }
        Ok(Outcome::Rows {
            columns,
            rows,
            literals,
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
    pub fn literal_rows(&self, table: &str) -> Result<Vec<Vec<String>>> {
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
        // `VACUUM INTO` cannot run inside a transaction, and an unwritten change
        // would not be in the copy anyway.
        if self.pending.get() {
            return Err(anyhow::anyhow!(
                "there are unwritten changes — write (W) or revert (R) them first"
            ));
        }
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

    /// The primary-key columns of `table`, in key order. Empty when it has none.
    pub fn primary_key_columns(&self, table: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare(&format!("PRAGMA table_info(\"{}\")", esc(table)))?;
        let mut keyed: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(5)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .filter(|(pk, _)| *pk > 0)
            .collect();
        // `pk` is the 1-based position within the key, so ordering by it gives the
        // key's own column order rather than the table's.
        keyed.sort_by_key(|(pk, _)| *pk);
        Ok(keyed.into_iter().map(|(_, name)| name).collect())
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

    /// `VACUUM`, `ANALYZE` or `REINDEX` — the maintenance DB Browser calls
    /// "Compact Database" and sqlite-utils exposes as its own subcommands. Returns
    /// the change in file size, which is the only visible result of a vacuum.
    pub fn maintain(&self, op: Maintenance) -> Result<i64> {
        if self.pending.get() {
            return Err(anyhow::anyhow!(
                "there are unwritten changes — write (W) or revert (R) them first"
            ));
        }
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
        // One transaction, or a thousand-row file is a thousand fsyncs. A
        // savepoint rather than BEGIN, because the session may already have the
        // edit savepoint open and a transaction cannot nest inside one.
        self.begin_edit()?;
        self.conn.execute_batch("SAVEPOINT zdbview_import")?;
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
                self.conn.execute_batch("RELEASE zdbview_import")?;
                Ok(done)
            }
            Err(e) => {
                self.conn
                    .execute_batch("ROLLBACK TO zdbview_import; RELEASE zdbview_import")?;
                Err(e)
            }
        }
    }

    // ----- database lifecycle (DB Browser's File menu) ----------------------

    /// Create an empty database and open it — DB Browser's "New Database".
    ///
    /// An empty file is not a SQLite database until something is written to it,
    /// so a page is forced out; otherwise the file zdbview just made would fail
    /// its own header check on the next open.
    pub fn create(path: &Path) -> Result<Self> {
        if path.exists() {
            return Err(anyhow::anyhow!("{} already exists", path.display()));
        }
        let store = Self::open(path)?;
        store
            .conn
            .execute_batch("PRAGMA user_version = 0; VACUUM")
            .with_context(|| format!("initialise {}", path.display()))?;
        Ok(store)
    }

    /// A database that lives only as long as this process — DB Browser's "New
    /// In-Memory Database". Its `path` is the SQLite URI, so everything that
    /// prints a path still has something to print.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("open an in-memory database")?;
        Self::from_conn(conn, Path::new(":memory:"))
    }

    /// Whether this store can write at all. A read-only connection reports it up
    /// front rather than failing at the first edit.
    pub fn is_readonly(&self) -> bool {
        // SQLite knows: this is `sqlite3_db_readonly`, not a guess from a failed
        // write.
        self.conn.is_readonly("main").unwrap_or(false)
    }

    /// Load a SQLite extension — DB Browser's "Load Extension". Extension
    /// loading is off in the C library by default and is enabled only for the
    /// duration of the call, which is what SQLite's own guard does.
    pub fn load_extension(&self, path: &Path) -> Result<()> {
        // Safety: the extension's entry point is C code this process cannot vet.
        // The user asked for this file by name, which is the same trust the
        // sqlite3 shell's `.load` extends.
        unsafe {
            let _guard = rusqlite::LoadExtensionGuard::new(&self.conn)
                .context("enable extension loading")?;
            self.conn
                .load_extension(path, None::<&str>)
                .with_context(|| format!("load {}", path.display()))?;
        }
        Ok(())
    }

    /// `PRAGMA optimize` — DB Browser's "Optimize". It runs whatever SQLite
    /// thinks is worth doing (usually `ANALYZE` on the indexes that have changed
    /// enough to matter), and reports what it ran.
    pub fn optimize(&self) -> Result<Vec<String>> {
        if self.pending.get() {
            return Err(anyhow::anyhow!(
                "there are unwritten changes — write (W) or revert (R) them first"
            ));
        }
        // `PRAGMA optimize(-1)` lists what it would do without doing it, which is
        // the only way to say what happened afterwards.
        let planned: Vec<String> = self
            .conn
            .prepare("PRAGMA optimize(-1)")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_default();
        self.conn.execute_batch("PRAGMA optimize")?;
        Ok(planned)
    }

    /// Run a file of SQL into the database — DB Browser's "Import from SQL
    /// file". The whole script is one savepoint, so a script that fails half way
    /// leaves nothing behind.
    ///
    /// A dump's own `BEGIN`/`COMMIT` pair is dropped first: the script runs
    /// inside this savepoint, where a nested `BEGIN` is an error, and the
    /// savepoint already gives the script the all-or-nothing the `BEGIN` was
    /// asking for. Without this, replaying `--export sql` — or any `sqlite3
    /// .dump`, which always writes that pair — failed on its first line.
    pub fn import_sql(&self, script: &str) -> Result<usize> {
        let script = strip_outer_transaction(script);
        let script = script.as_str();
        let before = self.conn.total_changes();
        self.begin_edit()?;
        self.conn.execute_batch("SAVEPOINT zdbview_script")?;
        match self.conn.execute_batch(script) {
            Ok(()) => {
                self.conn.execute_batch("RELEASE zdbview_script")?;
                Ok((self.conn.total_changes() - before) as usize)
            }
            Err(e) => {
                let _ = self
                    .conn
                    .execute_batch("ROLLBACK TO zdbview_script; RELEASE zdbview_script");
                Err(e.into())
            }
        }
    }

    // ----- Browse Data operations (DB Browser's data tab) -------------------

    /// Replace `find` with `to` in one column, over the rows the grid's filter
    /// leaves — DB Browser's Find and Replace, restricted to a column because a
    /// column is what the cursor is on.
    ///
    /// The replacement is `replace()`, so it is substring-wise and exact; a row
    /// whose value does not contain `find` is not touched, which is what makes
    /// the reported count meaningful. Like every write it lands in the edit
    /// buffer, so a replace that went wrong is one `R` away from undone.
    pub fn replace_in_column(
        &self,
        table: &str,
        column: &str,
        find: &str,
        to: &str,
        filter: &str,
    ) -> Result<usize> {
        if find.is_empty() {
            return Err(anyhow::anyhow!("nothing to find"));
        }
        let columns = self.columns(table)?;
        if !columns.iter().any(|c| c == column) {
            return Err(anyhow::anyhow!("{table} has no column {column:?}"));
        }
        // The filter's parameters come after this statement's own three.
        let (keep, filter_binds) = Self::and_filter(&columns, filter, 4);
        let sql = format!(
            "UPDATE \"{}\" SET \"{}\" = replace(CAST(\"{}\" AS TEXT), ?1, ?2) \
             WHERE instr(CAST(\"{}\" AS TEXT), ?3) > 0{keep}",
            esc(table),
            esc(column),
            esc(column),
            esc(column)
        );
        let mut binds: Vec<String> = vec![find.to_string(), to.to_string(), find.to_string()];
        binds.extend(filter_binds);
        self.begin_edit()?;
        Ok(self.conn.execute(&sql, rusqlite::params_from_iter(binds))?)
    }

    /// How many rows one column's value contains `find` in, under the same filter
    /// — what a replace would touch, asked before it is run.
    pub fn count_matches(
        &self,
        table: &str,
        column: &str,
        find: &str,
        filter: &str,
    ) -> Result<i64> {
        let columns = self.columns(table)?;
        let (keep, mut binds) = Self::and_filter(&columns, filter, 2);
        let sql = format!(
            "SELECT count(*) FROM \"{}\" WHERE instr(CAST(\"{}\" AS TEXT), ?1) > 0{keep}",
            esc(table),
            esc(column)
        );
        binds.insert(0, find.to_string());
        Ok(self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(binds), |r| r.get(0))?)
    }

    /// `CREATE VIEW` over what the grid is showing — DB Browser's "Save filter as
    /// view". The filter's patterns are inlined as literals, since a view cannot
    /// carry parameters.
    pub fn create_view_from_filter(&self, name: &str, table: &str, filter: &str) -> Result<String> {
        if name.trim().is_empty() {
            return Err(anyhow::anyhow!("the view needs a name"));
        }
        let columns = self.columns(table)?;
        let (keep, binds) = Self::and_filter(&columns, filter, 1);
        // Put each pattern back where its marker is, quoted as a SQL string.
        let mut where_sql = keep.trim_start_matches(" AND ").to_string();
        for (i, b) in binds.iter().enumerate() {
            where_sql = where_sql.replace(
                &format!("?{}", i + 1),
                &format!("'{}'", b.replace('\'', "''")),
            );
        }
        let sql = if where_sql.is_empty() {
            format!(
                "CREATE VIEW {} AS SELECT * FROM {}",
                crate::ddl::quote(name),
                crate::ddl::quote(table)
            )
        } else {
            format!(
                "CREATE VIEW {} AS SELECT * FROM {} WHERE {}",
                crate::ddl::quote(name),
                crate::ddl::quote(table),
                where_sql
            )
        };
        self.apply_ddl(&crate::ddl::AlterPlan {
            statements: vec![sql.clone()],
            rebuild: false,
        })?;
        Ok(sql)
    }

    /// Insert one row from typed values — DB Browser's "Insert Values". A `None`
    /// is a `NULL`, which is how a column with no value is told from an empty
    /// string.
    pub fn insert_values(&self, table: &str, values: &[(String, Option<String>)]) -> Result<usize> {
        if values.is_empty() {
            return self.insert_blank(table).map(|_| 1);
        }
        let cols = values
            .iter()
            .map(|(c, _)| format!("\"{}\"", esc(c)))
            .collect::<Vec<_>>()
            .join(", ");
        let marks = (1..=values.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("INSERT INTO \"{}\" ({cols}) VALUES ({marks})", esc(table));
        let binds: Vec<Option<String>> = values.iter().map(|(_, v)| v.clone()).collect();
        self.begin_edit()?;
        Ok(self.conn.execute(&sql, rusqlite::params_from_iter(binds))?)
    }

    /// Whether a view can be written to: SQLite refuses unless the view carries
    /// `INSTEAD OF` triggers for the statement. This is what DB Browser's "Unlock
    /// view editing" checks before it lets a cell be edited.
    pub fn view_is_writable(&self, name: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1 \
             AND upper(sql) LIKE '%INSTEAD OF%'",
            params![name],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Whether `name` is a view rather than a table.
    pub fn is_view(&self, name: &str) -> bool {
        matches!(self.object_sql(name), Ok(Some((ty, _))) if ty == "view")
    }

    // ----- editable pragmas (DB Browser's Edit Pragmas) ---------------------

    /// One pragma's current value, or `None` when it has no query form —
    /// `case_sensitive_like` is settable but not readable.
    pub fn pragma(&self, name: &str) -> Option<String> {
        self.conn
            .query_row(&format!("PRAGMA {name}"), [], |r| {
                r.get::<_, rusqlite::types::Value>(0)
            })
            .ok()
            .map(|v| match v {
                rusqlite::types::Value::Integer(i) => i.to_string(),
                rusqlite::types::Value::Real(f) => f.to_string(),
                rusqlite::types::Value::Text(t) => t,
                _ => String::new(),
            })
    }

    /// Set one pragma and report what it reads back as.
    ///
    /// The read-back is the point: SQLite accepts a pragma it will not apply
    /// rather than raising. `journal_mode` will not change inside a transaction,
    /// `max_page_count` clamps to the pages already in use, `page_size` and
    /// `auto_vacuum` only take effect when the file is next rewritten, and an
    /// unrecognised value is ignored outright. Showing the requested value would
    /// be a lie in every one of those cases.
    pub fn set_pragma(&self, name: &str, value: &str) -> Result<Option<String>> {
        // A pragma that changes how the file is written cannot be applied over
        // an open savepoint, and several are silently ignored inside one.
        if self.pending.get() {
            return Err(anyhow::anyhow!(
                "there are unwritten changes — write (W) or revert (R) them first"
            ));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
        {
            return Err(anyhow::anyhow!("{name} is not a pragma name"));
        }
        // A pragma value is never a bound parameter in SQLite, so it goes into
        // the text — quoted, and only after the value has been checked to be a
        // number or a bare word.
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            || value.is_empty()
        {
            return Err(anyhow::anyhow!("{value:?} is not a pragma value"));
        }
        self.conn
            .execute_batch(&format!("PRAGMA {name} = {value}"))
            .with_context(|| format!("PRAGMA {name} = {value}"))?;
        // Changing the page size or the vacuum mode also invalidates what has
        // been cached about the file's shape.
        Ok(self.pragma(name))
    }

    // ----- schema editing (DB Browser's Edit Table / Edit Index) -------------

    /// The `sqlite_master` row for one object, as `(type, sql)`. An auto-index
    /// has no `sql`, so it comes back with an empty statement rather than as a
    /// missing object.
    pub fn object_sql(&self, name: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT type, COALESCE(sql, '') FROM sqlite_master WHERE name = ?1",
                params![name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?)
    }

    /// One table's definition, parsed from its own `CREATE` statement. The
    /// pragmas cannot answer this — they do not report `COLLATE`, `CHECK`,
    /// `UNIQUE` or a foreign key — and a rebuild driven by them would drop all
    /// four, so the DDL is the source.
    pub fn table_def(&self, table: &str) -> Result<crate::ddl::TableDef> {
        let (ty, sql) = self
            .object_sql(table)?
            .ok_or_else(|| anyhow::anyhow!("{table} is not in this database"))?;
        if ty != "table" {
            return Err(anyhow::anyhow!("{table} is a {ty}, not a table"));
        }
        crate::ddl::parse_table(&sql)
            .ok_or_else(|| anyhow::anyhow!("{table} is not an ordinary table"))
    }

    /// One index's definition, parsed the same way.
    pub fn index_def(&self, name: &str) -> Result<crate::ddl::IndexDef> {
        let (ty, sql) = self
            .object_sql(name)?
            .ok_or_else(|| anyhow::anyhow!("{name} is not in this database"))?;
        if ty != "index" {
            return Err(anyhow::anyhow!("{name} is a {ty}, not an index"));
        }
        if sql.is_empty() {
            return Err(anyhow::anyhow!(
                "{name} is an index SQLite created for a constraint, so it has no definition to edit"
            ));
        }
        crate::ddl::parse_index(&sql).ok_or_else(|| anyhow::anyhow!("cannot parse {name}"))
    }

    /// Every object a rebuild of `table` has to put back: its indexes and
    /// triggers, which go with the table when it is dropped, and any view that
    /// names it, which a rename would leave dangling.
    ///
    /// A view is matched on the text of its statement because SQLite records no
    /// dependency for one; the identifier has to appear in the statement for the
    /// view to reference the table, so a text match cannot miss one, and a view
    /// it matches spuriously is only re-created as it already was.
    pub fn dependents(&self, table: &str) -> Result<Vec<crate::ddl::Dependent>> {
        let mut stmt = self.conn.prepare(
            "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
             WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
             ORDER BY CASE type WHEN 'index' THEN 0 WHEN 'trigger' THEN 1 ELSE 2 END, name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter(|(ty, _, sql)| {
                matches!(ty.as_str(), "index" | "trigger" | "view")
                    && crate::ddl::mentions_ident(sql, table)
            })
            .map(|(kind, name, sql)| crate::ddl::Dependent { kind, name, sql })
            .collect())
    }

    /// Run a schema edit. Everything happens in one transaction, so a statement
    /// that fails half way leaves the database exactly as it was.
    ///
    /// A rebuild additionally runs with foreign keys off and checks them at the
    /// end, which is SQLite's own documented procedure: dropping the old table
    /// would otherwise fire every child row's `ON DELETE` action, and the
    /// enforcement setting cannot be changed inside a transaction, so it is
    /// switched around the outside and put back afterwards.
    pub fn apply_ddl(&self, plan: &crate::ddl::AlterPlan) -> Result<()> {
        let fk_on: i64 = self
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap_or(0);
        if plan.rebuild && fk_on != 0 {
            self.conn.execute_batch("PRAGMA foreign_keys = OFF")?;
        }
        let run = || -> Result<()> {
            self.begin_edit()?;
            self.conn.execute_batch("SAVEPOINT zdbview_ddl")?;
            for sql in plan.statements.iter().filter(|s| !s.trim().is_empty()) {
                self.conn
                    .execute_batch(sql)
                    .with_context(|| sql.replace('\n', " "))?;
            }
            if plan.rebuild {
                let broken: Vec<String> = self
                    .conn
                    .prepare("PRAGMA foreign_key_check")?
                    .query_map([], |r| {
                        Ok(format!(
                            "{} row {} → {}",
                            r.get::<_, String>(0).unwrap_or_default(),
                            r.get::<_, i64>(1).unwrap_or_default(),
                            r.get::<_, String>(2).unwrap_or_default()
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                if !broken.is_empty() {
                    return Err(anyhow::anyhow!(
                        "the edit would break {} foreign key(s): {}",
                        broken.len(),
                        broken.join("; ")
                    ));
                }
            }
            self.conn.execute_batch("RELEASE zdbview_ddl")?;
            Ok(())
        };
        let opened = !self.pending.get();
        let result = run();
        if result.is_err() {
            let _ = self
                .conn
                .execute_batch("ROLLBACK TO zdbview_ddl; RELEASE zdbview_ddl");
            // A failed edit changed nothing, so the session is not dirty — unless
            // it already was before this call.
            if opened {
                let _ = self.write_changes();
            }
        }
        if plan.rebuild && fk_on != 0 {
            let _ = self.conn.execute_batch("PRAGMA foreign_keys = ON");
        }
        result
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
        /// The same cells as SQL literals. The display strings above describe a
        /// blob (`<blob 3 bytes>`) rather than carrying it, so anything writing SQL
        /// — `.mode insert`, a redirect — has to use these or it emits a quoted
        /// description of the bytes instead of the bytes.
        literals: Vec<Vec<String>>,
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

/// A script with its outermost `BEGIN …;` and closing `COMMIT;`/`END;` removed.
///
/// Only the first and last statements are considered, which is where a dump puts
/// them; a `BEGIN` in the middle of the script — a trigger body's, above all —
/// is left exactly where it is.
fn strip_outer_transaction(script: &str) -> String {
    let mut out = script.trim_end().to_string();

    // Leading: skip whitespace, comments and the PRAGMA lines a dump opens with,
    // then drop the transaction statement if that is what comes next.
    let mut at = 0;
    loop {
        let rest = &out[at..];
        let skipped = rest.len() - rest.trim_start().len();
        at += skipped;
        let rest = &out[at..];
        if rest.starts_with("--") {
            at += rest.find('\n').map(|n| n + 1).unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with("/*") {
            at += rest.find("*/").map(|n| n + 2).unwrap_or(rest.len());
            continue;
        }
        let word: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect::<String>()
            .to_ascii_uppercase();
        match word.as_str() {
            // A dump's `PRAGMA foreign_keys=OFF;` sits ahead of the BEGIN.
            "PRAGMA" => match rest.find(';') {
                Some(end) => at += end + 1,
                None => break,
            },
            "BEGIN" => {
                if let Some(end) = rest.find(';') {
                    out.replace_range(at..at + end + 1, "");
                }
                break;
            }
            _ => break,
        }
    }

    // Trailing: a final `COMMIT;`. A bare `END;` is left alone — it is how a
    // trigger body closes, and a dump full of triggers ends with one.
    let trimmed = out.trim_end();
    if let Some(body) = trimmed.strip_suffix(';') {
        let tail = body.rsplit(';').next().unwrap_or(body).trim();
        let tail_upper = tail.to_ascii_uppercase();
        let is_commit = matches!(
            tail_upper.as_str(),
            "COMMIT" | "COMMIT TRANSACTION" | "END TRANSACTION"
        );
        if is_commit {
            let cut = trimmed.len() - tail.len() - 1;
            out.truncate(cut);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database of `rows` rows in one table, with a `name` column holding
    /// `word-<i>` and every fifth row containing `kick`.
    fn fixture(name: &str, rows: i64) -> (PathBuf, SqliteStore) {
        let mut path = std::env::temp_dir();
        path.push(format!("zdbview_sqlite_{}_{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (name TEXT, n INTEGER)")
            .unwrap();
        {
            let tx = conn.unchecked_transaction().unwrap();
            let mut stmt = tx
                .prepare("INSERT INTO t (name, n) VALUES (?1, ?2)")
                .unwrap();
            for i in 0..rows {
                let word = if i % 5 == 0 {
                    format!("kick-{i}")
                } else {
                    format!("word-{i}")
                };
                stmt.execute(params![word, i]).unwrap();
            }
            drop(stmt);
            tx.commit().unwrap();
        }
        drop(conn);
        let store = SqliteStore::open(&path).unwrap();
        (path, store)
    }

    fn page<'a>(
        table: &'a str,
        limit: i64,
        offset: i64,
        filter: &'a str,
        hint: Option<&'a PageHint>,
        known_total: Option<i64>,
    ) -> PageQuery<'a> {
        PageQuery {
            table,
            limit,
            offset,
            sort: None,
            filter,
            hint,
            known_total,
            formats: &NO_FORMATS,
        }
    }

    fn hint_for(view: &RowsView, offset: i64) -> PageHint {
        PageHint {
            offset,
            first: view.rowids.first().copied().flatten().unwrap(),
            last: view.rowids.last().copied().flatten().unwrap(),
            len: view.rows.len() as i64,
        }
    }

    /// Stepping page by page with a cursor must land on exactly the rows an
    /// `OFFSET` fetch would return — that equivalence is the whole licence for
    /// skipping the offset scan.
    #[test]
    fn cursor_paging_matches_offset_paging_forwards_and_back() {
        let (path, store) = fixture("cursor", 250);
        let first = store.rows(&page("t", 50, 0, "", None, None)).unwrap();
        let mut hint = hint_for(&first, 0);

        for step in 1..5 {
            let offset = step * 50;
            let by_cursor = store
                .rows(&page("t", 50, offset, "", Some(&hint), None))
                .unwrap();
            let by_offset = store.rows(&page("t", 50, offset, "", None, None)).unwrap();
            assert_eq!(
                by_cursor.rowids, by_offset.rowids,
                "page at offset {offset} differs between cursor and offset paging"
            );
            assert_eq!(by_cursor.rows, by_offset.rows);
            hint = hint_for(&by_cursor, offset);
        }
        // And backwards, which reads the ordering from the far end.
        for step in (0..4).rev() {
            let offset = step * 50;
            let by_cursor = store
                .rows(&page("t", 50, offset, "", Some(&hint), None))
                .unwrap();
            let by_offset = store.rows(&page("t", 50, offset, "", None, None)).unwrap();
            assert_eq!(
                by_cursor.rowids, by_offset.rowids,
                "backwards page at offset {offset} differs"
            );
            hint = hint_for(&by_cursor, offset);
        }
        let _ = std::fs::remove_file(path);
    }

    /// The same for a filtered grid, where a cursor step is the difference between
    /// one index seek and re-walking every earlier match.
    #[test]
    fn cursor_paging_matches_offset_paging_under_a_filter() {
        let (path, store) = fixture("cursor_filtered", 500);
        let first = store.rows(&page("t", 20, 0, "kick", None, None)).unwrap();
        assert_eq!(first.rows.len(), 20, "the filter must leave a full page");
        let hint = hint_for(&first, 0);
        let by_cursor = store
            .rows(&page("t", 20, 20, "kick", Some(&hint), None))
            .unwrap();
        let by_offset = store.rows(&page("t", 20, 20, "kick", None, None)).unwrap();
        assert_eq!(by_cursor.rowids, by_offset.rowids);
        let _ = std::fs::remove_file(path);
    }

    /// `G` reads the last page backwards from the end of the ordering. It must
    /// hold the same rows as walking there with an offset — measured at 1 ms
    /// against 9.6 s on a 6.5M-row filtered grid, so the two are never both run.
    #[test]
    fn the_last_page_read_backwards_matches_the_offset_route() {
        let (path, store) = fixture("last", 250);
        let total = store.count_exact("t", "").unwrap();
        assert_eq!(total, 250);
        // 250 rows in pages of 60 leaves a short final page, the case that gets
        // the LIMIT wrong if `take` ignores the total.
        let offset = ((total - 1) / 60) * 60;
        let backwards = store
            .rows(&page("t", 60, offset, "", None, Some(total)))
            .unwrap();
        let forwards = store.rows(&page("t", 60, offset, "", None, None)).unwrap();
        assert_eq!(backwards.rowids, forwards.rowids, "last page differs");
        assert_eq!(backwards.rows.len(), 10, "250 rows in 60s leaves 10");
        assert!(backwards.total_exact);
        let _ = std::fs::remove_file(path);
    }

    /// A page reports what it can prove: a bound while more rows follow, the real
    /// total once it has reached the end. Nothing here is allowed to cost a scan.
    #[test]
    fn a_page_reports_an_exact_total_only_when_it_has_seen_the_end() {
        let (path, store) = fixture("totals", 30);
        let mid = store.rows(&page("t", 10, 0, "", None, None)).unwrap();
        assert!(!mid.total_exact, "rows follow, so the total is a bound");
        assert_eq!(
            mid.total, 11,
            "the bound is what the extra probe row proves"
        );

        let end = store.rows(&page("t", 10, 20, "", None, None)).unwrap();
        assert!(end.total_exact, "the page ended the table");
        assert_eq!(end.total, 30);

        // A counted total always wins over the bound.
        let known = store.rows(&page("t", 10, 0, "", None, Some(30))).unwrap();
        assert_eq!((known.total, known.total_exact), (30, true));
        assert_eq!(known.rows.len(), 10, "the probe row is never displayed");
        let _ = std::fs::remove_file(path);
    }

    /// The parallel count is only useful if it agrees with the obvious one. The
    /// table is wide enough in rowid span to be split, so this exercises the
    /// range fan-out rather than the single-statement fallback.
    #[test]
    fn the_parallel_count_agrees_with_one_statement() {
        let (path, store) = fixture("count", 2_000);
        // Force the split: the fixture's span is far under the floor.
        store
            .conn
            .execute("UPDATE t SET rowid = rowid + 5000000 WHERE n = 1999", [])
            .unwrap();
        store.cache.ranges.borrow_mut().clear();
        assert!(
            store.rowid_ranges("t").unwrap().is_some(),
            "a span this wide must be split"
        );
        for filter in ["", "kick", "name:word", "kick nothing"] {
            assert_eq!(
                store.count_exact("t", filter).unwrap(),
                store.count_filtered("t", filter).unwrap(),
                "parallel and single-statement counts disagree for {filter:?}"
            );
        }
        let _ = std::fs::remove_file(path);
    }

    /// Every row must be counted exactly once, whatever the rowid gaps — an
    /// off-by-one in the half-open ranges would double-count a boundary row.
    #[test]
    fn rowid_ranges_cover_every_row_exactly_once() {
        let (path, store) = fixture("ranges", 400);
        store
            .conn
            .execute("UPDATE t SET rowid = rowid * 100000", [])
            .unwrap();
        store.cache.ranges.borrow_mut().clear();
        let ranges = store.rowid_ranges("t").unwrap().expect("wide span splits");
        let mut seen = 0i64;
        for &(lo, hi) in ranges.iter() {
            assert!(lo < hi, "range ({lo}, {hi}] is empty or inverted");
            seen += store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM t WHERE rowid > ?1 AND rowid <= ?2",
                    params![lo, hi],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap();
        }
        assert_eq!(seen, 400, "ranges must partition the table");
        for pair in ranges.windows(2) {
            assert_eq!(
                pair[0].1, pair[1].0,
                "ranges must not overlap or leave gaps"
            );
        }
        let _ = std::fs::remove_file(path);
    }

    /// A small table is counted by one statement: splitting it would cost more
    /// than it saves.
    #[test]
    fn a_narrow_table_is_not_split() {
        let (path, store) = fixture("small", 100);
        assert!(store.rowid_ranges("t").unwrap().is_none());
        assert_eq!(store.count_exact("t", "kick").unwrap(), 20);
        let _ = std::fs::remove_file(path);
    }

    /// A table whose virtual-table module is missing cannot be read by anyone, so
    /// it is reported rather than left to fail when selected. This is Ableton's
    /// `search_aggregation`, an FTS4 index built with a custom tokenizer.
    #[test]
    fn a_table_needing_a_missing_module_is_reported_unreadable() {
        let (path, store) = fixture("module", 10);
        drop(store);
        // Writing the schema row directly is how such a table comes to exist for
        // a binary that cannot create it: the module was present when it was made.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA writable_schema = ON;
             INSERT INTO sqlite_master (type, name, tbl_name, rootpage, sql)
             VALUES ('table', 'weird', 'weird', 0,
                     'CREATE VIRTUAL TABLE weird USING no_such_module(a)');
             PRAGMA writable_schema = OFF;",
        )
        .unwrap();
        drop(conn);
        let store = SqliteStore::open(&path).unwrap();
        assert!(
            store.tables.iter().any(|t| t == "weird"),
            "the table is listed: {:?}",
            store.tables
        );
        let why = store
            .unreadable_reason("weird")
            .expect("a missing module must be reported");
        assert!(why.contains("no such module"), "unexpected reason: {why}");
        assert!(
            store.unreadable_reason("t").is_none(),
            "an ordinary table is readable"
        );
        let _ = std::fs::remove_file(path);
    }

    /// A search's ordinal decides which page it scrolls to, so the parallel path
    /// has to agree with the plain count for both orderings.
    #[test]
    fn the_search_ordinal_is_the_rows_position_in_the_display_order() {
        let (path, store) = fixture("ordinal", 2_000);
        store
            .conn
            .execute("UPDATE t SET rowid = rowid + 5000000 WHERE n >= 1000", [])
            .unwrap();
        store.cache.ranges.borrow_mut().clear();
        let view = store.rows(&page("t", 5, 0, "kick", None, None)).unwrap();
        let third = view.rowids[2].unwrap();
        assert_eq!(
            store.rowid_ordinal("t", third, None, "kick").unwrap(),
            3,
            "the third listed match is at position 3"
        );
        let sort = Sort {
            column: "n".into(),
            desc: true,
        };
        let desc = store
            .rows(&PageQuery {
                sort: Some(&sort),
                ..page("t", 5, 0, "kick", None, None)
            })
            .unwrap();
        assert_eq!(
            store
                .rowid_ordinal("t", desc.rowids[0].unwrap(), Some(&sort), "kick")
                .unwrap(),
            1,
            "the first row of a descending sort is at position 1"
        );
        let _ = std::fs::remove_file(path);
    }

    /// The schema cache must not outlive the schema, or an edit addresses a column
    /// that has moved.
    #[test]
    fn invalidate_forgets_a_stale_column_list() {
        let (path, mut store) = fixture("schema", 10);
        assert_eq!(store.columns("t").unwrap(), vec!["name", "n"]);
        store
            .conn
            .execute("ALTER TABLE t ADD COLUMN extra TEXT", [])
            .unwrap();
        assert_eq!(
            store.columns("t").unwrap(),
            vec!["name", "n"],
            "the cache is what makes a page cheap, so it holds until told otherwise"
        );
        store.invalidate();
        assert_eq!(store.columns("t").unwrap(), vec!["name", "n", "extra"]);
        let _ = std::fs::remove_file(path);
    }

    /// A table with no rowid keys its rows by primary key instead, and an edit
    /// has to reach exactly the row it was aimed at even when every other column
    /// looks the same.
    #[test]
    fn an_edit_on_a_rowid_less_table_hits_one_row() {
        let mut path = std::env::temp_dir();
        path.push(format!("zdbview_sqlite_{}_norowid.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE k (id TEXT PRIMARY KEY, note TEXT) WITHOUT ROWID;
             INSERT INTO k VALUES ('a', 'same'), ('b', 'same'), ('c', 'same');",
        )
        .unwrap();
        drop(conn);
        let store = SqliteStore::open(&path).unwrap();

        let view = store.rows(&page("k", 10, 0, "", None, None)).unwrap();
        assert_eq!(view.primary_key, vec!["id"], "the key is the handle here");
        assert!(
            view.rowids.iter().all(Option::is_none),
            "and there is no rowid to fall back on"
        );

        let key = RowKey::Primary(vec![("id".into(), "b".into())]);
        assert_eq!(
            store
                .update_cell_keyed("k", &key, "note", "edited")
                .unwrap(),
            1
        );
        store.write_changes().unwrap();

        let after = store.rows(&page("k", 10, 0, "", None, None)).unwrap();
        assert_eq!(
            after.rows,
            vec![
                vec!["a".to_string(), "same".to_string()],
                vec!["b".to_string(), "edited".to_string()],
                vec!["c".to_string(), "same".to_string()],
            ],
            "one row changed, the identical-looking ones untouched"
        );

        assert_eq!(store.delete_row_keyed("k", &key).unwrap(), 1);
        store.write_changes().unwrap();
        assert_eq!(store.count("k").unwrap(), 2);
        let _ = std::fs::remove_file(&path);
    }

    /// Nothing an edit does reaches the file until it is written, and reverting
    /// puts the database back where it was — the Write/Revert model the grid is
    /// built on.
    #[test]
    fn unwritten_edits_are_invisible_to_another_connection_and_revert_cleanly() {
        let (path, mut store) = fixture("pending", 20);
        assert!(!store.has_pending(), "a fresh store owes nothing");

        let key = RowKey::Rowid(1);
        store
            .update_cell_keyed("t", &key, "name", "changed")
            .unwrap();
        assert!(store.has_pending(), "the edit is held, not written");

        // A second connection sees the database as it was.
        let other = Connection::open(&path).unwrap();
        let seen: String = other
            .query_row("SELECT name FROM t WHERE rowid = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            seen, "kick-0",
            "unwritten edits stay on their own connection"
        );

        assert!(
            store.revert_changes().unwrap(),
            "there was something to revert"
        );
        assert!(!store.has_pending());
        assert!(
            !store.revert_changes().unwrap(),
            "and nothing left after that"
        );
        let restored: String = other
            .query_row("SELECT name FROM t WHERE rowid = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(restored, "kick-0");

        // Write, and the other connection sees it.
        store
            .update_cell_keyed("t", &key, "name", "written")
            .unwrap();
        assert!(store.write_changes().unwrap());
        assert!(!store.write_changes().unwrap(), "nothing to write twice");
        let seen: String = other
            .query_row("SELECT name FROM t WHERE rowid = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seen, "written");
        drop(other);
        let _ = std::fs::remove_file(&path);
    }

    /// Bytes are not text: a blob has to survive the round trip through the hex
    /// editor without being decoded, re-encoded, or truncated at its first NUL.
    #[test]
    fn a_blob_round_trips_through_the_keyed_edit() {
        let (path, store) = fixture("blobs", 5);
        let key = RowKey::Rowid(1);
        // Every byte value, so nothing about it is valid UTF-8 or NUL-free.
        let bytes: Vec<u8> = (0..=255u8).collect();
        assert_eq!(
            store
                .update_cell_blob_keyed("t", &key, "name", &bytes)
                .unwrap(),
            1
        );
        store.write_changes().unwrap();

        assert!(store.cell_is_blob_keyed("t", &key, "name").unwrap());
        assert_eq!(store.cell_bytes_keyed("t", &key, "name").unwrap(), bytes);

        // The grid cannot show raw bytes, so it says what they are rather than
        // pretending they are text.
        let view = store.rows(&page("t", 1, 0, "", None, None)).unwrap();
        assert_eq!(view.rows[0][0], "<blob 256 bytes>");
        // The export path cannot do that: a dump has to carry the bytes, so
        // there the same cell is the SQL literal for them.
        let literal = &store.literal_rows("t").unwrap()[0][0];
        assert!(literal.starts_with("x'000102"), "{literal}");
        assert!(literal.ends_with("fdfeff'"), "{literal}");

        // Text in the same column is not a blob, and reads back as its bytes.
        store.update_cell_keyed("t", &key, "name", "plain").unwrap();
        store.write_changes().unwrap();
        assert!(!store.cell_is_blob_keyed("t", &key, "name").unwrap());
        assert_eq!(store.cell_bytes_keyed("t", &key, "name").unwrap(), b"plain");
        let _ = std::fs::remove_file(&path);
    }

    /// NULL is not the empty string, and the two must not be confused by a path
    /// that binds every value as text.
    #[test]
    fn setting_a_cell_to_null_is_not_setting_it_to_empty() {
        let (path, store) = fixture("nulls", 5);
        let key = RowKey::Rowid(1);
        store.update_cell_keyed("t", &key, "name", "").unwrap();
        store.write_changes().unwrap();
        let is_null: bool = store
            .conn
            .query_row("SELECT name IS NULL FROM t WHERE rowid = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!is_null, "an empty string is a value");

        store.update_cell_null("t", &key, "name").unwrap();
        store.write_changes().unwrap();
        let is_null: bool = store
            .conn
            .query_row("SELECT name IS NULL FROM t WHERE rowid = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(is_null, "and NULL is the absence of one");
        let _ = std::fs::remove_file(&path);
    }

    /// A quote or a newline in the data is data, not syntax — the whole reason
    /// values bind as parameters.
    #[test]
    fn punctuation_in_a_value_is_data_not_syntax() {
        let (path, store) = fixture("quoting", 5);
        let nasty = "o'brien\n\"quoted\"; DROP TABLE t; --";
        let key = RowKey::Rowid(1);
        assert_eq!(
            store.update_cell_keyed("t", &key, "name", nasty).unwrap(),
            1
        );
        store.write_changes().unwrap();
        assert_eq!(store.count("t").unwrap(), 5, "the table is still there");
        let back: String = store
            .conn
            .query_row("SELECT name FROM t WHERE rowid = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(back, nasty, "stored exactly as typed");
        let _ = std::fs::remove_file(&path);
    }

    /// The outer transaction a dump writes is dropped so the script can run
    /// inside the edit savepoint — and nothing else that says BEGIN is touched.
    #[test]
    fn only_the_scripts_own_outer_transaction_is_stripped() {
        let dump = "PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\nCREATE TABLE t(a);\nCOMMIT;\n";
        let stripped = strip_outer_transaction(dump);
        assert!(stripped.contains("PRAGMA foreign_keys=OFF;"), "{stripped}");
        assert!(stripped.contains("CREATE TABLE t(a);"), "{stripped}");
        assert!(!stripped.to_uppercase().contains("BEGIN"), "{stripped}");
        assert!(!stripped.to_uppercase().contains("COMMIT"), "{stripped}");

        // A trigger body is BEGIN … END and is the script's own content — the
        // END that closes it is not the end of a transaction.
        let trigger = "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a = 1; END;";
        assert_eq!(strip_outer_transaction(trigger), trigger);
        // Including when a dump wraps it, where both are present at once.
        let wrapped = format!("BEGIN TRANSACTION;\n{trigger}\nCOMMIT;");
        let stripped = strip_outer_transaction(&wrapped);
        assert!(
            stripped.contains(trigger),
            "the trigger survives whole: {stripped}"
        );
        assert!(!stripped.contains("BEGIN TRANSACTION"), "{stripped}");
        assert!(!stripped.contains("COMMIT;"), "{stripped}");

        // A script with no transaction of its own comes back as it went in.
        let plain = "INSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);";
        assert_eq!(strip_outer_transaction(plain), plain);

        // The words inside a value are values, not statements.
        let literal = "INSERT INTO t VALUES ('begin transaction; commit;');";
        assert_eq!(strip_outer_transaction(literal), literal);

        // Comments ahead of the BEGIN do not hide it.
        let commented =
            "-- made by zdbview\n/* two kinds */\nBEGIN;\nINSERT INTO t VALUES (1);\nCOMMIT;";
        let stripped = strip_outer_transaction(commented);
        assert!(stripped.contains("-- made by zdbview"), "{stripped}");
        assert!(stripped.contains("INSERT INTO t VALUES (1);"), "{stripped}");
        assert!(!stripped.contains("BEGIN;"), "{stripped}");
        assert!(!stripped.contains("COMMIT;"), "{stripped}");
    }

    /// The whole point of stripping it: a dump replays through the same call the
    /// SQL-file import uses, and a broken statement still takes nothing with it.
    #[test]
    fn a_dump_imports_and_a_failed_script_leaves_nothing_behind() {
        let (path, store) = fixture("import_sql", 3);
        let dump = store.dump(Some("t")).unwrap();
        assert!(
            dump.contains("BEGIN TRANSACTION"),
            "the dump has one to strip"
        );

        let mut dest = std::env::temp_dir();
        dest.push(format!(
            "zdbview_sqlite_{}_import_dest.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&dest);
        let fresh = SqliteStore::create(&dest).unwrap();
        assert_eq!(fresh.import_sql(&dump).unwrap(), 3, "three rows replayed");
        fresh.write_changes().unwrap();
        assert_eq!(fresh.count("t").unwrap(), 3);

        // A script that fails part way is rolled back whole.
        let err = fresh.import_sql("INSERT INTO t (name, n) VALUES ('x', 1); NOT SQL;");
        assert!(err.is_err(), "the bad statement is reported");
        fresh.write_changes().unwrap();
        assert_eq!(
            fresh.count("t").unwrap(),
            3,
            "and the good one did not stick"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&dest);
    }
}
