//! Backend CRUD/inspection tests. These exercise the SQLite and rkyv stores
//! directly — the layer the TUI drives — without needing a terminal.

use std::io::Write;

// Pull the crate's modules in by path. The binary crate exposes them via the
// integration test harness only if declared in a lib; since zdbview is a bin,
// re-include the sources under test.
//
// Each re-included module is compiled fresh into this test binary, so whatever
// the tests below don't call looks dead here even though the real binary uses
// it — hence the per-module allow. It is scoped to the re-inclusion, so dead
// code in the test file itself is still reported.
// `rkyv_inspect` formats its hex rows through `hexedit` (one shared layout for
// every hex view), which in turn styles from `theme`, so both come along.
#[allow(dead_code)]
#[path = "../src/hexedit.rs"]
mod hexedit;
#[allow(dead_code)]
#[path = "../src/mru.rs"]
mod mru;
#[allow(dead_code)]
#[path = "../src/recover.rs"]
mod recover;
#[allow(dead_code)]
#[path = "../src/rkyv_inspect.rs"]
mod rkyv_inspect;
#[allow(dead_code)]
#[path = "../src/sqlite.rs"]
mod sqlite;
#[allow(dead_code)]
#[path = "../src/store.rs"]
mod store;
#[allow(dead_code)]
#[path = "../src/theme.rs"]
mod theme;
// `recover` applies a database's write-ahead log before reading its pages, so the
// WAL parser comes along.
#[allow(dead_code)]
#[path = "../src/wal.rs"]
mod wal;

use rkyv_inspect::RkyvStore;
use sqlite::{Sort, SqliteStore};
use store::{detect, Kind};

/// A page request over `table`: the shape every row fetch below wants, with no
/// cursor hint and no counted total.
fn pq<'a>(
    table: &'a str,
    limit: i64,
    offset: i64,
    sort: Option<&'a Sort>,
    filter: &'a str,
) -> sqlite::PageQuery<'a> {
    sqlite::PageQuery {
        table,
        limit,
        offset,
        sort,
        filter,
        hint: None,
        known_total: None,
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zdbview_test_{}_{}", std::process::id(), name));
    p
}

#[test]
fn sqlite_full_crud_roundtrip() {
    let path = tmp("crud.db");
    let _ = std::fs::remove_file(&path);

    // Build a table with rusqlite directly, then drive it through SqliteStore.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("CREATE TABLE items (name TEXT, qty INTEGER)", [])
        .unwrap();
    conn.execute("INSERT INTO items (name, qty) VALUES ('a', 1)", [])
        .unwrap();
    conn.execute("INSERT INTO items (name, qty) VALUES ('b', 2)", [])
        .unwrap();
    drop(conn);

    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.tables, vec!["items".to_string()]);
    assert_eq!(store.count("items").unwrap(), 2);
    assert_eq!(store.columns("items").unwrap(), vec!["name", "qty"]);

    let view = store.rows(&pq("items", 100, 0, None, "")).unwrap();
    assert_eq!(view.total, 2);
    assert_eq!(view.rows.len(), 2);
    assert_eq!(view.rows[0], vec!["a".to_string(), "1".to_string()]);
    let rowid_a = view.rowids[0].expect("rowid present");

    // UPDATE
    store
        .update_cell_keyed("items", &sqlite::RowKey::Rowid(rowid_a), "qty", "42")
        .unwrap();
    let view = store.rows(&pq("items", 100, 0, None, "")).unwrap();
    assert_eq!(view.rows[0], vec!["a".to_string(), "42".to_string()]);

    // INSERT (default values)
    store.insert_blank("items").unwrap();
    assert_eq!(store.count("items").unwrap(), 3);

    // DELETE
    store
        .delete_row_keyed("items", &sqlite::RowKey::Rowid(rowid_a))
        .unwrap();
    assert_eq!(store.count("items").unwrap(), 2);
    let view = store.rows(&pq("items", 100, 0, None, "")).unwrap();
    assert!(view.rows.iter().all(|r| r[0] != "a"));

    // raw exec
    let affected = store.exec("UPDATE items SET name = 'z'").unwrap();
    assert_eq!(affected, 2);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn rkyv_structural_strings_and_hex() {
    let path = tmp("archive.rkyv");
    // A synthetic binary blob: some bytes + an embedded string + more bytes.
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&[0x00, 0x01, 0x02]).unwrap();
    f.write_all(b"hello_field").unwrap();
    f.write_all(&[0xff, 0xfe]).unwrap();
    f.write_all(b"key").unwrap(); // len 3 — below MIN, must be skipped at min=4
    drop(f);

    let store = RkyvStore::open(&path).unwrap();
    assert_eq!(store.len(), 3 + 11 + 2 + 3);

    let hits = store.strings(4).hits;
    assert_eq!(hits.len(), 1, "only the >=4 run should match");
    assert_eq!(hits[0].text, "hello_field");
    assert_eq!(hits[0].offset, 3);

    // shorter min picks up the 3-char run too
    let hits = store.strings(3).hits;
    assert_eq!(hits.len(), 2);

    // hex row format: offset + 16 columns
    let row = store.hex_row(0);
    assert!(row.starts_with("00000000  "));
    assert!(row.contains("|"));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn mru_record_dedup_and_order() {
    let file = tmp("recent.list");
    let _ = std::fs::remove_file(&file);

    // Create three real files to record (paths must exist for canonicalize).
    let a = tmp("mru_a.db");
    let b = tmp("mru_b.rkyv");
    std::fs::write(&a, b"x").unwrap();
    std::fs::write(&b, b"y").unwrap();

    mru::record_path(&file, &a, Kind::Sqlite);
    mru::record_path(&file, &b, Kind::Rkyv);
    // Re-record `a`: it must move to the front, not duplicate.
    mru::record_path(&file, &a, Kind::Sqlite);

    let entries = mru::load_path(&file);
    assert_eq!(entries.len(), 2, "dedup by path");
    assert_eq!(entries[0].path, std::fs::canonicalize(&a).unwrap());
    assert_eq!(entries[0].kind, Kind::Sqlite);
    assert_eq!(entries[1].path, std::fs::canonicalize(&b).unwrap());

    for p in [&file, &a, &b] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn detect_rkyv_when_db_extension_but_not_sqlite() {
    // A .db file that is NOT a SQLite database (the plugins.db case) must be
    // detected as rkyv, because the magic check is authoritative.
    let path = tmp("fake.db");
    std::fs::write(&path, b"this is definitely not a sqlite header at all").unwrap();
    assert!(matches!(detect(&path, false, false).unwrap(), Kind::Rkyv));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn detect_sqlite_by_magic_and_extension() {
    // Real sqlite file → magic detection.
    let dbpath = tmp("detect.db");
    let _ = std::fs::remove_file(&dbpath);
    let conn = rusqlite::Connection::open(&dbpath).unwrap();
    conn.execute("CREATE TABLE t (x)", []).unwrap();
    drop(conn);
    assert!(matches!(
        detect(&dbpath, false, false).unwrap(),
        Kind::Sqlite
    ));

    // Non-sqlite file with unknown extension → rkyv default.
    let binpath = tmp("blob.bin");
    std::fs::write(&binpath, [0u8, 1, 2, 3]).unwrap();
    assert!(matches!(
        detect(&binpath, false, false).unwrap(),
        Kind::Rkyv
    ));

    // Force flags win.
    assert!(matches!(
        detect(&binpath, true, false).unwrap(),
        Kind::Sqlite
    ));
    assert!(matches!(detect(&dbpath, false, true).unwrap(), Kind::Rkyv));

    let _ = std::fs::remove_file(&dbpath);
    let _ = std::fs::remove_file(&binpath);
}

/// Build a table whose natural rowid order differs from every column order, so a
/// wrong ORDER BY cannot accidentally pass.
/// A row query over table `t`, which is what every search test here searches.
fn rq<'a>(
    columns: &'a [String],
    term: &'a str,
    sort: Option<&'a Sort>,
    filter: &'a str,
) -> sqlite::RowQuery<'a> {
    sqlite::RowQuery {
        table: "t",
        columns,
        term,
        sort,
        filter,
    }
}

