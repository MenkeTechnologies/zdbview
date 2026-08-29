//! Salvage rows straight out of a database file, the way the sqlite3 shell's
//! `.recover` does — by reading pages rather than by asking SQLite.
//!
//! This exists for the case where SQLite itself refuses: a corrupt b-tree root, a
//! truncated file, a bad page. It never opens a connection to the file, so nothing
//! it does can be refused by an integrity check, and it reads *every* page rather
//! than only the ones reachable from `sqlite_master`, which is what brings back
//! rows on freed pages.
//!
//! Format reference: <https://sqlite.org/fileformat2.html>. The parts used here:
//!
//! * The 100-byte database header — page size at offset 16 (a `1` means 65536),
//!   reserved per-page bytes at 20, page count at 28, text encoding at 56.
//! * A b-tree page header: type byte (`0x0d` table leaf, `0x05` table interior),
//!   cell count at 3, and for interior pages a right-most child pointer at 8.
//! * A table-leaf cell: payload length, rowid, then the record — and a 4-byte
//!   overflow page number when the payload does not fit on the page.
//! * A record: a header of serial types, then the values in the same order.
//!
//! What it does not do: recover indexes (they are rebuilt by replaying the
//! `CREATE INDEX` statements), decode WITHOUT ROWID index-leaf pages, or repair
//! the file in place. Rows on pages it cannot attribute to a table go to a
//! `lost_and_found` table, as the shell's do.

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::Path;

/// One decoded cell value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    /// As a SQL literal, which is the only form a recovery script can use.
    pub fn literal(&self) -> String {
        match self {
            Value::Null => "NULL".into(),
            Value::Int(i) => i.to_string(),
            Value::Real(f) => {
                let s = format!("{f:?}");
                if s.contains(['.', 'e', 'E', 'n']) {
                    s
                } else {
                    format!("{s}.0")
                }
            }
            Value::Text(t) => format!("'{}'", t.replace('\'', "''")),
            Value::Blob(b) => {
                let mut out = String::with_capacity(b.len() * 2 + 3);
                out.push_str("X'");
                for byte in b {
                    out.push_str(&format!("{byte:02X}"));
                }
                out.push('\'');
                out
            }
        }
    }
}

/// A row as it came off a page.
#[derive(Debug, Clone)]
pub struct Row {
    /// The table it belongs to, or `None` when the page could not be attributed.
    pub table: Option<String>,
    /// The page it was read from, which is what makes a recovery auditable.
    pub page: u32,
    /// `None` for a row off an index-leaf page — a `WITHOUT ROWID` table keeps its
    /// rows in an index b-tree, where the primary key is the handle and there is no
    /// rowid to record.
    pub rowid: Option<i64>,
    pub values: Vec<Value>,
}

/// A table as `sqlite_master` describes it.
#[derive(Debug, Clone)]
pub struct TableDef {
    pub name: String,
    pub rootpage: u32,
    /// Column names parsed out of the `CREATE` statement, for the `INSERT` list.
    pub columns: Vec<String>,
}

/// Everything a recovery pass found.
#[derive(Debug, Default)]
pub struct Recovered {
    /// `CREATE` statements for every object the schema page still holds.
    pub schema: Vec<(String, String)>,
    pub tables: Vec<TableDef>,
    pub rows: Vec<Row>,
    /// Pages that held rows but could not be attributed to one table.
    pub orphan_pages: Vec<u32>,
    /// What the pass had to work around, reported rather than hidden.
    pub notes: Vec<String>,
}

impl Recovered {
    pub fn rows_for<'a>(&'a self, table: &'a str) -> impl Iterator<Item = &'a Row> {
        self.rows
            .iter()
            .filter(move |r| r.table.as_deref() == Some(table))
    }

    /// How many rows came back with no table.
    pub fn orphans(&self) -> usize {
        self.rows.iter().filter(|r| r.table.is_none()).count()
    }
}

/// A page-level view of a database file.
struct Db {
    bytes: Vec<u8>,
    page_size: usize,
    /// Page size less the reserved trailer, which is what payload sizing uses.
    usable: usize,
    pages: u32,
}

impl Db {
    /// The file as it stands, with its write-ahead log applied on top.
    ///
    /// In WAL mode the database file holds the pre-write version of every page the
    /// log has since rewritten, and a table created but not yet checkpointed is not
    /// in the file's schema at all. Reading the file alone therefore reads the past,
    /// which is wrong for both a page map and a recovery.
    fn open_with_wal(path: &Path) -> Result<Self> {
        let mut db = Db::open(path)?;
        let Some((page_size, pages, db_size)) = crate::wal::latest_pages(path) else {
            return Ok(db);
        };
        if page_size as usize != db.page_size {
            // A log for a different page size cannot belong to this file.
            return Ok(db);
        }
        let needed =
            (db_size.max(pages.keys().copied().max().unwrap_or(0)) as usize) * db.page_size;
        if needed > db.bytes.len() {
            db.bytes.resize(needed, 0);
        }
        for (page, image) in pages {
            let start = (page as usize - 1) * db.page_size;
            if start + db.page_size <= db.bytes.len() {
                db.bytes[start..start + db.page_size].copy_from_slice(&image);
            }
        }
        db.pages = (db.bytes.len() / db.page_size) as u32;
        Ok(db)
    }

