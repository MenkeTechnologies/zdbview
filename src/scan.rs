//! Background filesystem scan for openable files.
//!
//! With no file argument, the picker lists the recent files *and* whatever the
//! scan finds, so a shard or database does not have to be located by hand first.
//!
//! The scan runs on its own thread and streams hits back over a channel, so the
//! picker is interactive immediately and fills in as results arrive (the event
//! loop already ticks every 250 ms).
//!
//! What counts as a hit is deliberately narrow — a scan that dumps every file it
//! walks is as useless as no scan:
//!
//! * the SQLite header magic — authoritative, so any real database qualifies;
//! * one of the rkyv shard magics from [`crate::formats`] — found anywhere in the
//!   sniffed window, since rkyv writes its root last and the header may sit at
//!   any offset;
//! * a `.rkyv` extension, which is a strong enough hint on its own for the
//!   header-less hash-keyed shards (pythonrs, rubylang, arb) that carry no magic.
//!
//! Anything else is dropped.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::store::Kind;

/// Depth used for a producer's own directory (`~/.zshrs` and friends) and for
/// the working directory: deep enough for `~/.zshrs/pkg/<plugin>/cache`.
pub const DEEP: usize = 5;
/// Give up after this long, so no filesystem can keep the scan running forever.
const TIME_BUDGET: Duration = Duration::from_secs(20);
/// Stop after examining this many directory entries, so a scan can never run
/// away on a huge tree.
const MAX_ENTRIES: usize = 60_000;
/// Stop after this many hits — well past a usable picker length.
const MAX_HITS: usize = 500;
/// How much of a file is read when looking for a magic.
const SNIFF: usize = 64 * 1024;
/// Files larger than this are only sniffed at their head and tail.
const BIG_FILE: u64 = 8 * 1024 * 1024;

/// Directory names never worth walking — they hold no caches or databases but
/// plenty of files.
const SKIP_DIRS: &[&str] = &[
    ".git",
    // Package-manager and toolchain caches: enormous, and none of it is ours.
    ".npm",
    ".pnpm-store",
    ".yarn",
    ".bun",
    ".deno",
    ".docker",
    ".gem",
    ".pyenv",
    ".nvm",
    ".rbenv",
    // Application data, not cache stores; reachable with --scan when wanted.
    "Library",
    "Applications",
    // Chromium/Electron profile stores: hundreds of SQLite files that are the
    // browser's business, not the user's. Matched case-sensitively, so a
    // producer's own lowercase `cache/` directory is still walked.
    "Cache",
    "Cache_Data",
    "Code Cache",
    "GPUCache",
    "GrShaderCache",
    "ShaderCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "IndexedDB",
    "Service Worker",
    "Local Storage",
    "Session Storage",
    "blob_storage",
    "component_crx_cache",
    "extensions_crx_cache",
    "Crashpad",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "build",
    "dist",
    ".rustup",
    ".cargo",
    ".venv",
    "venv",
    "__pycache__",
    ".Trash",
    "Trash",
    ".terraform",
    ".gradle",
    ".m2",
];

/// Extensions worth sniffing. Everything else is skipped without being read,
/// which is what keeps the scan cheap.
const CANDIDATE_EXTS: &[&str] = &[
    "db", "sqlite", "sqlite3", "sqlitedb", "rkyv", "bin", "cache", "shard", "dat", "zwc",
];

/// One place to look, and how far down.
#[derive(Debug, Clone)]
pub struct Root {
    pub path: PathBuf,
    /// Subdirectory levels to descend; `0` means this directory's files only.
    pub depth: usize,
}

impl Root {
    pub fn new(path: PathBuf, depth: usize) -> Self {
        Root { path, depth }
    }
}

/// A file the scan found.
#[derive(Debug, Clone)]
pub struct Hit {
    pub path: PathBuf,
    pub kind: Kind,
    /// Recognized rkyv format name, when a magic identified it.
    pub format: Option<&'static str>,
    pub size: u64,
    pub modified: SystemTime,
    /// Display priority, lowest first. A machine holds hundreds of unrelated
    /// SQLite files, so what the tool exists for comes first: a shard whose
    /// magic was recognized, then any other rkyv archive, then databases.
    pub rank: u8,
}

/// A running scan: hits arrive on `rx` until the thread finishes.
pub struct Scan {
    pub rx: Receiver<Hit>,
    /// Set to stop the walk early (on quit).
    cancel: Arc<AtomicBool>,
    /// `false` once the channel has closed, i.e. the walk is over.
    pub running: bool,
    /// Hits accepted so far, for the picker's status line.
    pub found: usize,
}