fn sortable_db(name: &str) -> (std::path::PathBuf, SqliteStore) {
    let path = tmp(name);
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("CREATE TABLE t (name TEXT, qty INTEGER)", [])
        .unwrap();
    for (n, q) in [("pear", 3), ("apple", 10), ("fig", 3), ("date", 7)] {
        conn.execute("INSERT INTO t (name, qty) VALUES (?1, ?2)", (n, q))
            .unwrap();
    }
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();
    (path, store)
}

fn col(view: &sqlite::RowsView, i: usize) -> Vec<String> {
    view.rows.iter().map(|r| r[i].clone()).collect()
}

#[test]
fn rows_sort_ascending_descending_and_natural_order() {
    let (path, store) = sortable_db("sort.db");

    // No sort: insertion (rowid) order.
    let v = store.rows(&pq("t", 100, 0, None, "")).unwrap();
    assert_eq!(col(&v, 0), ["pear", "apple", "fig", "date"]);

    let asc = Sort {
        column: "name".into(),
        desc: false,
    };
    let v = store.rows(&pq("t", 100, 0, Some(&asc), "")).unwrap();
    assert_eq!(col(&v, 0), ["apple", "date", "fig", "pear"]);

    let desc = Sort {
        column: "name".into(),
        desc: true,
    };
    let v = store.rows(&pq("t", 100, 0, Some(&desc), "")).unwrap();
    assert_eq!(col(&v, 0), ["pear", "fig", "date", "apple"]);

    // Numeric column must sort numerically, not lexically (10 after 7).
    let qty = Sort {
        column: "qty".into(),
        desc: false,
    };
    let v = store.rows(&pq("t", 100, 0, Some(&qty), "")).unwrap();
    assert_eq!(col(&v, 1), ["3", "3", "7", "10"]);

    // An unknown column falls back to rowid order instead of failing the query.
    let bogus = Sort {
        column: "nope".into(),
        desc: false,
    };
    let v = store.rows(&pq("t", 100, 0, Some(&bogus), "")).unwrap();
    assert_eq!(col(&v, 0), ["pear", "apple", "fig", "date"]);

    let _ = std::fs::remove_file(&path);
}

/// Paging must partition the sorted order without gaps or repeats — the rowid
/// tiebreaker is what makes this hold when the sort column has duplicates.
#[test]
fn sorted_paging_is_stable_across_duplicate_keys() {
    let (path, store) = sortable_db("sort_page.db");
    let qty = Sort {
        column: "qty".into(),
        desc: false,
    };

    let mut seen = Vec::new();
    for offset in [0, 2] {
        let page = store.rows(&pq("t", 2, offset, Some(&qty), "")).unwrap();
        assert_eq!(page.rows.len(), 2);
        seen.extend(col(&page, 0));
    }
    let full = col(&store.rows(&pq("t", 100, 0, Some(&qty), "")).unwrap(), 0);
    assert_eq!(seen, full, "pages must concatenate into the full order");
    let _ = std::fs::remove_file(&path);
}

/// Search steps through matches in *display* order, so with a sort active the
/// next match is the next one on screen, not the next by rowid.
#[test]
fn search_and_ordinals_follow_the_sorted_order() {
    let (path, store) = sortable_db("sort_search.db");
    let cols = store.columns("t").unwrap();
    let asc = Sort {
        column: "name".into(),
        desc: false,
    };

    // Sorted ascending: apple(2) date(4) fig(3) pear(1) by rowid.
    let sorted = store.rows(&pq("t", 100, 0, Some(&asc), "")).unwrap();
    let rowid_of = |n: &str| -> i64 {
        let i = sorted.rows.iter().position(|r| r[0] == n).unwrap();
        sorted.rowids[i].unwrap()
    };

    // Every row matches "e"? No — apple, date, pear do. From apple, forward is
    // date (next in sorted order), not fig or the next rowid.
    let next = store
        .find_row(&rq(&cols, "e", Some(&asc), ""), rowid_of("apple"), true)
        .unwrap();
    assert_eq!(next, Some(rowid_of("date")));

    // Backward from pear is date as well.
    let prev = store
        .find_row(&rq(&cols, "e", Some(&asc), ""), rowid_of("pear"), false)
        .unwrap();
    assert_eq!(prev, Some(rowid_of("date")));

    // Nothing after pear: the caller wraps via the edge query, which returns the
    // first match in display order.
    assert_eq!(
        store
            .find_row(&rq(&cols, "e", Some(&asc), ""), rowid_of("pear"), true)
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .find_row_edge(&rq(&cols, "e", Some(&asc), ""), true)
            .unwrap(),
        Some(rowid_of("apple"))
    );
    assert_eq!(
        store
            .find_row_edge(&rq(&cols, "e", Some(&asc), ""), false)
            .unwrap(),
        Some(rowid_of("pear"))
    );

    // Ordinals are positions in the sorted view: apple is 1st, pear 4th.
    assert_eq!(
        store
            .rowid_ordinal("t", rowid_of("apple"), Some(&asc), "")
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .rowid_ordinal("t", rowid_of("pear"), Some(&asc), "")
            .unwrap(),
        4
    );
    // Without a sort the same rowid is placed by rowid instead.
    assert_eq!(
        store
            .rowid_ordinal("t", rowid_of("pear"), None, "")
            .unwrap(),
        1
    );

    // Descending flips both the stepping direction and the ordinals.
    let desc = Sort {
        column: "name".into(),
        desc: true,
    };
    assert_eq!(
        store
            .find_row(&rq(&cols, "e", Some(&desc), ""), rowid_of("pear"), true)
            .unwrap(),
        Some(rowid_of("date"))
    );
    assert_eq!(
        store
            .rowid_ordinal("t", rowid_of("pear"), Some(&desc), "")
            .unwrap(),
        1
    );

    let _ = std::fs::remove_file(&path);
}