    fn open(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        if bytes.len() < 100 {
            return Err(anyhow!("too short to be a database (no header)"));
        }
        if &bytes[..16] != b"SQLite format 3\0" {
            return Err(anyhow!("not a SQLite database (header magic)"));
        }
        let raw = be16(&bytes, 16) as usize;
        // The header stores 65536 as 1, since it does not fit in two bytes.
        let page_size = if raw == 1 { 65536 } else { raw };
        if page_size < 512 || !page_size.is_power_of_two() {
            return Err(anyhow!("page size {page_size} is not a power of two ≥ 512"));
        }
        let reserved = bytes[20] as usize;
        let counted = be32(&bytes, 28);
        // A truncated file's header still claims the original count, so trust the
        // file's actual length instead — recovering what is there is the point.
        let present = (bytes.len() / page_size) as u32;
        Ok(Db {
            page_size,
            usable: page_size - reserved,
            pages: present.max(1).min(counted.max(present)),
            bytes,
        })
    }

    /// One page, 1-based as SQLite numbers them.
    fn page(&self, n: u32) -> Option<&[u8]> {
        if n == 0 {
            return None;
        }
        let start = (n as usize - 1) * self.page_size;
        self.bytes
            .get(start..(start + self.page_size).min(self.bytes.len()))
    }

    /// Every row a page holds. Three page kinds carry them:
    ///
    /// * a **table leaf** (`0x0d`), whose cells are `(payload, rowid)`;
    /// * an **index leaf** (`0x0a`), whose cells are the key record alone — which
    ///   for a `WITHOUT ROWID` table is the whole row;
    /// * an **index interior** (`0x02`), which unlike a table interior carries key
    ///   payloads of its own after each child pointer, so a `WITHOUT ROWID` table
    ///   keeps some of its rows there.
    ///
    /// `None` for anything else, including a table interior — that holds only child
    /// pointers and rowids.
    fn leaf_cells(&self, n: u32) -> Option<Vec<(Option<i64>, Vec<Value>)>> {
        let page = self.page(n)?;
        // Page 1 carries the database header before its b-tree header.
        let base = if n == 1 { 100 } else { 0 };
        // (payload carries a rowid, header length, bytes before the payload)
        let (with_rowid, header, skip) = match *page.get(base)? {
            0x0d => (true, 8, 0),
            0x0a => (false, 8, 0),
            0x02 => (false, 12, 4),
            _ => return None,
        };
        let ncells = be16(page, base + 3) as usize;
        let mut out = Vec::with_capacity(ncells);
        for i in 0..ncells {
            let ptr = base + header + i * 2;
            let off = be16(page, ptr) as usize;
            if off == 0 || off >= page.len() {
                continue;
            }
            if let Some(cell) = self.leaf_cell(page, off + skip, with_rowid) {
                out.push(cell);
            }
        }
        Some(out)
    }

    /// One leaf cell: payload length, a rowid on a table leaf, then the record —
    /// following the overflow chain when the payload does not fit on the page.
    fn leaf_cell(
        &self,
        page: &[u8],
        off: usize,
        with_rowid: bool,
    ) -> Option<(Option<i64>, Vec<Value>)> {
        let (payload_len, n1) = varint(page, off)?;
        let (rowid, n2) = if with_rowid {
            let (r, n) = varint(page, off + n1)?;
            (Some(r), n)
        } else {
            (None, 0)
        };
        let head = off + n1 + n2;
        let payload_len = payload_len as usize;

        // How much of the payload lives on this page (fileformat2.html §1.6). A
        // table leaf keeps far more of it locally than an index leaf does, so using
        // the table formula on an index cell reads the wrong number of bytes and
        // decodes a short record.
        let max_local = if with_rowid {
            self.usable - 35
        } else {
            ((self.usable - 12) * 64 / 255) - 23
        };
        let min_local = ((self.usable - 12) * 32 / 255) - 23;
        let local = if payload_len <= max_local {
            payload_len
        } else {
            let candidate = min_local + (payload_len - min_local) % (self.usable - 4);
            if candidate > max_local {
                min_local
            } else {
                candidate
            }
        };
        let mut payload = page.get(head..head + local)?.to_vec();
        if local < payload_len {
            let mut next = be32(page, head + local);
            // Bounded by the file's own page count, so a corrupt chain cannot loop.
            let mut guard = self.pages as usize + 1;
            while next != 0 && payload.len() < payload_len && guard > 0 {
                guard -= 1;
                let ov = self.page(next)?;
                let take = (payload_len - payload.len()).min(self.usable - 4);
                payload.extend_from_slice(ov.get(4..4 + take)?);
                next = be32(ov, 0);
            }
        }
        Some((rowid, decode_record(&payload)))
    }

    /// `(type, name, rootpage)` for every object the schema table holds, read from
    /// the b-tree rooted at page 1 rather than from a query.
    fn schema_objects(&self) -> Vec<(String, String, u32)> {
        let mut out = Vec::new();
        for page in self.walk(1) {
            for (_, values) in self.leaf_cells(page).unwrap_or_default() {
                let text = |i: usize| match values.get(i) {
                    Some(Value::Text(t)) => Some(t.clone()),
                    _ => None,
                };
                let rootpage = match values.get(3) {
                    Some(Value::Int(i)) => *i as u32,
                    _ => 0,
                };
                if let (Some(kind), Some(name)) = (text(0), text(1)) {
                    out.push((kind, name, rootpage));
                }
            }
        }
        out
    }