impl Scan {
    /// Drain whatever the thread has produced since the last call.
    pub fn drain(&mut self) -> Vec<Hit> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(hit) => out.push(hit),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.running = false;
                    break;
                }
            }
        }
        self.found += out.len();
        out
    }

    /// Ask the walk to stop; it checks between entries.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Where to look when no roots were given.
///
/// The producers in the format registry keep their stores in their own home
/// directory — `~/.zshrs/scripts.rkyv`, `~/.zshrs/compsys.db` and so on — so the
/// dot-directories of `$HOME` are the primary roots, most-recently-touched
/// first, followed by the XDG cache/data directories and the working directory.
/// `$HOME` itself contributes its own files but is not descended into, and
/// `~/Library` is left out (thousands of application-private databases); reach
/// those with `--scan`.
pub fn default_roots() -> Vec<Root> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut roots: Vec<Root> = Vec::new();

    // Producer homes: every `$HOME/.<name>` directory, newest activity first.
    if let Some(home) = &home {
        let mut dots: Vec<(SystemTime, PathBuf)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(home) {
            for e in entries.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                    continue;
                }
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let mtime = e
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    dots.push((mtime, e.path()));
                }
            }
        }
        dots.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
        roots.extend(dots.into_iter().map(|(_, p)| Root::new(p, DEEP)));
    }

    // XDG directories, in case they point outside `$HOME`.
    let xdg = |var: &str, fallback: &str| -> Option<PathBuf> {
        std::env::var_os(var)
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(fallback)))
    };
    roots.extend(xdg("XDG_CACHE_HOME", ".cache").map(|p| Root::new(p, DEEP)));
    roots.extend(xdg("XDG_DATA_HOME", ".local/share").map(|p| Root::new(p, DEEP)));

    if let Ok(cwd) = std::env::current_dir() {
        roots.push(Root::new(cwd, DEEP));
    }
    // `$HOME` last, files only: a stray `~/foo.db` counts, the whole home tree
    // does not.
    roots.extend(home.map(|h| Root::new(h, 0)));

    roots.retain(|r| r.path.is_dir());
    // Drop roots already covered by an earlier (equal-or-deeper) one.
    let mut kept: Vec<Root> = Vec::new();
    for mut r in roots {
        r.path = r.path.canonicalize().unwrap_or(r.path);
        if !kept
            .iter()
            .any(|k| r.path.starts_with(&k.path) && k.depth >= r.depth)
        {
            kept.push(r);
        }
    }
    kept
}

/// Start scanning `roots` on a background thread.
pub fn spawn(roots: Vec<Root>) -> Scan {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    std::thread::spawn(move || {
        let mut budget = Budget {
            examined: 0,
            hits: 0,
            deadline: Instant::now() + TIME_BUDGET,
        };
        for root in roots {
            if walk(&root.path, 0, root.depth, &tx, &flag, &mut budget).is_break() {
                break;
            }
        }
    });
    Scan {
        rx,
        cancel,
        running: true,
        found: 0,
    }
}

use std::ops::ControlFlow;

/// The limits that stop a walk: entries seen, hits produced, wall clock.
struct Budget {
    examined: usize,
    hits: usize,
    deadline: Instant,
}

impl Budget {
    fn spent(&self) -> bool {
        self.examined > MAX_ENTRIES || self.hits >= MAX_HITS || Instant::now() > self.deadline
    }
}

fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    tx: &mpsc::Sender<Hit>,
    cancel: &AtomicBool,
    budget: &mut Budget,
) -> ControlFlow<()> {
    if depth > max_depth || cancel.load(Ordering::Relaxed) {
        return ControlFlow::Continue(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // Unreadable directories are normal (permissions); just move on.
        Err(_) => return ControlFlow::Continue(()),
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        budget.examined += 1;
        if budget.spent() || cancel.load(Ordering::Relaxed) {
            return ControlFlow::Break(());
        }
        let path = entry.path();
        // `file_type` comes from the directory entry, so it costs no extra stat
        // and does not follow symlinks — which is also how directory cycles are
        // avoided.
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !SKIP_DIRS.contains(&name.as_ref()) {
                subdirs.push(path);
            }
        } else if ft.is_file() {
            if let Some(hit) = classify(&path) {
                if tx.send(hit).is_err() {
                    // The picker closed.
                    return ControlFlow::Break(());
                }
                budget.hits += 1;
            }
        }
    }
    for sub in subdirs {
        walk(&sub, depth + 1, max_depth, tx, cancel, budget)?;
    }
    ControlFlow::Continue(())
}

