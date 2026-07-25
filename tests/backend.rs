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