    /// Every page reachable from `root` as a table b-tree, leaves included.
    fn walk(&self, root: u32) -> Vec<u32> {
        let mut seen = Vec::new();
        let mut stack = vec![root];
        let mut guard = self.pages as usize * 2 + 8;
        while let Some(n) = stack.pop() {
            if guard == 0 {
                break;
            }
            guard -= 1;
            if n == 0 || n > self.pages || seen.contains(&n) {
                continue;
            }
            seen.push(n);
            let page = match self.page(n) {
                Some(p) => p,
                None => continue,
            };
            let base = if n == 1 { 100 } else { 0 };
            // `0x05` is a table interior, `0x02` an index interior — and a
            // `WITHOUT ROWID` table's rows live in an index b-tree, so both have to
            // be walked or its pages are never reached.
            if !matches!(page.get(base).copied(), Some(0x05) | Some(0x02)) {
                continue; // a leaf, or not a b-tree page at all
            }
            let ncells = be16(page, base + 3) as usize;
            for i in 0..ncells {
                let off = be16(page, base + 12 + i * 2) as usize;
                if off + 4 <= page.len() {
                    stack.push(be32(page, off));
                }
            }
            // The right-most child sits in the interior page's own header.
            stack.push(be32(page, base + 8));
        }
        seen
    }
}

/// The rows a single page image holds, for a caller that has one page and no file
/// — reading a WAL frame's payload, which is a page as of one write.
#[derive(Debug, Default)]
pub struct PageRows {
    /// `(rowid, values)` — no rowid for a row off an index leaf, where the key is
    /// the handle.
    pub rows: Vec<(Option<i64>, Vec<Value>)>,
    /// A cell's payload continued onto an overflow page. Those pages are separate
    /// frames, so the value here stops where the page does and is reported rather
    /// than silently truncated.
    pub overflowed: bool,
    /// What kind of page it is, for a caller to explain an empty result.
    pub kind: PageKind,
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum PageKind {
    TableLeaf,
    TableInterior,
    IndexLeaf,
    IndexInterior,
    /// An overflow page, a freelist page, or anything else with no cell array.
    #[default]
    Other,
}

impl PageKind {
    pub fn label(self) -> &'static str {
        match self {
            PageKind::TableLeaf => "table leaf",
            PageKind::TableInterior => "table interior",
            PageKind::IndexLeaf => "index leaf",
            PageKind::IndexInterior => "index interior",
            PageKind::Other => "overflow / freelist",
        }
    }
}

/// Decode one page image. `page_no` matters only because page 1 carries the
/// 100-byte file header before its b-tree header.
pub fn decode_page_image(image: &[u8], page_no: u32) -> PageRows {
    let base = if page_no == 1 { 100 } else { 0 };
    let kind = match image.get(base) {
        Some(0x0d) => PageKind::TableLeaf,
        Some(0x05) => PageKind::TableInterior,
        Some(0x0a) => PageKind::IndexLeaf,
        Some(0x02) => PageKind::IndexInterior,
        _ => PageKind::Other,
    };
    let mut out = PageRows {
        kind,
        ..Default::default()
    };
    // Three kinds hold rows, and they differ in what precedes the payload — see
    // `Db::leaf_cells`, which reads them from a whole file.
    let (with_rowid, header, skip) = match kind {
        PageKind::TableLeaf => (true, 8, 0),
        PageKind::IndexLeaf => (false, 8, 0),
        PageKind::IndexInterior => (false, 12, 4),
        _ => return out,
    };
    // Payload sizing uses the usable size; a page image carries no reserved-bytes
    // field, so the whole page is assumed usable — which is true unless the
    // database was built with a reserved trailer.
    let usable = image.len();
    let max_local = if with_rowid {
        usable.saturating_sub(35)
    } else {
        // An index cell keeps much less of its payload locally than a table cell.
        ((usable.saturating_sub(12)) * 64 / 255).saturating_sub(23)
    };
    let min_local = ((usable.saturating_sub(12)) * 32 / 255).saturating_sub(23);
    let ncells = be16(image, base + 3) as usize;
    for i in 0..ncells {
        let off = be16(image, base + header + i * 2) as usize + skip;
        if off == 0 || off >= image.len() {
            continue;
        }
        let Some((payload_len, n1)) = varint(image, off) else {
            continue;
        };
        let (rowid, n2) = if with_rowid {
            match varint(image, off + n1) {
                Some((r, n)) => (Some(r), n),
                None => continue,
            }
        } else {
            (None, 0)
        };
        let head = off + n1 + n2;
        let payload_len = payload_len as usize;
        let local = if payload_len <= max_local {
            payload_len
        } else {
            out.overflowed = true;
            let candidate = min_local + (payload_len - min_local) % usable.saturating_sub(4);
            if candidate > max_local {
                min_local
            } else {
                candidate
            }
        };
        let Some(payload) = image.get(head..(head + local).min(image.len())) else {
            continue;
        };
        out.rows.push((rowid, decode_record(payload)));
    }
    out
}

