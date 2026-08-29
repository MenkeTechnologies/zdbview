//! File-kind detection and the top-level store enum.

use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

/// Which backend a file is opened with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Sqlite,
    Rkyv,
}

/// The SQLite file header magic (first 16 bytes of every SQLite database).
const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

/// Whether `head` starts with the SQLite header magic. The scanner uses this so
/// there is one definition of "is a SQLite file" in the crate.
pub fn is_sqlite_header(head: &[u8]) -> bool {
    head.len() >= SQLITE_MAGIC.len() && &head[..SQLITE_MAGIC.len()] == SQLITE_MAGIC
}

/// Decide how to open `path`. Explicit `--sqlite`/`--rkyv` win; otherwise the
/// SQLite header magic is authoritative — its presence means SQLite, its
/// absence in a readable header means NOT SQLite (extension is ignored, because
/// e.g. zshrs stores rkyv shards under a `.db` name). The extension is only a
/// tie-breaker when the file is too short to carry a header.
pub fn detect(path: &Path, force_sqlite: bool, force_rkyv: bool) -> Result<Kind> {
    if force_sqlite {
        return Ok(Kind::Sqlite);
    }
    if force_rkyv {
        return Ok(Kind::Rkyv);
    }

    let mut buf = [0u8; 16];
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let n = f.read(&mut buf)?;

    if n >= SQLITE_MAGIC.len() {
        // Full header available: magic is the sole authority.
        return Ok(if is_sqlite_header(&buf) {
            Kind::Sqlite
        } else {
            Kind::Rkyv
        });
    }

    // Too short for a SQLite header — fall back to the extension hint.
    match path.extension().and_then(|e| e.to_str()) {
        Some("db") | Some("sqlite") | Some("sqlite3") => Ok(Kind::Sqlite),
        _ => Ok(Kind::Rkyv),
    }
}

/// The opened backend, holding whichever store was selected.
pub enum Store {
    Sqlite(crate::sqlite::SqliteStore),
    Rkyv(crate::rkyv_inspect::RkyvStore),
}

impl Store {
    /// Open `path` with the detected `kind`. Returns the store and the kind
    /// actually used: if a SQLite open fails (the file only looked like a
    /// database), it falls back to the rkyv/binary inspector rather than error.
    pub fn open(path: &Path, kind: Kind) -> Result<(Self, Kind)> {
        match kind {
            Kind::Sqlite => match crate::sqlite::SqliteStore::open(path) {
                Ok(s) => Ok((Store::Sqlite(s), Kind::Sqlite)),
                Err(_) => {
                    let r = crate::rkyv_inspect::RkyvStore::open(path)?;
                    Ok((Store::Rkyv(r), Kind::Rkyv))
                }
            },
            Kind::Rkyv => {
                let r = crate::rkyv_inspect::RkyvStore::open(path)?;
                Ok((Store::Rkyv(r), Kind::Rkyv))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{detect, is_sqlite_header, Kind};
    use std::path::PathBuf;

    fn scratch(name: &str, bytes: &[u8]) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("zdbview_store_{}_{}", std::process::id(), name));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn the_header_outranks_the_file_name() {
        // zshrs writes rkyv shards under `.db`, and SQLite databases turn up
        // under every extension there is; only the first 16 bytes decide.
        let shard = scratch("shard.db", b"not sqlite, but long enough for a header");
        assert_eq!(detect(&shard, false, false).unwrap(), Kind::Rkyv);

        let mut db = b"SQLite format 3\0".to_vec();
        db.extend(std::iter::repeat_n(0u8, 64));
        let db = scratch("db.rkyv", &db);
        assert_eq!(detect(&db, false, false).unwrap(), Kind::Sqlite);

        for p in [&shard, &db] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn a_forced_backend_is_not_second_guessed() {
        // --sqlite and --rkyv exist for files the header cannot speak for, so
        // they win without the file even being read: this path does not exist.
        let missing = std::env::temp_dir().join("zdbview_store_no_such_file");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(detect(&missing, true, false).unwrap(), Kind::Sqlite);
        assert_eq!(detect(&missing, false, true).unwrap(), Kind::Rkyv);
        // Without a force, the same path is an error naming what failed.
        let err = detect(&missing, false, false).unwrap_err().to_string();
        assert!(err.contains("open"), "{err}");
        assert!(err.contains("zdbview_store_no_such_file"), "{err}");
    }

    #[test]
    fn a_file_too_short_for_a_header_falls_back_to_the_extension() {
        let short_db = scratch("tiny.db", b"x");
        let short_other = scratch("tiny.rkyv", b"x");
        assert_eq!(detect(&short_db, false, false).unwrap(), Kind::Sqlite);
        assert_eq!(detect(&short_other, false, false).unwrap(), Kind::Rkyv);
        // An empty file has no header either, and the same rule applies.
        let empty = scratch("empty.sqlite3", b"");
        assert_eq!(detect(&empty, false, false).unwrap(), Kind::Sqlite);

        for p in [&short_db, &short_other, &empty] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn the_magic_is_matched_whole() {
        assert!(is_sqlite_header(b"SQLite format 3\0trailing bytes"));
        // The NUL is part of it, and a truncated magic is not a match.
        assert!(!is_sqlite_header(b"SQLite format 3 "));
        assert!(!is_sqlite_header(b"SQLite format 3"));
        assert!(!is_sqlite_header(b""));
    }
}