/// Decide whether `path` is openable, reading as little as possible.
fn classify(path: &Path) -> Option<Hit> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let is_candidate = match &ext {
        Some(e) => CANDIDATE_EXTS.contains(&e.as_str()),
        // Extension-less files are worth a look only inside a cache directory
        // (the producers write some shards without a suffix).
        None => path
            .parent()
            .and_then(|p| p.to_str())
            .is_some_and(|p| p.contains("cache")),
    };
    if !is_candidate {
        return None;
    }
    let meta = path.metadata().ok()?;
    if meta.len() == 0 {
        return None;
    }
    let (kind, format) = sniff(path, meta.len(), ext.as_deref())?;
    let rank = match (kind, format) {
        (Kind::Rkyv, Some(_)) => 0,
        (Kind::Rkyv, None) => 1,
        (Kind::Sqlite, _) => 2,
    };
    Some(Hit {
        path: path.to_path_buf(),
        kind,
        format,
        size: meta.len(),
        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        rank,
    })
}

/// Classify by content: the SQLite header magic, else a known rkyv shard magic
/// in the sniffed window, else the `.rkyv` extension as a last resort.
fn sniff(path: &Path, len: u64, ext: Option<&str>) -> Option<(Kind, Option<&'static str>)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; SNIFF.min(len as usize)];
    let n = f.read(&mut head).ok()?;
    head.truncate(n);

    if crate::store::is_sqlite_header(&head) {
        return Some((Kind::Sqlite, None));
    }
    if let Some(name) = crate::formats::magic_in(&head) {
        return Some((Kind::Rkyv, Some(name)));
    }
    // rkyv serializes its root last, so a big shard's header can sit past the
    // head window; check the tail before giving up.
    if len > BIG_FILE {
        let mut tail = vec![0u8; SNIFF];
        if f.seek(SeekFrom::End(-(SNIFF as i64))).is_ok() {
            let n = f.read(&mut tail).ok()?;
            tail.truncate(n);
            if let Some(name) = crate::formats::magic_in(&tail) {
                return Some((Kind::Rkyv, Some(name)));
            }
        }
    }
    // The header-less hash-keyed shards carry no magic; trust `.rkyv`.
    (ext == Some("rkyv")).then_some((Kind::Rkyv, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("zdbview_scan_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    fn collect(root: &Path) -> Vec<Hit> {
        let scan = spawn(vec![Root::new(root.to_path_buf(), DEEP)]);
        // The walk is short; wait for the channel to close.
        let mut out = Vec::new();
        while let Ok(hit) = scan.rx.recv() {
            out.push(hit);
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    fn sqlite_bytes() -> Vec<u8> {
        let mut v = b"SQLite format 3\0".to_vec();
        v.extend(std::iter::repeat_n(0u8, 100));
        v
    }

    #[test]
    fn finds_sqlite_and_rkyv_and_ignores_the_rest() {
        let root = tmpdir("mixed");
        write(&root.join("real.db"), &sqlite_bytes());
        // A `.db` file that is not SQLite but is a recognized shard.
        write(
            &root.join("plugins.db"),
            &crate::formats::test_script_shard_bytes("/tmp/x.sh", b"blob"),
        );
        // A magic-less shard, accepted on its extension alone.
        write(&root.join("hashes.rkyv"), b"not a magic but named rkyv");
        // Noise that must not show up.
        write(&root.join("notes.txt"), b"SQLite format 3\0 in a text file");
        write(&root.join("empty.db"), b"");
        write(&root.join("script.sh"), b"#!/bin/sh\necho hi\n");

        let hits = collect(&root);
        let names: Vec<String> = hits
            .iter()
            .map(|h| h.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["hashes.rkyv", "plugins.db", "real.db"],
            "only openable files, and no empty or wrong-extension ones"
        );

        let by_name = |n: &str| hits.iter().find(|h| h.path.ends_with(n)).unwrap();
        assert_eq!(by_name("real.db").kind, Kind::Sqlite);
        assert!(by_name("real.db").format.is_none());
        // The magic wins over the misleading `.db` name.
        assert_eq!(by_name("plugins.db").kind, Kind::Rkyv);
        assert_eq!(
            by_name("plugins.db").format,
            Some("zshrs script cache (ZRSC)")
        );
        assert_eq!(by_name("hashes.rkyv").kind, Kind::Rkyv);
        assert!(by_name("hashes.rkyv").format.is_none());
        assert!(by_name("real.db").size > 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walks_into_subdirectories_but_skips_noise_dirs() {
        let root = tmpdir("nested");
        write(&root.join("a/b/c/deep.db"), &sqlite_bytes());
        write(&root.join("node_modules/pkg/pkg.db"), &sqlite_bytes());
        write(&root.join("target/debug/build.db"), &sqlite_bytes());
        write(&root.join(".git/index.db"), &sqlite_bytes());

        let hits = collect(&root);
        assert_eq!(hits.len(), 1, "got {:?}", hits);
        assert!(hits[0].path.ends_with("a/b/c/deep.db"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Depth is bounded, so a pathological tree cannot stall the picker.
    #[test]
    fn stops_at_the_depth_limit() {
        let root = tmpdir("deep");
        let mut p = root.clone();
        for i in 0..(DEEP + 3) {
            p = p.join(format!("d{i}"));
        }
        write(&p.join("too_deep.db"), &sqlite_bytes());
        write(&root.join("shallow.db"), &sqlite_bytes());

        let hits = collect(&root);
        let names: Vec<_> = hits.iter().map(|h| h.path.clone()).collect();
        assert!(names.iter().any(|p| p.ends_with("shallow.db")));
        assert!(
            !names.iter().any(|p| p.ends_with("too_deep.db")),
            "the depth limit must hold: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Extension-less files count only under a cache directory, where the
    /// producers write shards without a suffix.
    #[test]
    fn extensionless_files_only_count_inside_a_cache_dir() {
        let root = tmpdir("extless");
        write(&root.join("cache/shard0"), &sqlite_bytes());
        write(&root.join("elsewhere/shard0"), &sqlite_bytes());

        let hits = collect(&root);
        assert_eq!(hits.len(), 1, "got {:?}", hits);
        assert!(hits[0].path.ends_with("cache/shard0"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_stops_the_walk() {
        let root = tmpdir("cancel");
        for i in 0..50 {
            write(&root.join(format!("f{i}.db")), &sqlite_bytes());
        }
        let scan = spawn(vec![Root::new(root.clone(), DEEP)]);
        scan.cancel();
        // Draining after cancelling must terminate rather than hang.
        let mut scan = scan;
        while scan.running {
            scan.drain();
            if scan
                .rx
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err()
            {
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The producers keep their stores in their own dot-directory, so those must
    /// be roots, and none of the roots may be redundant.
    #[test]
    fn default_roots_cover_home_dot_dirs_without_redundancy() {
        let roots = default_roots();
        for r in &roots {
            assert!(r.path.is_dir(), "{} is not a directory", r.path.display());
        }
        // No root is contained in an earlier one that already goes as deep.
        for (i, a) in roots.iter().enumerate() {
            for b in roots.iter().take(i) {
                assert!(
                    !(a.path.starts_with(&b.path) && b.depth >= a.depth),
                    "{} is already covered by {}",
                    a.path.display(),
                    b.path.display()
                );
            }
        }
        // Every existing `$HOME/.<dir>` that is not on the skip list is a root.
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let canon_roots: Vec<PathBuf> = roots.iter().map(|r| r.path.clone()).collect();
            for e in std::fs::read_dir(&home).into_iter().flatten().flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy().into_owned();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if !is_dir || !name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                let path = e.path().canonicalize().unwrap_or_else(|_| e.path());
                assert!(
                    canon_roots.iter().any(|r| path.starts_with(r)),
                    "{} is not covered by any root",
                    path.display()
                );
            }
            // `$HOME` itself is present but files-only, so the whole home tree is
            // never walked.
            let home_c = home.canonicalize().unwrap_or(home);
            assert!(
                roots.iter().any(|r| r.path == home_c && r.depth == 0),
                "expected a files-only $HOME root"
            );
        }
    }

    /// A files-only root must not descend at all.
    #[test]
    fn depth_zero_root_reads_only_its_own_files() {
        let root = tmpdir("depth0");
        write(&root.join("top.db"), &sqlite_bytes());
        write(&root.join("sub/inner.db"), &sqlite_bytes());
        let scan = spawn(vec![Root::new(root.clone(), 0)]);
        let mut hits = Vec::new();
        while let Ok(h) = scan.rx.recv() {
            hits.push(h);
        }
        assert_eq!(hits.len(), 1, "got {hits:?}");
        assert!(hits[0].path.ends_with("top.db"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