/// Which table owns each page of the database, read straight from the file: every
/// table's b-tree walked from its root. Indexes are named too, prefixed so a
/// caller can tell them apart from the tables they serve.
///
/// This is what lets a write be attributed to a table without asking SQLite: a WAL
/// frame carries a page number, and this says whose page that is. It is a snapshot
/// — pages move as a database grows, so a caller watching writes has to refresh it.
pub fn page_owners(path: &Path) -> Result<HashMap<u32, String>> {
    let db = Db::open_with_wal(path)?;
    let mut out = HashMap::new();
    // Page 1 is the schema itself, whatever else it holds.
    out.insert(1, "sqlite_schema".to_string());
    for (kind, name, rootpage) in db.schema_objects() {
        if rootpage == 0 {
            continue;
        }
        let label = if kind == "index" {
            format!("index {name}")
        } else {
            name
        };
        for page in db.walk(rootpage) {
            // The first claim wins: a page cannot belong to two b-trees, and if a
            // corrupt root says otherwise the earlier object is the better guess.
            out.entry(page).or_insert_with(|| label.clone());
        }
    }
    Ok(out)
}

/// Recover what a file still holds. Never opens the database, so a file SQLite
/// refuses is still readable here.
pub fn recover(path: &Path) -> Result<Recovered> {
    let db = Db::open_with_wal(path)?;
    let mut out = Recovered::default();
    if db.bytes.len() % db.page_size != 0 {
        out.notes.push(format!(
            "file is {} bytes, not a whole number of {}-byte pages — the last page is partial",
            db.bytes.len(),
            db.page_size
        ));
    }

    // The schema lives in the table rooted at page 1: (type, name, tbl_name,
    // rootpage, sql).
    let mut schema_rows = Vec::new();
    for page in db.walk(1) {
        if let Some(cells) = db.leaf_cells(page) {
            schema_rows.extend(cells);
        }
    }
    if schema_rows.is_empty() {
        out.notes.push(
            "the schema page holds no readable rows — every row will be a lost_and_found row"
                .into(),
        );
    }
    for (_, values) in &schema_rows {
        let text = |i: usize| match values.get(i) {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        };
        let kind = text(0).unwrap_or_default();
        let name = text(1).unwrap_or_default();
        let sql = text(4).unwrap_or_default();
        let rootpage = match values.get(3) {
            Some(Value::Int(i)) => *i as u32,
            _ => 0,
        };
        if sql.is_empty() || name.starts_with("sqlite_") {
            continue;
        }
        out.schema.push((kind.clone(), sql.clone()));
        if kind == "table" {
            out.tables.push(TableDef {
                columns: create_columns(&sql),
                name,
                rootpage,
            });
        }
    }

    // Which table each page belongs to, from the b-trees that are still walkable,
    // and which pages belong to an index instead — an index leaf holds key entries,
    // not rows, so those pages must not be read as data.
    let mut owner: HashMap<u32, String> = HashMap::new();
    let mut index_pages: std::collections::HashSet<u32> = Default::default();
    for (kind, name, rootpage) in db.schema_objects() {
        if kind != "index" || rootpage == 0 {
            continue;
        }
        let _ = name;
        for p in db.walk(rootpage) {
            index_pages.insert(p);
        }
    }
    for t in &out.tables {
        if t.rootpage == 0 {
            out.notes.push(format!(
                "{}: no root page recorded (a virtual table?)",
                t.name
            ));
            continue;
        }
        let pages = db.walk(t.rootpage);
        if pages.len() == 1 && db.leaf_cells(t.rootpage).is_none() {
            out.notes.push(format!(
                "{}: root page {} is not a readable table page, so its rows are recovered from \
                 unreachable pages instead",
                t.name, t.rootpage
            ));
        }
        for p in pages {
            owner.insert(p, t.name.clone());
        }
    }

    // Every page, not just the reachable ones: that is what brings back rows on
    // pages a corrupt root no longer points at.
    let mut attributed: HashMap<String, usize> = HashMap::new();
    for n in 1..=db.pages {
        let cells = match db.leaf_cells(n) {
            Some(c) if !c.is_empty() => c,
            _ => continue,
        };
        if n == 1 {
            continue; // the schema itself, already read
        }
        // An index's own pages carry `(key…, rowid)` entries that duplicate columns
        // of a table already being read. Recovering them as rows would invent rows
        // that never existed; the index is rebuilt by replaying its CREATE instead.
        if index_pages.contains(&n) && !owner.contains_key(&n) {
            continue;
        }
        let table = match owner.get(&n) {
            Some(t) => Some(t.clone()),
            None => {
                // Unattributed: accept it only when exactly one table has that
                // many columns, otherwise it is a guess and the row goes to
                // lost_and_found.
                let ncols = cells[0].1.len();
                let mut matches = out
                    .tables
                    .iter()
                    .filter(|t| t.columns.len() == ncols)
                    .map(|t| t.name.clone());
                let first = matches.next();
                match (first, matches.next()) {
                    (Some(name), None) => {
                        // Counted rather than reported per page: a corrupt root can
                        // orphan hundreds of pages, and one line each buries every
                        // other note in the script.
                        *attributed.entry(name.clone()).or_insert(0usize) += 1;
                        Some(name)
                    }
                    _ => {
                        out.orphan_pages.push(n);
                        None
                    }
                }
            }
        };
        for (rowid, values) in cells {
            out.rows.push(Row {
                table: table.clone(),
                page: n,
                rowid,
                values,
            });
        }
    }
    let mut counts: Vec<(String, usize)> = attributed.into_iter().collect();
    counts.sort();
    for (name, pages) in counts {
        out.notes.push(format!(
            "{pages} page{} unreachable from any root matched {name} by column count alone",
            if pages == 1 { "" } else { "s" }
        ));
    }
    if !out.orphan_pages.is_empty() {
        out.notes.push(format!(
            "{} page{} could not be attributed to a table; their rows are in lost_and_found",
            out.orphan_pages.len(),
            if out.orphan_pages.len() == 1 { "" } else { "s" }
        ));
    }
    Ok(out)
}