/// A column name with a quote must not break out of its identifier.
#[test]
fn sort_column_names_are_escaped() {
    let path = tmp("sort_quote.db");
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(r#"CREATE TABLE t ("od""d" TEXT)"#, [])
        .unwrap();
    conn.execute(r#"INSERT INTO t VALUES ('b'), ('a')"#, [])
        .unwrap();
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();
    let cols = store.columns("t").unwrap();
    assert_eq!(cols, vec![r#"od"d"#.to_string()]);
    let sort = Sort {
        column: cols[0].clone(),
        desc: false,
    };
    let v = store.rows(&pq("t", 100, 0, Some(&sort), "")).unwrap();
    assert_eq!(col(&v, 0), ["a", "b"]);
    assert_eq!(store.rowid_ordinal("t", 2, Some(&sort), "").unwrap(), 1);
    let _ = std::fs::remove_file(&path);
}

// ----- the sqlite3 shell's own reports (.dbinfo, .intck, .lint, .eqp, .dump) ---

/// A database with one of everything the shell's reports care about: a parent and
/// a child joined by a foreign key, an index, a view, a trigger, and a row whose
/// values cover every storage class including a blob and an embedded quote.
fn reported_db(name: &str) -> (std::path::PathBuf, SqliteStore) {
    let path = tmp(name);
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE child (
            id INTEGER PRIMARY KEY,
            parent_id INTEGER REFERENCES parent(id),
            note TEXT,
            weight REAL,
            raw BLOB
         );
         CREATE INDEX parent_name_idx ON parent(name);
         CREATE VIEW child_names AS SELECT c.id, p.name FROM child c JOIN parent p ON p.id = c.parent_id;
         CREATE TRIGGER child_ins AFTER INSERT ON child BEGIN
            UPDATE parent SET name = name WHERE id = new.parent_id;
         END;
         INSERT INTO parent (id, name) VALUES (1, 'it''s here');
         INSERT INTO child (id, parent_id, note, weight, raw)
            VALUES (1, 1, 'plain', 1.5, x'00ff10'), (2, 1, NULL, 2.0, NULL);",
    )
    .unwrap();
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();
    (path, store)
}

fn pairs_get<'a>(info: &'a [(String, String)], key: &str) -> &'a str {
    info.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("no {key:?} in {info:?}"))
}

