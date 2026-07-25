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

use rkyv_inspect::RkyvStore;
use sqlite::{Sort, SqliteStore};
use store::{detect, Kind};

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

    let view = store.rows("items", 100, 0, None, "").unwrap();
    assert_eq!(view.total, 2);
    assert_eq!(view.rows.len(), 2);
    assert_eq!(view.rows[0], vec!["a".to_string(), "1".to_string()]);
    let rowid_a = view.rowids[0].expect("rowid present");

    // UPDATE
    store.update_cell("items", rowid_a, "qty", "42").unwrap();
    let view = store.rows("items", 100, 0, None, "").unwrap();
    assert_eq!(view.rows[0], vec!["a".to_string(), "42".to_string()]);

    // INSERT (default values)
    store.insert_blank("items").unwrap();
    assert_eq!(store.count("items").unwrap(), 3);

    // DELETE
    store.delete_row("items", rowid_a).unwrap();
    assert_eq!(store.count("items").unwrap(), 2);
    let view = store.rows("items", 100, 0, None, "").unwrap();
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
    let v = store.rows("t", 100, 0, None, "").unwrap();
    assert_eq!(col(&v, 0), ["pear", "apple", "fig", "date"]);

    let asc = Sort {
        column: "name".into(),
        desc: false,
    };
    let v = store.rows("t", 100, 0, Some(&asc), "").unwrap();
    assert_eq!(col(&v, 0), ["apple", "date", "fig", "pear"]);

    let desc = Sort {
        column: "name".into(),
        desc: true,
    };
    let v = store.rows("t", 100, 0, Some(&desc), "").unwrap();
    assert_eq!(col(&v, 0), ["pear", "fig", "date", "apple"]);

    // Numeric column must sort numerically, not lexically (10 after 7).
    let qty = Sort {
        column: "qty".into(),
        desc: false,
    };
    let v = store.rows("t", 100, 0, Some(&qty), "").unwrap();
    assert_eq!(col(&v, 1), ["3", "3", "7", "10"]);

    // An unknown column falls back to rowid order instead of failing the query.
    let bogus = Sort {
        column: "nope".into(),
        desc: false,
    };
    let v = store.rows("t", 100, 0, Some(&bogus), "").unwrap();
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
        let page = store.rows("t", 2, offset, Some(&qty), "").unwrap();
        assert_eq!(page.rows.len(), 2);
        seen.extend(col(&page, 0));
    }
    let full = col(&store.rows("t", 100, 0, Some(&qty), "").unwrap(), 0);
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
    let sorted = store.rows("t", 100, 0, Some(&asc), "").unwrap();
    let rowid_of = |n: &str| -> i64 {
        let i = sorted.rows.iter().position(|r| r[0] == n).unwrap();
        sorted.rowids[i].unwrap()
    };

    // Every row matches "e"? No — apple, date, pear do. From apple, forward is
    // date (next in sorted order), not fig or the next rowid.
    let next = store
        .find_row("t", &cols, "e", rowid_of("apple"), true, Some(&asc))
        .unwrap();
    assert_eq!(next, Some(rowid_of("date")));

    // Backward from pear is date as well.
    let prev = store
        .find_row("t", &cols, "e", rowid_of("pear"), false, Some(&asc))
        .unwrap();
    assert_eq!(prev, Some(rowid_of("date")));

    // Nothing after pear: the caller wraps via the edge query, which returns the
    // first match in display order.
    assert_eq!(
        store
            .find_row("t", &cols, "e", rowid_of("pear"), true, Some(&asc))
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .find_row_edge("t", &cols, "e", true, Some(&asc))
            .unwrap(),
        Some(rowid_of("apple"))
    );
    assert_eq!(
        store
            .find_row_edge("t", &cols, "e", false, Some(&asc))
            .unwrap(),
        Some(rowid_of("pear"))
    );

    // Ordinals are positions in the sorted view: apple is 1st, pear 4th.
    assert_eq!(
        store
            .rowid_ordinal("t", rowid_of("apple"), Some(&asc))
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .rowid_ordinal("t", rowid_of("pear"), Some(&asc))
            .unwrap(),
        4
    );
    // Without a sort the same rowid is placed by rowid instead.
    assert_eq!(store.rowid_ordinal("t", rowid_of("pear"), None).unwrap(), 1);

    // Descending flips both the stepping direction and the ordinals.
    let desc = Sort {
        column: "name".into(),
        desc: true,
    };
    assert_eq!(
        store
            .find_row("t", &cols, "e", rowid_of("pear"), true, Some(&desc))
            .unwrap(),
        Some(rowid_of("date"))
    );
    assert_eq!(
        store
            .rowid_ordinal("t", rowid_of("pear"), Some(&desc))
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
    let v = store.rows("t", 100, 0, Some(&sort), "").unwrap();
    assert_eq!(col(&v, 0), ["a", "b"]);
    assert_eq!(store.rowid_ordinal("t", 2, Some(&sort)).unwrap(), 1);
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
    let rows = copy.rows("child", 10, 0, None, "").unwrap();
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
/// through `update_cell` would store the description of the bytes instead.
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

    assert!(store.cell_is_blob("t", 1, "raw").unwrap());
    assert!(
        !store.cell_is_blob("t", 1, "txt").unwrap(),
        "text stays with the line editor"
    );
    assert_eq!(store.cell_bytes("t", 1, "raw").unwrap(), [0x00, 0xff, 0x41]);

    store
        .update_cell_blob("t", 1, "raw", &[0xde, 0xad, 0xbe, 0xef])
        .unwrap();
    assert_eq!(
        store.cell_bytes("t", 1, "raw").unwrap(),
        [0xde, 0xad, 0xbe, 0xef]
    );
    assert!(
        store.cell_is_blob("t", 1, "raw").unwrap(),
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
    let view = store.rows("p", 10, 0, None, "").unwrap();
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
