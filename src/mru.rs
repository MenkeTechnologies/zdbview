//! Most-recently-used file tracking.
//!
//! Every successful open is recorded to `$XDG_CACHE_HOME/zdbview/recent`
//! (falling back to `~/.cache/zdbview/recent`), most-recent-first, deduped by
//! absolute path and capped. Running `zdbview` with no argument shows this list
//! as a picker.

use crate::store::Kind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Maximum number of remembered files.
const CAP: usize = 50;

pub struct Entry {
    pub path: PathBuf,
    pub kind: Kind,
    pub opened: SystemTime,
}

/// Path of the MRU store file.
fn store_file() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"));
    base.join("zdbview").join("recent")
}

fn kind_str(k: Kind) -> &'static str {
    match k {
        Kind::Sqlite => "sqlite",
        Kind::Rkyv => "rkyv",
    }
}

fn parse_kind(s: &str) -> Option<Kind> {
    match s {
        "sqlite" => Some(Kind::Sqlite),
        "rkyv" => Some(Kind::Rkyv),
        _ => None,
    }
}

/// Load the MRU list (most-recent-first) from the default location.
pub fn load() -> Vec<Entry> {
    load_path(&store_file())
}

/// Record `path` as the most-recently-used file at the default location.
pub fn record(path: &Path, kind: Kind) {
    record_path(&store_file(), path, kind);
}

pub(crate) fn load_path(file: &Path) -> Vec<Entry> {
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let mut it = line.splitn(3, '\t');
        let (Some(ts), Some(kind), Some(path)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let Ok(secs) = ts.parse::<u64>() else {
            continue;
        };
        let Some(kind) = parse_kind(kind) else {
            continue;
        };
        out.push(Entry {
            path: PathBuf::from(path),
            kind,
            opened: UNIX_EPOCH + Duration::from_secs(secs),
        });
    }
    out
}

pub(crate) fn record_path(file: &Path, path: &Path, kind: Kind) {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut entries = load_path(file);
    entries.retain(|e| e.path != abs);
    entries.insert(
        0,
        Entry {
            path: abs,
            kind,
            opened: UNIX_EPOCH + Duration::from_secs(now),
        },
    );
    entries.truncate(CAP);

    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut buf = String::new();
    for e in &entries {
        let secs = e
            .opened
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        buf.push_str(&format!(
            "{}\t{}\t{}\n",
            secs,
            kind_str(e.kind),
            e.path.display()
        ));
    }
    // Write to a temp file then rename so a concurrent reader never sees a
    // half-written list.
    let tmp = file.with_extension("tmp");
    if std::fs::write(&tmp, &buf).is_ok() {
        let _ = std::fs::rename(&tmp, file);
    }
}

/// Human-readable age like "3m ago".
pub fn rel_age(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::{load_path, record_path, rel_age, CAP};
    use crate::store::Kind;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn scratch(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("zdbview_mru_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn the_list_stops_at_the_cap_and_drops_the_oldest() {
        let store = scratch("cap");
        let dir = scratch("cap_dir");
        std::fs::create_dir_all(&dir).unwrap();
        // One more file than the list holds, recorded oldest-first.
        let files: Vec<PathBuf> = (0..CAP + 5)
            .map(|i| {
                let p = dir.join(format!("f{i}.db"));
                std::fs::write(&p, b"x").unwrap();
                record_path(&store, &p, Kind::Sqlite);
                p
            })
            .collect();

        let entries = load_path(&store);
        assert_eq!(entries.len(), CAP, "the cap is the cap");
        // Most-recent-first, so the newest is at the front and the first five
        // recorded are gone.
        let newest = std::fs::canonicalize(files.last().unwrap()).unwrap();
        assert_eq!(entries[0].path, newest);
        let oldest_kept = std::fs::canonicalize(&files[5]).unwrap();
        assert_eq!(entries[CAP - 1].path, oldest_kept);
        assert!(
            !entries
                .iter()
                .any(|e| e.path == std::fs::canonicalize(&files[4]).unwrap()),
            "what fell off the end is gone"
        );

        let _ = std::fs::remove_file(&store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_line_costs_only_itself() {
        let store = scratch("damaged");
        std::fs::write(
            &store,
            "not a record\n\
             xyz\tsqlite\t/tmp/bad-timestamp.db\n\
             12\tmysql\t/tmp/unknown-kind.db\n\
             34\tsqlite\n\
             56\trkyv\t/tmp/good.rkyv\n",
        )
        .unwrap();

        let entries = load_path(&store);
        assert_eq!(entries.len(), 1, "only the well-formed line survives");
        assert_eq!(entries[0].path, PathBuf::from("/tmp/good.rkyv"));
        assert_eq!(entries[0].kind, Kind::Rkyv);
        let _ = std::fs::remove_file(&store);
    }

    #[test]
    fn a_path_holding_a_tab_is_read_back_whole() {
        // The line is three tab-separated fields, so a tab in the path — legal
        // on every platform zdbview runs on — must not split it further.
        let store = scratch("tabbed");
        std::fs::write(&store, "7\tsqlite\t/tmp/od\td\tname.db\n").unwrap();
        let entries = load_path(&store);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("/tmp/od\td\tname.db"));
        let _ = std::fs::remove_file(&store);
    }

    #[test]
    fn a_missing_list_is_empty_rather_than_an_error() {
        let store = scratch("absent");
        assert!(load_path(&store).is_empty());
    }

    #[test]
    fn ages_read_in_the_largest_unit_that_fits() {
        let ago = |secs: u64| rel_age(SystemTime::now() - Duration::from_secs(secs));
        assert_eq!(ago(0), "0s ago");
        assert_eq!(ago(59), "59s ago");
        assert_eq!(ago(60), "1m ago");
        assert_eq!(ago(3599), "59m ago");
        assert_eq!(ago(3600), "1h ago");
        assert_eq!(ago(86_399), "23h ago");
        assert_eq!(ago(86_400), "1d ago");
        // A timestamp in the future is not a negative age.
        assert_eq!(rel_age(SystemTime::now() + Duration::from_secs(600)), "0s ago");
    }
}