#[test]
fn db_info_reports_pragmas_and_object_counts() {
    let (path, store) = reported_db("dbinfo.db");
    let info = store.db_info();

    // Pragmas, straight from the file.
    let page_size: u64 = pairs_get(&info, "page size").parse().unwrap();
    let page_count: u64 = pairs_get(&info, "page count").parse().unwrap();
    assert!(
        page_size.is_power_of_two() && page_size >= 512,
        "{page_size}"
    );
    assert!(page_count > 0);
    assert_eq!(pairs_get(&info, "encoding"), "UTF-8");
    assert_eq!(pairs_get(&info, "journal mode"), "delete");
    assert_eq!(
        pairs_get(&info, "data size"),
        format!("{} bytes", page_size * page_count),
        "data size is derived from the two pragmas above"
    );

    // Object counts, from sqlite_master.
    assert_eq!(pairs_get(&info, "tables"), "2");
    assert_eq!(pairs_get(&info, "indexes"), "1");
    assert_eq!(pairs_get(&info, "views"), "1");
    assert_eq!(pairs_get(&info, "triggers"), "1");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn integrity_and_quick_check_pass_on_a_sound_file() {
    let (path, store) = reported_db("intck.db");
    assert_eq!(
        store.integrity_check(false).unwrap(),
        vec!["ok".to_string()]
    );
    assert_eq!(store.integrity_check(true).unwrap(), vec!["ok".to_string()]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn foreign_key_lint_flags_only_unindexed_child_columns() {
    let (path, store) = reported_db("fklint.db");

    // `child.parent_id` has no index, so every parent-row change scans `child`.
    let lint = store.missing_fk_indexes().unwrap();
    assert_eq!(lint.len(), 1, "one unindexed foreign key: {lint:?}");
    assert!(
        lint[0].starts_with("child.parent_id -> parent"),
        "got {:?}",
        lint[0]
    );

    // Indexing that column silences the lint; an index on some other column of
    // the same table must not.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE INDEX child_note_idx ON child(note)")
        .unwrap();
    let store2 = SqliteStore::open(&path).unwrap();
    assert_eq!(
        store2.missing_fk_indexes().unwrap().len(),
        1,
        "an index on an unrelated column does not serve the key"
    );
    conn.execute_batch("CREATE INDEX child_parent_idx ON child(parent_id)")
        .unwrap();
    drop(conn);
    let store3 = SqliteStore::open(&path).unwrap();
    assert!(
        store3.missing_fk_indexes().unwrap().is_empty(),
        "the key now has an index"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn query_plan_is_drawn_as_the_shell_draws_it() {
    let (path, store) = reported_db("eqp.db");

    // A plain scan: header plus one leaf.
    let plan = store.explain_plan("SELECT * FROM child").unwrap();
    assert_eq!(plan[0], "QUERY PLAN");
    assert_eq!(plan.len(), 2, "{plan:?}");
    assert!(plan[1].starts_with("`--SCAN child"), "{:?}", plan[1]);

    // A join with an ORDER BY has siblings, so all but the last get `|--`, and
    // the index the planner picks is named.
    let plan = store
        .explain_plan(
            "SELECT p.name FROM parent p JOIN child c ON c.parent_id = p.id ORDER BY p.name",
        )
        .unwrap();
    assert!(plan.len() >= 3, "{plan:?}");
    assert!(
        plan[1..plan.len() - 1].iter().all(|l| l.starts_with("|--")),
        "every step but the last has a sibling: {plan:?}"
    );
    assert!(
        plan.last().unwrap().starts_with("`--"),
        "the last step closes the tree: {:?}",
        plan.last()
    );
    // Bad SQL is an error, not an empty plan.
    assert!(store.explain_plan("SELECT * FROM nope").is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn dump_replays_into_an_empty_database() {
    let (path, store) = reported_db("dump.db");
    let sql = store.dump(None).unwrap();

    // The frame the shell writes.
    assert!(
        sql.starts_with("PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\n"),
        "{sql}"
    );
    assert!(sql.ends_with("COMMIT;\n"), "{sql}");
    // Values are literals, not the strings the grid shows: the blob comes out as
    // hex and the embedded quote is doubled.
    assert!(sql.contains("x'00ff10'"), "blob must survive as hex: {sql}");
    assert!(
        !sql.contains("<blob"),
        "no display strings in a dump: {sql}"
    );
    assert!(sql.contains("'it''s here'"), "{sql}");
    assert!(
        sql.contains(",NULL,"),
        "NULL is a keyword, not a string: {sql}"
    );

    // Replaying it rebuilds the database, values included.
    let replay = tmp("dump_replay.db");
    let _ = std::fs::remove_file(&replay);
    let conn = rusqlite::Connection::open(&replay).unwrap();
    conn.execute_batch(&sql).unwrap();
    let (note, weight, raw): (Option<String>, f64, Option<Vec<u8>>) = conn
        .query_row(
            "SELECT note, weight, raw FROM child WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(note.as_deref(), Some("plain"));
    assert_eq!(weight, 1.5, "a real must not be truncated to an integer");
    assert_eq!(raw, Some(vec![0x00, 0xff, 0x10]));
    let objects: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(objects, 5, "2 tables, 1 index, 1 view, 1 trigger");

    // One table only, on request.
    let one = store.dump(Some("parent")).unwrap();
    assert!(one.contains("CREATE TABLE parent"), "{one}");
    assert!(!one.contains("CREATE TABLE child"), "{one}");
    for p in [path, replay] {
        let _ = std::fs::remove_file(p);
    }
}

/// A virtual table cannot be dumped as `CREATE VIRTUAL TABLE`: that runs the
/// module's constructor, which builds the shadow tables the dump then tries to
/// create again. This is the case that made a naive dump unreplayable.
#[test]
fn dump_of_a_virtual_table_replays() {
    let path = tmp("dump_fts.db");
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT);
         CREATE VIRTUAL TABLE docs_fts USING fts5(body);
         INSERT INTO docs (body) VALUES ('the quick brown fox'), ('lazy dog');
         INSERT INTO docs_fts (rowid, body) SELECT id, body FROM docs;",
    )
    .unwrap();
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();
    let sql = store.dump(None).unwrap();

    assert!(
        sql.contains("PRAGMA writable_schema=ON;") && sql.contains("PRAGMA writable_schema=OFF;"),
        "the schema row is written directly, as the shell does it: {sql}"
    );
    assert!(
        sql.contains("INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)"),
        "{sql}"
    );
    assert!(
        !sql.lines().any(|l| l.starts_with("CREATE VIRTUAL")),
        "running the create would build the shadow tables twice — the statement \
         may appear only as the text inserted into sqlite_schema: {sql}"
    );
    assert!(
        sql.contains("CREATE TABLE IF NOT EXISTS 'docs_fts_data'"),
        "shadow tables are created only if absent: {sql}"
    );

    let replay = tmp("dump_fts_replay.db");
    let _ = std::fs::remove_file(&replay);
    let conn = rusqlite::Connection::open(&replay).unwrap();
    conn.execute_batch(&sql).expect("the dump must replay");
    // A schema row written under `writable_schema` is invisible to the connection
    // that wrote it until the schema is re-read, exactly as with the shell — so
    // reopen, then check the rebuilt index answers queries, which is the only
    // proof the shadow tables came across intact.
    drop(conn);
    let conn = rusqlite::Connection::open(&replay).unwrap();
    let hit: String = conn
        .query_row(
            "SELECT body FROM docs_fts WHERE docs_fts MATCH 'brown'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hit, "the quick brown fox");
    for p in [path, replay] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn backup_writes_a_second_readable_database() {
    let (path, store) = reported_db("backup_src.db");
    let out = tmp("backup_dst.db");
    let _ = std::fs::remove_file(&out);
    store.backup_to(&out).unwrap();

    assert!(matches!(detect(&out, false, false).unwrap(), Kind::Sqlite));
    let copy = SqliteStore::open(&out).unwrap();
    assert_eq!(copy.tables, store.tables);
    let rows = copy.rows(&pq("child", 10, 0, None, "")).unwrap();
    assert_eq!(rows.rows.len(), 2);

    // VACUUM INTO refuses to overwrite, which is what keeps a backup from
    // clobbering a live database.
    assert!(
        store.backup_to(&out).is_err(),
        "an existing target is an error"
    );
    for p in [path, out] {
        let _ = std::fs::remove_file(p);
    }
}

// ----- what the GUI/CLI tools show: column stats, blob cells, maintenance -----

#[test]
fn column_stats_describe_each_column() {
    let path = tmp("stats.db");
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER, tag TEXT, qty INTEGER);
         INSERT INTO t VALUES (1, 'a', 10), (2, 'a', 20), (3, NULL, 30), (4, 'bbbb', NULL);
         -- A column declared INTEGER holding text: SQLite allows it, and the
         -- numeric count is the only place that shows up.
         INSERT INTO t VALUES (5, 'c', 'not a number');",
    )
    .unwrap();
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();
    let stats = store.column_stats("t").unwrap();
    let by = |name: &str| stats.iter().find(|c| c.name == name).unwrap();

    let tag = by("tag");
    assert_eq!(tag.declared, "TEXT");
    assert_eq!(tag.rows, 5);
    assert_eq!(tag.nulls, 1);
    assert_eq!(tag.distinct, 3, "a, bbbb, c — NULL is not a distinct value");
    assert_eq!(tag.min, "a");
    assert_eq!(tag.max, "c");
    assert_eq!(tag.longest, 4, "bbbb");
    assert_eq!(tag.numeric, 0);
    assert!(tag.avg.is_none(), "text has no mean");

    let qty = by("qty");
    assert_eq!(qty.nulls, 1);
    assert_eq!(
        qty.numeric, 3,
        "three of the five cells are stored as numbers"
    );
    assert_eq!(
        qty.avg,
        Some(20.0),
        "mean of 10, 20, 30 — the text is skipped"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn frequency_ranks_values_by_count() {
    let path = tmp("freq.db");
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE t (k TEXT);
         INSERT INTO t VALUES ('x'),('x'),('x'),('y'),('y'),('z'),(NULL);",
    )
    .unwrap();
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();
    let freq = store.frequency("t", "k", 3).unwrap();
    assert_eq!(
        freq,
        vec![
            ("x".to_string(), 3),
            ("y".to_string(), 2),
            ("NULL".to_string(), 1)
        ],
        "counted descending, and NULL is a value here — it is a row that exists"
    );
    assert_eq!(
        store.frequency("t", "k", 1).unwrap().len(),
        1,
        "limit applies"
    );
    let _ = std::fs::remove_file(&path);
}

/// A blob cell has no text form, so it is read and written as bytes. Editing it
/// through the text path would store the description of the bytes instead.
#[test]
fn blob_cells_round_trip_as_bytes() {
    let path = tmp("blob.db");
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE t (raw BLOB, txt TEXT)")
        .unwrap();
    conn.execute(
        "INSERT INTO t VALUES (?1, 'plain')",
        [&[0x00u8, 0xff, 0x41][..]],
    )
    .unwrap();
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();

    assert!(store
        .cell_is_blob_keyed("t", &sqlite::RowKey::Rowid(1), "raw")
        .unwrap());
    assert!(
        !store
            .cell_is_blob_keyed("t", &sqlite::RowKey::Rowid(1), "txt")
            .unwrap(),
        "text stays with the line editor"
    );
    assert_eq!(
        store
            .cell_bytes_keyed("t", &sqlite::RowKey::Rowid(1), "raw")
            .unwrap(),
        [0x00, 0xff, 0x41]
    );

    store
        .update_cell_blob_keyed(
            "t",
            &sqlite::RowKey::Rowid(1),
            "raw",
            &[0xde, 0xad, 0xbe, 0xef],
        )
        .unwrap();
    assert_eq!(
        store
            .cell_bytes_keyed("t", &sqlite::RowKey::Rowid(1), "raw")
            .unwrap(),
        [0xde, 0xad, 0xbe, 0xef]
    );
    assert!(
        store
            .cell_is_blob_keyed("t", &sqlite::RowKey::Rowid(1), "raw")
            .unwrap(),
        "it must still be a blob, not a string of hex digits"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn maintenance_statements_run_and_report_the_size_change() {
    use sqlite::Maintenance;
    let path = tmp("maint.db");
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE t (a TEXT); CREATE INDEX t_a ON t(a);")
        .unwrap();
    for i in 0..500 {
        conn.execute(
            "INSERT INTO t VALUES (?1)",
            [format!("row {i} padding padding")],
        )
        .unwrap();
    }
    conn.execute_batch("DELETE FROM t WHERE rowid % 2 = 0")
        .unwrap();
    drop(conn);

    let store = SqliteStore::open(&path).unwrap();
    // Half the rows are gone, so a vacuum has pages to reclaim.
    let delta = store.maintain(Maintenance::Vacuum).unwrap();
    assert!(delta < 0, "vacuum must shrink this file, got {delta}");
    // ANALYZE's visible result is the statistics table it writes.
    store.maintain(Maintenance::Analyze).unwrap();
    // The store hides `sqlite_%` tables, so ask the file directly.
    let probe = rusqlite::Connection::open(&path).unwrap();
    let stat1: i64 = probe
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'sqlite_stat1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stat1, 1, "ANALYZE writes sqlite_stat1");
    drop(probe);
    store.maintain(Maintenance::Reindex).unwrap();
    // The data survived all three.
    assert_eq!(store.count("t").unwrap(), 250);
    assert_eq!(Maintenance::Vacuum.label(), "VACUUM");
    let _ = std::fs::remove_file(&path);
}

/// `.import`: the header names the columns, so a file ordered differently from the
/// table still lands correctly, and a bad row takes the whole file with it rather
/// than leaving half of it inserted.
#[test]
fn import_maps_columns_by_header_and_rolls_back_a_bad_file() {
    let path = tmp("import.db");
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE p (id INTEGER PRIMARY KEY, name TEXT, score REAL, note TEXT)")
        .unwrap();
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();

    // Header order is score, name — the reverse of the table's.
    let header = vec!["score".to_string(), "name".to_string()];
    let rows = vec![
        vec!["9.5".to_string(), "ada".to_string()],
        vec!["8".to_string(), "grace".to_string()],
    ];
    assert_eq!(store.import_rows("p", &header, &rows).unwrap(), 2);
    let view = store.rows(&pq("p", 10, 0, None, "")).unwrap();
    assert_eq!(view.rows[0][1], "ada");
    assert_eq!(view.rows[0][2], "9.5", "the value went to the named column");

    // A column the table does not have is an error, before anything is written.
    let bad_header = vec!["name".to_string(), "nope".to_string()];
    assert!(store
        .import_rows("p", &bad_header, &rows)
        .unwrap_err()
        .to_string()
        .contains("nope"));

    // A row with the wrong field count rolls the whole import back.
    let ragged = vec![
        vec!["1".to_string(), "fine".to_string()],
        vec!["2".to_string()],
    ];
    assert!(store.import_rows("p", &header, &ragged).is_err());
    assert_eq!(
        store.count("p").unwrap(),
        2,
        "the good row from the ragged file must not survive"
    );
    let _ = std::fs::remove_file(&path);
}

/// A filtered grid lists a subset, so `n` has to walk that subset and the ordinal
/// that positions the row has to count only what is listed. Before this, a search
/// with a filter active jumped to a hidden row and scrolled to the wrong page.
#[test]
fn search_and_ordinals_stay_inside_the_filter() {
    let path = tmp("filtered_search.db");
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE t (name TEXT, note TEXT);
         INSERT INTO t VALUES
            ('keep one',   'match'),
            ('drop two',   'match'),
            ('keep three', 'match'),
            ('drop four',  'match');",
    )
    .unwrap();
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();
    let cols = store.columns("t").unwrap();
    let name_of = |rid: i64| -> String {
        let v = store.rows(&pq("t", 10, 0, None, "")).unwrap();
        let i = v.rowids.iter().position(|r| *r == Some(rid)).unwrap();
        v.rows[i][0].clone()
    };

    // Unfiltered, stepping from row 1 finds row 2 — the one the filter hides.
    let next = store
        .find_row(&rq(&cols, "match", None, ""), 1, true)
        .unwrap()
        .unwrap();
    assert_eq!(name_of(next), "drop two");

    // With `keep` filtering the grid, the same step skips to the next listed row.
    let next = store
        .find_row(&rq(&cols, "match", None, "keep"), 1, true)
        .unwrap()
        .unwrap();
    assert_eq!(name_of(next), "keep three");

    // The wrap-around entry point is filtered too.
    let first = store
        .find_row_edge(&rq(&cols, "match", None, "keep"), true)
        .unwrap()
        .unwrap();
    assert_eq!(name_of(first), "keep one");
    let last = store
        .find_row_edge(&rq(&cols, "match", None, "keep"), false)
        .unwrap()
        .unwrap();
    assert_eq!(name_of(last), "keep three");

    // And the ordinal counts listed rows only: `keep three` is the 3rd row of the
    // table but the 2nd of the filtered view, which is what positions the page.
    assert_eq!(store.rowid_ordinal("t", next, None, "").unwrap(), 3);
    assert_eq!(store.rowid_ordinal("t", next, None, "keep").unwrap(), 2);

    // A filter that hides everything finds nothing rather than falling back to the
    // whole table.
    assert!(store
        .find_row_edge(&rq(&cols, "match", None, "nothing matches this"), true)
        .unwrap()
        .is_none());

    // The same holds with a sort active, where display order is not rowid order.
    let desc = Sort {
        column: "name".into(),
        desc: true,
    };
    let first = store
        .find_row_edge(&rq(&cols, "match", Some(&desc), "keep"), true)
        .unwrap()
        .unwrap();
    assert_eq!(name_of(first), "keep three", "descending by name, filtered");
    let _ = std::fs::remove_file(&path);
}

/// Per-column filters, the way DB Browser's filter row works: `name:value` limits
/// the match to that column, bare words still match anywhere, and terms are ANDed.
/// `name:` counts as a column only when `name` is one, so filtering for a value
/// that happens to contain a colon still works.
#[test]
fn a_filter_can_target_one_column() {
    let path = tmp("colfilter.db");
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE t (cwd TEXT, line TEXT);
         INSERT INTO t VALUES
            ('/home/zshrs',  'echo one'),
            ('/home/other',  'echo two'),
            ('/home/zshrs',  'ls three'),
            ('/tmp',         'at 12:30 do this');",
    )
    .unwrap();
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();
    let lines = |filter: &str| -> Vec<String> {
        store
            .rows(&pq("t", 50, 0, None, filter))
            .unwrap()
            .rows
            .iter()
            .map(|r| r[1].clone())
            .collect()
    };

    // A bare word still matches any column.
    assert_eq!(lines("zshrs").len(), 2);
    // One column only: `line:echo` must not match a cwd that contains "echo".
    assert_eq!(lines("line:echo"), ["echo one", "echo two"]);
    assert_eq!(lines("cwd:zshrs"), ["echo one", "ls three"]);
    // Terms are ANDed across columns.
    assert_eq!(lines("cwd:zshrs line:echo"), ["echo one"]);
    // A value with a colon whose prefix is not a column stays one plain term.
    assert_eq!(lines("12:30"), ["at 12:30 do this"]);
    // An unknown column name is a plain term too, so nothing matches "nope:x".
    assert!(lines("nope:x").is_empty());
    // The count the pager uses agrees with the rows.
    assert_eq!(store.count_filtered("t", "cwd:zshrs").unwrap(), 2);
    // And a per-column filter constrains the search the same way a bare one does.
    let cols = store.columns("t").unwrap();
    let q = sqlite::RowQuery {
        table: "t",
        columns: &cols,
        term: "echo",
        sort: None,
        filter: "cwd:zshrs",
    };
    let hit = store.find_row_edge(&q, true).unwrap().unwrap();
    let view = store.rows(&pq("t", 50, 0, None, "")).unwrap();
    let i = view.rowids.iter().position(|r| *r == Some(hit)).unwrap();
    assert_eq!(view.rows[i][1], "echo one");
    let _ = std::fs::remove_file(&path);
}

/// `.databases` / ATTACH, and the index advice `.expert` reports. The advice is
/// read from the plan the planner actually produced, so the assertions are about
/// what it chose, not about what it might have.
#[test]
fn attach_lists_databases_and_advice_follows_the_plan() {
    let main = tmp("attach_main.db");
    let other = tmp("attach_other.db");
    for p in [&main, &other] {
        let _ = std::fs::remove_file(p);
    }
    let conn = rusqlite::Connection::open(&main).unwrap();
    conn.execute_batch(
        "CREATE TABLE big (id INTEGER PRIMARY KEY, tag TEXT, note TEXT);
         CREATE INDEX big_tag ON big(tag);",
    )
    .unwrap();
    for i in 0..200 {
        conn.execute(
            "INSERT INTO big (tag, note) VALUES (?1, ?2)",
            [format!("tag{}", i % 7), format!("note {i}")],
        )
        .unwrap();
    }
    drop(conn);
    rusqlite::Connection::open(&other)
        .unwrap()
        .execute_batch("CREATE TABLE side (v TEXT)")
        .unwrap();

    let store = SqliteStore::open(&main).unwrap();
    // Only `main` until something is attached.
    let names: Vec<String> = store
        .databases()
        .unwrap()
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    assert_eq!(names, ["main".to_string()]);
    store.attach(&other, "side").unwrap();
    let listed = store.databases().unwrap();
    assert!(
        listed
            .iter()
            .any(|(a, f)| a == "side" && f.ends_with("attach_other.db")),
        "{listed:?}"
    );
    // A cross-database query works once attached.
    assert!(store.run("SELECT count(*) FROM side.side", 10).is_ok());
    store.detach("side").unwrap();
    assert_eq!(store.databases().unwrap().len(), 1, "detached again");

    // An unindexed column that the statement compares: advice names an index.
    let advice = store
        .index_advice("SELECT id FROM big WHERE note = 'note 5'")
        .unwrap();
    assert_eq!(advice.len(), 1, "{advice:?}");
    assert!(advice[0].contains("big: full scan"), "{advice:?}");
    assert!(
        advice[0].contains("CREATE INDEX") && advice[0].contains("\"note\""),
        "{advice:?}"
    );

    // A column that already has an index: the planner uses it, so there is nothing
    // to advise.
    assert!(
        store
            .index_advice("SELECT id FROM big WHERE tag = 'tag1'")
            .unwrap()
            .is_empty(),
        "an indexed lookup is not a full scan"
    );

    // A scan whose columns are all indexed already is reported without advice
    // rather than with an index that exists.
    let advice = store.index_advice("SELECT count(*) FROM big").unwrap();
    assert_eq!(advice.len(), 1);
    assert!(
        advice[0].contains("no unindexed column"),
        "a bare count scans, but nothing is compared: {advice:?}"
    );

    // Bad SQL is an error, not silent advice.
    assert!(store.index_advice("SELECT * FROM nope").is_err());
    for p in [main, other] {
        let _ = std::fs::remove_file(p);
    }
}

// ----- .recover: reading pages when SQLite refuses the file --------------------

/// A database with more rows than fit on one page, so its b-tree has an interior
/// root above real leaves. `page_size` is small to keep the fixture small.
fn multipage_db(name: &str, rows: usize) -> std::path::PathBuf {
    let path = tmp(name);
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "PRAGMA page_size=512;
         CREATE TABLE t (a TEXT, b INTEGER);
         CREATE INDEX t_b ON t(b);",
    )
    .unwrap();
    for i in 0..rows {
        conn.execute(
            "INSERT INTO t VALUES (?1, ?2)",
            (format!("row {i} with padding to fill the page"), i as i64),
        )
        .unwrap();
    }
    drop(conn);
    path
}

/// Overwrite `page` (1-based) with zeroes, which is what a bad sector looks like
/// to SQLite: it refuses the whole file.
fn zero_page(path: &std::path::Path, page: usize, page_size: usize) {
    let mut bytes = std::fs::read(path).unwrap();
    let start = (page - 1) * page_size;
    for b in &mut bytes[start..start + page_size] {
        *b = 0;
    }
    std::fs::write(path, bytes).unwrap();
}

/// The case `.recover` exists for: the table's b-tree root is destroyed, so every
/// leaf under it is unreachable and SQLite will not read the table at all. Reading
/// pages directly still finds every row.
#[test]
fn recover_reads_rows_a_corrupt_root_has_orphaned() {
    let path = multipage_db("recover_root.db", 400);
    // Sanity: intact, SQLite reads all 400.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 400);
        let root: i64 = conn
            .query_row(
                "SELECT rootpage FROM sqlite_master WHERE name = 't'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(root, 2, "the fixture's root page");
    }
    zero_page(&path, 2, 512);

    // SQLite now refuses the table. (`count(*)` alone can still be answered from
    // the index, so the query has to read the table's own pages.)
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert!(
            conn.query_row("SELECT sum(length(a)) FROM t", [], |r| r.get::<_, i64>(0))
                .is_err(),
            "a zeroed root must make SQLite refuse the table's rows"
        );
    }

    let found = recover::recover(&path).unwrap();
    assert_eq!(
        found.rows.len(),
        400,
        "every row comes back: {:?}",
        found.notes
    );
    assert_eq!(found.orphans(), 0, "all attributed to t: {:?}", found.notes);
    assert!(
        found.notes.iter().any(|n| n.contains("unreachable")),
        "the pass says how it attributed them: {:?}",
        found.notes
    );

    // The values are the original ones, not just the right count.
    let first = found
        .rows_for("t")
        .find(|r| r.rowid == Some(1))
        .expect("rowid 1");
    assert_eq!(
        first.values[0],
        recover::Value::Text("row 0 with padding to fill the page".into())
    );
    assert_eq!(first.values[1], recover::Value::Int(0));

    // And the script it writes replays into a working database.
    let sql = recover::to_sql(&found);
    let replay = tmp("recover_root_replay.db");
    let _ = std::fs::remove_file(&replay);
    let conn = rusqlite::Connection::open(&replay).unwrap();
    conn.execute_batch(&sql).expect("the recovery must replay");
    let (n, min, max): (i64, i64, i64) = conn
        .query_row("SELECT count(*), min(b), max(b) FROM t", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!((n, min, max), (400, 0, 399));
    // The index came back too, because its CREATE statement is in the script.
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='t_b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1);
    for p in [path, replay] {
        let _ = std::fs::remove_file(p);
    }
}

/// A truncated file: the header still claims the original page count, so the pass
/// has to trust the bytes that are actually there.
#[test]
fn recover_handles_a_truncated_file() {
    let path = multipage_db("recover_trunc.db", 400);
    let full = recover::recover(&path).unwrap().rows.len();
    assert_eq!(full, 400);

    // Keep the first twenty pages and a fragment of the twenty-first.
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..512 * 20 + 100]).unwrap();

    let found = recover::recover(&path).unwrap();
    assert!(
        found.rows.len() > 100 && found.rows.len() < 400,
        "what survived, not everything and not nothing: {}",
        found.rows.len()
    );
    assert!(
        found.notes.iter().any(|n| n.contains("partial")),
        "the partial last page is reported: {:?}",
        found.notes
    );
    // Rowids are contiguous from 1, which is what shows nothing was misdecoded.
    let mut ids: Vec<i64> = found.rows_for("t").filter_map(|r| r.rowid).collect();
    ids.sort_unstable();
    assert_eq!(ids.first(), Some(&1));
    assert_eq!(
        ids.last().copied().unwrap() as usize,
        ids.len(),
        "no gaps in what came back"
    );
    let _ = std::fs::remove_file(&path);
}

/// Values bigger than a page live on overflow pages, and a recovery that stops at
/// the page boundary would truncate them.
#[test]
fn recover_follows_overflow_pages() {
    let path = tmp("recover_overflow.db");
    let _ = std::fs::remove_file(&path);
    let big = "x".repeat(20_000);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA page_size=512; CREATE TABLE t (a TEXT, b BLOB, c REAL)")
        .unwrap();
    conn.execute(
        "INSERT INTO t VALUES (?1, ?2, ?3)",
        rusqlite::params![big, vec![0xabu8; 5000], 1.5f64],
    )
    .unwrap();
    drop(conn);

    let found = recover::recover(&path).unwrap();
    assert_eq!(found.rows.len(), 1, "{:?}", found.notes);
    let row = &found.rows[0];
    assert_eq!(
        row.values[0],
        recover::Value::Text(big),
        "a 20 KB text value spans overflow pages and must come back whole"
    );
    assert_eq!(row.values[1], recover::Value::Blob(vec![0xab; 5000]));
    assert_eq!(row.values[2], recover::Value::Real(1.5));
    let _ = std::fs::remove_file(&path);
}

/// A file with no readable schema at all: the rows still come back, as
/// lost_and_found, because that is better than nothing.
#[test]
fn recover_puts_unattributable_rows_in_lost_and_found() {
    let path = multipage_db("recover_lost.db", 60);
    // Wipe the schema b-tree but keep the 100-byte file header, which is what says
    // how big a page is — the shape of a damaged page 1 rather than a missing file.
    {
        let mut bytes = std::fs::read(&path).unwrap();
        for b in &mut bytes[100..512] {
            *b = 0;
        }
        std::fs::write(&path, bytes).unwrap();
    }
    let found = recover::recover(&path).unwrap();
    assert!(found.tables.is_empty(), "no schema survived");
    assert!(found.orphans() > 0, "but rows did: {:?}", found.notes);
    assert!(
        found.notes.iter().any(|n| n.contains("lost_and_found")),
        "{:?}",
        found.notes
    );

    let sql = recover::to_sql(&found);
    assert!(sql.contains("CREATE TABLE lost_and_found("), "{sql:.200}");
    let replay = tmp("recover_lost_replay.db");
    let _ = std::fs::remove_file(&replay);
    let conn = rusqlite::Connection::open(&replay).unwrap();
    conn.execute_batch(&sql)
        .expect("lost_and_found must replay");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM lost_and_found", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n as usize, found.orphans());
    // The page each row came from is recorded, which is what makes it auditable.
    let pages: i64 = conn
        .query_row("SELECT count(DISTINCT pgno) FROM lost_and_found", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(pages >= 1);
    for p in [path, replay] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn recover_refuses_what_is_not_a_database() {
    let path = tmp("recover_notdb.bin");
    // Long enough to have a header, so it is the magic that rejects it.
    std::fs::write(&path, "not a database, just text\n".repeat(10)).unwrap();
    let err = recover::recover(&path).unwrap_err().to_string();
    assert!(err.contains("not a SQLite database"), "{err}");

    std::fs::write(&path, b"short").unwrap();
    assert!(recover::recover(&path)
        .unwrap_err()
        .to_string()
        .contains("too short"));
    let _ = std::fs::remove_file(&path);
}

/// A `WITHOUT ROWID` table has no rowid to address a row by, so edits go through
/// its primary key — including a composite one. Before this it was listed
/// read-only.
#[test]
fn a_table_without_rowid_is_edited_by_its_primary_key() {
    let path = tmp("norowid.db");
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE kv (ns TEXT, k TEXT, v TEXT, PRIMARY KEY (ns, k)) WITHOUT ROWID;
         INSERT INTO kv VALUES ('a', 'one', 'first'), ('a', 'two', 'second'), ('b', 'one', 'other');",
    )
    .unwrap();
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();

    // The view reports the key columns in key order, and no rowids.
    let view = store.rows(&pq("kv", 10, 0, None, "")).unwrap();
    assert_eq!(view.primary_key, ["ns", "k"]);
    assert!(
        view.rowids.iter().all(Option::is_none),
        "a WITHOUT ROWID table exposes none"
    );
    assert_eq!(store.primary_key_columns("kv").unwrap(), ["ns", "k"]);

    // Both key columns are needed: ('a','two') must not touch ('b','one').
    let key = sqlite::RowKey::Primary(vec![
        ("ns".to_string(), "a".to_string()),
        ("k".to_string(), "two".to_string()),
    ]);
    assert_eq!(
        store.update_cell_keyed("kv", &key, "v", "edited").unwrap(),
        1,
        "exactly one row matches a full key"
    );
    let v = |ns: &str, k: &str| -> String {
        let view = store.rows(&pq("kv", 10, 0, None, "")).unwrap();
        let i = view
            .rows
            .iter()
            .position(|r| r[0] == ns && r[1] == k)
            .unwrap();
        view.rows[i][2].clone()
    };
    assert_eq!(v("a", "two"), "edited");
    assert_eq!(v("b", "one"), "other", "the other row is untouched");
    assert_eq!(v("a", "one"), "first");

    // Bytes go in the same way, and come back as bytes.
    store
        .update_cell_blob_keyed("kv", &key, "v", &[0x00, 0xff])
        .unwrap();
    assert_eq!(
        store.cell_bytes_keyed("kv", &key, "v").unwrap(),
        [0x00, 0xff]
    );
    assert!(store.cell_is_blob_keyed("kv", &key, "v").unwrap());

    // And delete addresses one row, not the whole namespace.
    assert_eq!(store.delete_row_keyed("kv", &key).unwrap(), 1);
    assert_eq!(store.count("kv").unwrap(), 2);
    assert_eq!(v("a", "one"), "first");

    // A key that matches nothing reports zero rather than erroring.
    let gone = sqlite::RowKey::Primary(vec![
        ("ns".to_string(), "zz".to_string()),
        ("k".to_string(), "nope".to_string()),
    ]);
    assert_eq!(store.update_cell_keyed("kv", &gone, "v", "x").unwrap(), 0);
    assert_eq!(store.delete_row_keyed("kv", &gone).unwrap(), 0);

    // An ordinary table still reports its rowids and no key columns.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE plain (a TEXT); INSERT INTO plain VALUES ('x')")
        .unwrap();
    drop(conn);
    let store = SqliteStore::open(&path).unwrap();
    let view = store.rows(&pq("plain", 10, 0, None, "")).unwrap();
    assert!(
        view.primary_key.is_empty(),
        "the rowid is the better handle"
    );
    assert_eq!(view.rowids[0], Some(1));
    let _ = std::fs::remove_file(&path);
}

/// A `WITHOUT ROWID` table keeps its rows in an index b-tree, so a recovery that
/// only read table-leaf pages could not bring back a single one of them.
#[test]
fn recover_reads_a_table_without_rowid() {
    let path = tmp("recover_norowid.db");
    let _ = std::fs::remove_file(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "PRAGMA page_size=512;
         CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID;",
    )
    .unwrap();
    for i in 0..200 {
        conn.execute(
            "INSERT INTO kv VALUES (?1, ?2)",
            [format!("key-{i:04}"), format!("value {i} with padding")],
        )
        .unwrap();
    }
    drop(conn);

    let found = recover::recover(&path).unwrap();
    assert_eq!(
        found.rows_for("kv").count(),
        200,
        "every keyed row comes back: {:?}",
        found.notes
    );
    assert!(
        found.rows_for("kv").all(|r| r.rowid.is_none()),
        "and none of them invents a rowid"
    );
    let one = found
        .rows_for("kv")
        .find(|r| r.values[0] == recover::Value::Text("key-0007".into()))
        .expect("a known key");
    assert_eq!(
        one.values[1],
        recover::Value::Text("value 7 with padding".into())
    );

    // The script must not name `_rowid_` for such a table, or the replay fails.
    let sql = recover::to_sql(&found);
    assert!(
        !sql.contains("_rowid_"),
        "a keyed table has no rowid column"
    );
    let replay = tmp("recover_norowid_replay.db");
    let _ = std::fs::remove_file(&replay);
    let conn = rusqlite::Connection::open(&replay).unwrap();
    conn.execute_batch(&sql).expect("the recovery must replay");
    let (n, first): (i64, String) = conn
        .query_row("SELECT count(*), min(k) FROM kv", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((n, first.as_str()), (200, "key-0000"));
    for p in [path, replay] {
        let _ = std::fs::remove_file(p);
    }
}