/// A recovery script: the schema, the rows that could be attributed, and a
/// `lost_and_found` table for the rest — the shape the shell's `.recover` writes,
/// including `INSERT OR IGNORE` so a rowid recovered twice does not stop the
/// replay.
pub fn to_sql(r: &Recovered) -> String {
    let mut out =
        String::from("BEGIN;\nPRAGMA writable_schema = on;\nPRAGMA foreign_keys = off;\n");
    for note in &r.notes {
        out.push_str(&format!("-- {note}\n"));
    }
    // Tables first, then everything else, so the inserts have somewhere to go.
    for (kind, sql) in r.schema.iter().filter(|(k, _)| k == "table") {
        let _ = kind;
        out.push_str(sql.trim_end_matches(';'));
        out.push_str(";\n");
    }
    for t in &r.tables {
        let named = t
            .columns
            .iter()
            .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        for row in r.rows_for(&t.name) {
            // A row off a table leaf carries its rowid, and restoring it keeps
            // references to it valid. One off an index leaf — a `WITHOUT ROWID`
            // table — has none, and naming `_rowid_` there is an error.
            let (cols, lead) = match (row.rowid, t.columns.is_empty()) {
                (_, true) => (String::new(), String::new()),
                (Some(id), false) => (format!("(_rowid_, {named})"), format!("{id}, ")),
                (None, false) => (format!("({named})"), String::new()),
            };
            out.push_str(&format!(
                "INSERT OR IGNORE INTO \"{}\"{} VALUES ({}{});\n",
                t.name.replace('"', "\"\""),
                cols,
                lead,
                row.values
                    .iter()
                    .map(Value::literal)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let orphans: Vec<&Row> = r.rows.iter().filter(|r| r.table.is_none()).collect();
    if !orphans.is_empty() {
        let widest = orphans.iter().map(|r| r.values.len()).max().unwrap_or(0);
        let cols: Vec<String> = (0..widest).map(|i| format!("c{i}")).collect();
        out.push_str(&format!(
            "CREATE TABLE lost_and_found(pgno INTEGER, nfield INTEGER, id INTEGER{}{});\n",
            if cols.is_empty() { "" } else { ", " },
            cols.join(", ")
        ));
        for row in orphans {
            let mut values: Vec<String> = vec![
                row.page.to_string(),
                row.values.len().to_string(),
                row.rowid
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "NULL".into()),
            ];
            values.extend(row.values.iter().map(Value::literal));
            for _ in row.values.len()..widest {
                values.push("NULL".into());
            }
            out.push_str(&format!(
                "INSERT INTO lost_and_found VALUES ({});\n",
                values.join(", ")
            ));
        }
    }
    for (kind, sql) in r.schema.iter().filter(|(k, _)| k != "table") {
        let _ = kind;
        out.push_str(sql.trim_end_matches(';'));
        out.push_str(";\n");
    }
    out.push_str("PRAGMA writable_schema = off;\nCOMMIT;\n");
    out
}

/// Column names from a `CREATE TABLE` statement. Deliberately shallow: it splits
/// the top-level parenthesised list on commas and takes the first identifier of
/// each part, which is what names a column. A table constraint (`PRIMARY KEY (…)`,
/// `FOREIGN KEY …`, `UNIQUE …`, `CHECK …`) is skipped, since those are not columns.
fn create_columns(sql: &str) -> Vec<String> {
    let open = match sql.find('(') {
        Some(i) => i,
        None => return Vec::new(),
    };
    let body = &sql[open + 1..];
    let mut depth = 0usize;
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in body.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' if depth == 0 => break,
            ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    const CONSTRAINTS: &[&str] = &["primary", "unique", "check", "foreign", "constraint"];
    parts
        .iter()
        .filter_map(|p| {
            let name = first_identifier(p)?;
            if CONSTRAINTS.contains(&name.to_lowercase().as_str()) {
                return None;
            }
            Some(name)
        })
        .filter(|c| !c.is_empty())
        .collect()
}

/// The first identifier of a column definition, respecting the quoting SQLite
/// allows — `"odd name"`, `` `odd name` ``, `[odd name]` — so a quoted name is not
/// cut at its space.
fn first_identifier(part: &str) -> Option<String> {
    let text = part.trim_start();
    let mut chars = text.chars();
    let open = chars.next()?;
    let close = match open {
        '"' => '"',
        '`' => '`',
        '[' => ']',
        '\'' => '\'',
        _ => {
            let end = text
                .find(|c: char| c.is_whitespace() || c == '(')
                .unwrap_or(text.len());
            return Some(text[..end].to_string());
        }
    };
    let rest = &text[open.len_utf8()..];
    let end = rest.find(close)?;
    Some(rest[..end].to_string())
}

/// A record: a header of serial types followed by the values in the same order.
fn decode_record(payload: &[u8]) -> Vec<Value> {
    let (header_len, n) = match varint(payload, 0) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let header_end = (header_len as usize).min(payload.len());
    let mut types = Vec::new();
    let mut at = n;
    while at < header_end {
        match varint(payload, at) {
            Some((t, used)) => {
                types.push(t);
                at += used;
            }
            None => break,
        }
    }
    let mut values = Vec::with_capacity(types.len());
    let mut body = header_end;
    for t in types {
        let (value, used) = read_value(payload, body, t);
        values.push(value);
        body += used;
    }
    values
}

/// One value of serial type `t` at `at`, with how many bytes it consumed.
fn read_value(payload: &[u8], at: usize, t: i64) -> (Value, usize) {
    let int = |len: usize| -> Value {
        match payload.get(at..at + len) {
            Some(b) => {
                // Two's complement, big-endian, of the stated width.
                let mut v: i64 = if b[0] & 0x80 != 0 { -1 } else { 0 };
                for byte in b {
                    v = (v << 8) | *byte as i64;
                }
                Value::Int(v)
            }
            None => Value::Null,
        }
    };
    match t {
        0 => (Value::Null, 0),
        1 => (int(1), 1),
        2 => (int(2), 2),
        3 => (int(3), 3),
        4 => (int(4), 4),
        5 => (int(6), 6),
        6 => (int(8), 8),
        7 => match payload.get(at..at + 8) {
            Some(b) => (
                Value::Real(f64::from_be_bytes(b.try_into().unwrap_or([0; 8]))),
                8,
            ),
            None => (Value::Null, 0),
        },
        // 8 and 9 are the constants 0 and 1, stored in the type itself.
        8 => (Value::Int(0), 0),
        9 => (Value::Int(1), 0),
        n if n >= 12 && n % 2 == 0 => {
            let len = (n as usize - 12) / 2;
            match payload.get(at..at + len) {
                Some(b) => (Value::Blob(b.to_vec()), len),
                None => (Value::Null, 0),
            }
        }
        n if n >= 13 => {
            let len = (n as usize - 13) / 2;
            match payload.get(at..at + len) {
                Some(b) => (Value::Text(String::from_utf8_lossy(b).into_owned()), len),
                None => (Value::Null, 0),
            }
        }
        // 10 and 11 are reserved and never appear in a file SQLite wrote.
        _ => (Value::Null, 0),
    }
}

/// A SQLite varint: up to nine big-endian bytes, seven bits each, high bit set to
/// continue. The ninth byte contributes all eight of its bits.
fn varint(bytes: &[u8], at: usize) -> Option<(i64, usize)> {
    let mut value: u64 = 0;
    for i in 0..9 {
        let byte = *bytes.get(at + i)?;
        if i == 8 {
            value = (value << 8) | byte as u64;
            return Some((value as i64, 9));
        }
        value = (value << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return Some((value as i64, i + 1));
        }
    }
    None
}

fn be16(b: &[u8], at: usize) -> u16 {
    match b.get(at..at + 2) {
        Some(s) => u16::from_be_bytes([s[0], s[1]]),
        None => 0,
    }
}

fn be32(b: &[u8], at: usize) -> u32 {
    match b.get(at..at + 4) {
        Some(s) => u32::from_be_bytes([s[0], s[1], s[2], s[3]]),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_match_the_format_spec() {
        // One byte below 0x80, two bytes above it, and the nine-byte maximum.
        assert_eq!(varint(&[0x00], 0), Some((0, 1)));
        assert_eq!(varint(&[0x7f], 0), Some((127, 1)));
        assert_eq!(varint(&[0x81, 0x00], 0), Some((128, 2)));
        assert_eq!(varint(&[0x82, 0x21], 0), Some((289, 2)));
        assert_eq!(
            varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], 0),
            Some((-1, 9)),
            "the nine-byte form is the full 64 bits, so all-ones is -1"
        );
        assert_eq!(
            varint(&[0x81], 0),
            None,
            "a truncated varint is not a value"
        );
    }

    #[test]
    fn serial_types_decode_to_their_values() {
        // NULL, the two constants, a one-byte negative int, a real, text, a blob.
        assert_eq!(read_value(&[], 0, 0).0, Value::Null);
        assert_eq!(read_value(&[], 0, 8).0, Value::Int(0));
        assert_eq!(read_value(&[], 0, 9).0, Value::Int(1));
        assert_eq!(read_value(&[0xff], 0, 1).0, Value::Int(-1));
        assert_eq!(read_value(&[0x7f, 0xff], 0, 2).0, Value::Int(32767));
        let pi = std::f64::consts::PI.to_be_bytes();
        assert_eq!(
            read_value(&pi, 0, 7).0,
            Value::Real(std::f64::consts::PI),
            "type 7 is an IEEE double"
        );
        assert_eq!(
            read_value(b"hi", 0, 13 + 2 * 2).0,
            Value::Text("hi".into()),
            "odd types ≥ 13 are text of (N-13)/2 bytes"
        );
        assert_eq!(
            read_value(&[0x00, 0xff], 0, 12 + 2 * 2).0,
            Value::Blob(vec![0x00, 0xff]),
            "even types ≥ 12 are blobs of (N-12)/2 bytes"
        );
    }

    #[test]
    fn literals_round_trip_the_hard_cases() {
        assert_eq!(Value::Null.literal(), "NULL");
        assert_eq!(Value::Int(-5).literal(), "-5");
        assert_eq!(Value::Real(1.0).literal(), "1.0", "a real keeps its point");
        assert_eq!(Value::Text("it's".into()).literal(), "'it''s'");
        assert_eq!(Value::Blob(vec![0, 255]).literal(), "X'00FF'");
    }

    #[test]
    fn column_names_come_from_the_create_statement() {
        assert_eq!(
            create_columns("CREATE TABLE t (a TEXT, b INTEGER)"),
            ["a", "b"]
        );
        assert_eq!(
            create_columns("CREATE TABLE t (\"odd name\" TEXT, b NUMERIC(10, 2), PRIMARY KEY (b))"),
            ["odd name", "b"],
            "a table constraint is not a column, and a type's own commas do not split"
        );
        assert_eq!(
            create_columns("CREATE TABLE t (id INTEGER, FOREIGN KEY (id) REFERENCES o(id))"),
            ["id"]
        );
        assert!(create_columns("CREATE VIEW v AS SELECT 1").is_empty());
    }

    /// One page image, decoded without the file it came from — what the frame
    /// browser does with a WAL payload. Page 1 is the case that bites: its
    /// b-tree header sits 100 bytes in, behind the file header, so decoding it
    /// as any other page reads the wrong byte for the page kind.
    #[test]
    fn a_page_image_decodes_on_its_own_and_page_one_is_offset() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "zdbview_recover_{}_pageimage.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (a TEXT, b INTEGER);
             INSERT INTO t VALUES ('first', 1), ('second', 2);",
        )
        .unwrap();
        drop(conn);

        let bytes = std::fs::read(&path).unwrap();
        let page_size = u16::from_be_bytes([bytes[16], bytes[17]]) as usize;
        let page = |n: usize| &bytes[(n - 1) * page_size..n * page_size];

        // Page 1 holds the schema, and only reads as a leaf when the 100-byte
        // file header is skipped.
        let schema = super::decode_page_image(page(1), 1);
        assert_eq!(schema.kind, PageKind::TableLeaf);
        assert!(
            !schema.rows.is_empty(),
            "the schema row is a row like any other"
        );
        let misread = super::decode_page_image(page(1), 2);
        assert_ne!(
            misread.kind,
            PageKind::TableLeaf,
            "read as any other page, page 1 does not decode"
        );

        // The table's own page decodes to its rows, with rowids.
        let leaf = (2..=(bytes.len() / page_size))
            .map(|n| (n, super::decode_page_image(page(n), n as u32)))
            .find(|(_, d)| d.rows.len() == 2)
            .map(|(_, d)| d)
            .expect("a page holding both rows");
        assert_eq!(leaf.kind, PageKind::TableLeaf);
        assert_eq!(leaf.rows[0].0, Some(1), "a table leaf carries the rowid");
        assert_eq!(leaf.rows[0].1[0], Value::Text("first".into()));
        assert_eq!(leaf.rows[1].1[1], Value::Int(2));
        assert!(!leaf.overflowed, "nothing here spills onto another page");

        // Anything that is not a b-tree page is reported as such rather than
        // decoded into nonsense.
        let junk = super::decode_page_image(&[0u8; 64], 7);
        assert_eq!(junk.kind, PageKind::Other);
        assert!(junk.rows.is_empty());
        assert_eq!(PageKind::Other.label(), "overflow / freelist");
        assert!(super::decode_page_image(&[], 1).rows.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    /// Write attribution rests on this: a WAL frame carries a page number, and
    /// the page map says whose page that is. Read from the file, without asking
    /// SQLite, since the point is to attribute a write to a database nobody has
    /// opened.
    #[test]
    fn the_page_map_names_the_owner_of_every_page_it_claims() {
        let mut path = std::env::temp_dir();
        path.push(format!("zdbview_recover_{}_owners.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE alpha (v TEXT);
             CREATE TABLE beta (v TEXT);
             CREATE INDEX alpha_v ON alpha(v);",
        )
        .unwrap();
        // Enough rows that each table owns several pages, not just its root.
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..500 {
            tx.execute(
                "INSERT INTO alpha VALUES (?1)",
                [format!("a{i} padded out")],
            )
            .unwrap();
            tx.execute("INSERT INTO beta VALUES (?1)", [format!("b{i} padded out")])
                .unwrap();
        }
        tx.commit().unwrap();
        drop(conn);

        let owners = super::page_owners(&path).expect("a page map");
        assert_eq!(
            owners.get(&1).map(String::as_str),
            Some("sqlite_schema"),
            "page 1 is the schema, whatever else it holds"
        );
        let count = |name: &str| owners.values().filter(|v| v.as_str() == name).count();
        assert!(count("alpha") > 1, "alpha owns more than its root");
        assert!(count("beta") > 1, "and so does beta");
        assert!(
            count("index alpha_v") > 0,
            "an index is named too, marked as one: {:?}",
            owners.values().collect::<std::collections::HashSet<_>>()
        );
        // No page is claimed twice, since each map entry is one owner.
        let pages: Vec<u32> = owners.keys().copied().collect();
        assert_eq!(
            pages.len(),
            pages.iter().collect::<std::collections::HashSet<_>>().len()
        );

        // A file that is not a database has no pages to attribute.
        let junk = path.with_extension("junk");
        std::fs::write(&junk, b"not a database at all, just some bytes").unwrap();
        assert!(super::page_owners(&junk).is_err());

        let _ = std::fs::remove_file(&junk);
        let _ = std::fs::remove_file(&path);
    }

    /// In WAL mode the database file holds the *pre-write* version of every page
    /// the log has since rewritten, so a table created but never checkpointed is
    /// not in the file at all. Recovery has to read the log on top, or it
    /// recovers the past.
    #[test]
    fn recovery_reads_what_only_the_log_holds() {
        let mut path = std::env::temp_dir();
        path.push(format!("zdbview_recover_{}_wal.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(crate::wal::wal_path(&path));

        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute_batch(
            "CREATE TABLE only_in_log (name TEXT);
             INSERT INTO only_in_log VALUES ('written'), ('but not checkpointed');",
        )
        .unwrap();

        // The premise: the database file on its own does not have this table. A
        // copy without the sidecar is what another reader of the file would see.
        let orphan = path.with_extension("nolog.db");
        std::fs::copy(&path, &orphan).unwrap();
        let alone = rusqlite::Connection::open(&orphan).unwrap();
        let table_count: i64 = alone
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'only_in_log'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            table_count, 0,
            "the file alone must not have it, or this proves nothing"
        );
        drop(alone);

        // Recovery reads the log on top and finds both the schema and the rows.
        let recovered = super::recover(&path).expect("recovery");
        assert!(
            recovered.tables.iter().any(|t| t.name == "only_in_log"),
            "the uncheckpointed schema is recovered: {:?}",
            recovered.tables
        );
        let names: Vec<String> = recovered
            .rows_for("only_in_log")
            .filter_map(|r| match r.values.first() {
                Some(Value::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["written", "but not checkpointed"]);

        drop(conn);
        let _ = std::fs::remove_file(&orphan);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(crate::wal::wal_path(&path));
    }

    /// A database SQLite refuses to read is exactly what recovery is for: the
    /// pass reads rows out of the pages that are still there, without opening it.
    #[test]
    fn rows_come_back_from_a_file_sqlite_will_not_read() {
        let mut path = std::env::temp_dir();
        path.push(format!("zdbview_recover_{}_broken.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("CREATE TABLE people (name TEXT, age INTEGER)", [])
                .unwrap();
            let tx = conn.unchecked_transaction().unwrap();
            for i in 0..2000 {
                tx.execute(
                    "INSERT INTO people VALUES (?1, ?2)",
                    rusqlite::params![format!("person{i}"), i],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        // Cut the file short of the pages its header accounts for — the shape a
        // half-copied or half-written database has.
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes.len() > 40_960,
            "the table has to span pages: {}",
            bytes.len()
        );
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        let refused = rusqlite::Connection::open(&path)
            .and_then(|c| c.query_row("SELECT count(*) FROM people", [], |r| r.get::<_, i64>(0)));
        assert!(
            refused.is_err(),
            "sqlite must refuse it, or this proves nothing: {refused:?}"
        );

        let recovered = super::recover(&path).expect("recovery reads the pages regardless");
        let names: Vec<String> = recovered
            .rows_for("people")
            .filter_map(|r| match r.values.first() {
                Some(Value::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(
            names.len() > 100,
            "the surviving pages give up their rows: {} recovered",
            names.len()
        );
        assert!(
            names.contains(&"person0".to_string()),
            "including the first"
        );
        assert!(
            recovered
                .tables
                .iter()
                .any(|t| t.name == "people" && t.columns == vec!["name", "age"]),
            "the schema is recovered too: {:?}",
            recovered.tables
        );

        // The script replays: the CREATE, then rowid-preserving inserts that a
        // second run cannot trip over.
        let sql = super::to_sql(&recovered);
        assert!(sql.contains("CREATE TABLE people"), "{}", &sql[..400]);
        assert!(
            sql.contains(r#"INSERT OR IGNORE INTO "people"(_rowid_, "name", "age") VALUES (1, 'person0', 0);"#),
            "the first row, with its rowid"
        );
        assert!(sql.trim_end().ends_with("COMMIT;"));

        let _ = std::fs::remove_file(&path);
    }

    /// A truncated record is what a damaged page holds; decoding one must stop
    /// rather than read past the payload or panic.
    #[test]
    fn a_truncated_record_decodes_to_what_survived() {
        // Header: length 4, then serial types 1 (i8), 13+2*3 = 19 (3-char text).
        let full = [0x03u8, 0x01, 0x13, 0x07, b'a', b'b', b'c'];
        assert_eq!(
            super::decode_record(&full),
            vec![Value::Int(7), Value::Text("abc".into())]
        );
        // The same record with its body cut short: the int survives, the text
        // that is no longer there comes back NULL rather than as stray bytes.
        assert_eq!(
            super::decode_record(&full[..5]),
            vec![Value::Int(7), Value::Null]
        );
        // Nothing at all is no values, not a panic.
        assert!(super::decode_record(&[]).is_empty());
        assert!(super::decode_record(&[0x80]).is_empty());
    }
}
