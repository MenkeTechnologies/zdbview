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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::store::Kind;

/// Depth used for a producer's own directory (`~/.zshrs` and friends) and for
/// the working directory: deep enough for `~/.zshrs/pkg/<plugin>/cache`.
pub const DEEP: usize = 5;
/// Depth for the whole-filesystem root: what stops the walk there is the shape
/// of the filesystem — the skip lists and the device boundary — not a level
/// count, since a database can sit at any depth.
pub const UNLIMITED: usize = usize::MAX;
/// Hang backstop, not a budget: nothing may keep the walk running forever, but a
/// full walk of `/` takes minutes and is meant to finish. It is affordable
/// because the result is kept until the filesystem invalidates it (see
/// [`Cached::refresh_roots`]) rather than re-walked on a clock.
const TIME_BUDGET: Duration = Duration::from_secs(10 * 60);
/// Backstop for a pathological tree, above what a whole-disk walk costs (a warm
/// macOS root is a few million entries, and one walks ~90k entries per second).
const MAX_ENTRIES: usize = 20_000_000;
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
    // Not walked as part of a wider tree: `~/Library` is thousands of
    // application-private files, and `Applications` is program code. The one
    // part of `~/Library` that holds databases a user cares about is
    // `Application Support`, which `default_roots` names explicitly — a root is
    // never matched against this list, only the directories below it.
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

/// Absolute directories never descended into once the walk reaches `/`. These
/// are not "noise" like [`SKIP_DIRS`] — each one would break the walk outright:
///
/// * `/System/Volumes` holds the mount point of the data volume itself
///   (`/System/Volumes/Data`), which is the same filesystem already reached
///   through `/Users`; descending it walks the entire disk a second time under
///   a second set of paths. `/Volumes` is the same for external and network
///   disks, which the device check below also refuses.
/// * `/net` and `/home` are automounter triggers: reading them mounts something.
/// * `/dev`, `/proc`, `/sys` and `/run` are kernel-synthesized, and
///   `/private/var/vm` is swap — files there are not databases in any sense.
/// * `/private/var/folders` is the per-user temporary store: thousands of
///   short-lived SQLite files that are gone before the picker could open them.
const SKIP_PATHS: &[&str] = &[
    "/System/Volumes",
    "/Volumes",
    "/net",
    "/home",
    "/cores",
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/private/var/vm",
    "/private/var/folders",
    "/.Spotlight-V100",
    "/.fseventsd",
    "/.DocumentRevisions-V100",
];

/// Extensions worth sniffing. Everything else is skipped without being read,
/// which is what keeps the scan cheap.
const CANDIDATE_EXTS: &[&str] = &[
    "db", "sqlite", "sqlite3", "sqlitedb", "rkyv", "bin", "cache", "shard", "dat", "zwc",
];

/// Format marker for the cache file, bumped if its layout changes. Version 2
/// added the completeness flag and the watched-directory rows that replaced the
/// old 24-hour expiry.
const CACHE_VERSION: u32 = 2;

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

/// A previously saved scan, loaded from appdata.
pub struct Cached {
    pub hits: Vec<Hit>,
    pub scanned_at: SystemTime,
    /// Whether the walk that wrote this ran to completion. A walk cut short by
    /// quitting the picker saves what it had with this clear, so the work is not
    /// thrown away and the next start still knows the index is partial.
    pub complete: bool,
    /// Every directory that held a hit, plus the roots, with the mtime it had
    /// when the scan was saved. A directory's mtime moves when an entry is
    /// added, removed or renamed in it, which is exactly what invalidates the
    /// list of files found there.
    dirs: Vec<(PathBuf, SystemTime)>,
    /// Directories of hits whose file has gone away. The listing was dropped on
    /// load; the directory still has to be looked at again.
    gone: Vec<PathBuf>,
}

impl Cached {
    pub fn age(&self) -> Duration {
        SystemTime::now()
            .duration_since(self.scanned_at)
            .unwrap_or_default()
    }

    /// What has to be walked to bring this index up to date, given the roots a
    /// full walk would use.
    ///
    /// There is deliberately no clock in this: an index of a disk nobody touched
    /// is as good a year later as it was the day it was written, and re-walking
    /// `/` on a timer is work that answers a question already answered. Empty
    /// means no walk at all.
    ///
    /// The one thing it cannot see is a database created in a directory that
    /// held none before, since no directory it recorded changed. `r` in the
    /// picker and `--rescan` exist for that.
    pub fn refresh_roots(&self, full: &[Root]) -> Vec<Root> {
        if !self.complete {
            // A walk that never finished never described the whole disk.
            return full.to_vec();
        }
        self.changed_dirs()
            .into_iter()
            .map(|p| Root::new(p, DEEP))
            .collect()
    }

    /// The watched directories that have changed since they were read.
    ///
    /// This is what a refresh walks, rather than the whole disk again. A machine
    /// that is being worked on always has *something* changing — a cache
    /// directory written a minute ago — and re-walking `/` for it would mean
    /// walking `/` on every start, which is the behaviour keeping the index was
    /// meant to remove. What changed is a handful of directories; what did not
    /// is still described correctly by the index, so it is left alone.
    pub fn changed_dirs(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .dirs
            .iter()
            .filter(|(p, t)| dir_changed(p, *t))
            .map(|(p, _)| p.clone())
            .collect();
        out.extend(self.gone.iter().cloned());
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Whether `dir` has changed since it was read at `when`. An unreadable or
/// missing directory counts as changed: whatever the index said about it can no
/// longer be trusted.
fn dir_changed(dir: &Path, when: SystemTime) -> bool {
    // Whole seconds on both sides: the index stores seconds, and comparing a
    // full-precision mtime against a truncated one reports every directory as
    // changed the moment it is written.
    let secs = |t: SystemTime| {
        t.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    };
    match std::fs::metadata(dir).and_then(|m| m.modified()) {
        Ok(now) => secs(now) > secs(when),
        Err(_) => true,
    }
}

/// `$XDG_CACHE_HOME/zdbview/scan`, beside the recent-files list.
fn cache_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("zdbview").join("scan"))
}

/// Load the saved scan, dropping entries whose file has since gone away. A
/// missing, unreadable or foreign-version file simply yields `None`, which makes
/// the caller walk the filesystem instead.
pub fn load_cache() -> Option<Cached> {
    load_cache_from(&cache_file()?)
}

pub(crate) fn load_cache_from(path: &Path) -> Option<Cached> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    // Header: `# zdbview scan <version> <unix-seconds> <complete>`
    let header: Vec<&str> = lines.next()?.split_whitespace().collect();
    if header.len() != 6 || header[1] != "zdbview" || header[2] != "scan" {
        return None;
    }
    if header[3].parse::<u32>().ok()? != CACHE_VERSION {
        return None;
    }
    let secs: u64 = header[4].parse().ok()?;
    let scanned_at = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
    let complete = header[5] == "1";

    let mut hits = Vec::new();
    let mut dirs = Vec::new();
    let mut gone = Vec::new();
    for line in lines {
        let f: Vec<&str> = line.split('\t').collect();
        match f.first() {
            // A watched directory: `d <mtime> <path>`.
            Some(&"d") if f.len() == 3 => {
                let when = SystemTime::UNIX_EPOCH + Duration::from_secs(f[1].parse().unwrap_or(0));
                dirs.push((PathBuf::from(f[2]), when));
            }
            // A file: `f <kind> <rank> <size> <mtime> <format> <path>`.
            Some(&"f") if f.len() == 7 => {
                let kind = match f[1] {
                    "sqlite" => Kind::Sqlite,
                    "rkyv" => Kind::Rkyv,
                    _ => continue,
                };
                let path = PathBuf::from(f[6]);
                // A cached path that no longer resolves is not offered, and its
                // directory is one the index no longer describes.
                if !path.is_file() {
                    gone.extend(path.parent().map(Path::to_path_buf));
                    continue;
                }
                hits.push(Hit {
                    path,
                    kind,
                    format: crate::formats::magic_label(f[5]),
                    size: f[3].parse().unwrap_or(0),
                    modified: SystemTime::UNIX_EPOCH
                        + Duration::from_secs(f[4].parse().unwrap_or(0)),
                    rank: f[2].parse().unwrap_or(3),
                });
            }
            _ => continue,
        }
    }
    Some(Cached {
        hits,
        scanned_at,
        complete,
        dirs,
        gone,
    })
}

/// What a save records beside the rows themselves.
pub struct Save<'a> {
    /// Every file the index lists, cached rows and new hits together.
    pub hits: &'a [Hit],
    /// The full default root set, watched so a database dropped straight into
    /// one is noticed.
    pub roots: &'a [Root],
    /// Whether the walk covered everything it set out to. A cleared flag means
    /// the whole walk runs again next start.
    pub complete: bool,
    /// Directories an interrupted *refresh* never finished reading. They are
    /// recorded as never-read, so the next start walks those directories again
    /// without having to distrust the rest of the index — quitting the picker
    /// two seconds in must not cost a walk of the disk.
    pub unfinished: &'a [Root],
}

/// Save a scan so the next start does not have to walk again. Best-effort: a
/// failure here only costs a walk next time.
pub fn save_cache(save: Save<'_>) {
    if let Some(path) = cache_file() {
        save_cache_to(&path, save);
    }
}

pub(crate) fn save_cache_to(path: &Path, save: Save<'_>) {
    let Save {
        hits,
        roots,
        complete,
        unfinished,
    } = save;
    let secs = |t: SystemTime| {
        t.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    };
    // A path that cannot survive the round trip is dropped rather than
    // corrupting the file: rows are tab-separated so paths may contain spaces.
    let writable = |p: &Path| -> Option<String> {
        match p.to_str() {
            Some(s) if !s.contains('\t') && !s.contains('\n') => Some(s.to_string()),
            _ => None,
        }
    };
    let now = secs(SystemTime::now());
    let flag = u8::from(complete);
    let mut body = format!("# zdbview scan {CACHE_VERSION} {now} {flag}\n");

    // The directories whose contents the index claims to describe: every one
    // that held a hit, plus the roots themselves so a database dropped straight
    // into one is noticed. Their mtimes are read now rather than during the
    // walk, so a change made while the walk was running is missed exactly once.
    let mut dirs: Vec<&Path> = hits.iter().filter_map(|h| h.path.parent()).collect();
    dirs.extend(roots.iter().chain(unfinished).map(|r| r.path.as_path()));
    dirs.sort_unstable();
    dirs.dedup();
    // Never the directory the index itself lives in: writing the index moves
    // that directory's mtime, so watching it would mean every saved scan
    // invalidates itself the moment it is written.
    let own = path.parent();
    for dir in dirs {
        if Some(dir) == own {
            continue;
        }
        let Some(p) = writable(dir) else { continue };
        // A directory the walk did not finish reading is written as never-read,
        // which is what makes the next start pick it up again.
        if unfinished.iter().any(|r| r.path == dir) {
            body.push_str(&format!("d\t0\t{p}\n"));
            continue;
        }
        let Ok(mtime) = std::fs::metadata(dir).and_then(|m| m.modified()) else {
            continue;
        };
        body.push_str(&format!("d\t{}\t{}\n", secs(mtime), p));
    }

    // A saved list may be the cached rows plus what a later walk added, and the
    // same file can be in both — the walk only knows about its own hits. First
    // row wins, so the index never grows a second copy of a path.
    let mut listed = HashSet::new();
    for h in hits {
        let Some(p) = writable(&h.path) else { continue };
        if !listed.insert(&h.path) {
            continue;
        }
        body.push_str(&format!(
            "f\t{}\t{}\t{}\t{}\t{}\t{}\n",
            match h.kind {
                Kind::Sqlite => "sqlite",
                Kind::Rkyv => "rkyv",
            },
            h.rank,
            h.size,
            secs(h.modified),
            h.format.unwrap_or(""),
            p
        ));
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Write via temp + rename so a reader never sees a half-written list.
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Forget the saved scan, so the next walk starts from nothing.
pub fn clear_cache() {
    if let Some(path) = cache_file() {
        let _ = std::fs::remove_file(path);
    }
}

/// Where to look when no roots were given.
///
/// The producers in the format registry keep their stores in their own home
/// directory — `~/.zshrs/scripts.rkyv`, `~/.zshrs/compsys.db` and so on — so the
/// dot-directories of `$HOME` are the primary roots, most-recently-touched
/// first, followed by the XDG cache/data directories and the working directory.
/// `$HOME` itself contributes its own files but is not descended into.
///
/// `~/Library/Application Support` comes next-to-last. It is where every macOS
/// application keeps its databases — Ableton's `Live Database`, a browser
/// profile, a DAW's plugin index — so leaving it out meant the picker could not
/// offer the databases most worth opening on a Mac.
///
/// `/` is last, and covers everything the roots above already did. Those roots
/// are kept anyway because order is what the user feels: the dot-directories are
/// read in milliseconds, so the picker has the rows that matter before the walk
/// has left `/bin`. Duplicate hits are dropped by the walk, so listing a
/// directory twice costs a second `read_dir` of a warm directory and nothing
/// else.
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
    // `$HOME` files only: a stray `~/foo.db` counts, the whole home tree does
    // not. It is one `read_dir`, so it goes before the expensive root below and
    // can never be starved by it.
    roots.extend(home.as_ref().map(|h| Root::new(h.clone(), 0)));
    // Where macOS applications keep their databases. On Linux the path does not
    // exist and the `is_dir` filter below drops it.
    roots.extend(home.map(|h| Root::new(h.join("Library/Application Support"), DEEP)));
    // Everything else on this machine.
    roots.push(Root::new(PathBuf::from("/"), UNLIMITED));

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
        let mut w = Walk {
            tx,
            cancel: flag,
            examined: 0,
            deadline: Instant::now() + TIME_BUDGET,
            devs: local_devices(),
            sent: HashSet::new(),
        };
        for root in roots {
            // A root the user named is walked whatever disk it is on; only what
            // lies *below* it has to stay on a permitted device.
            w.devs.extend(device(&root.path));
            if w.walk(&root.path, 0, root.depth).is_break() {
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

/// Hard ceiling on recursion, so no directory tree can run the walk thread out
/// of stack. Nothing legitimate nests this far; what does (a runaway symlink
/// farm, a corrupted tree) is not worth the crash.
const MAX_DEPTH: usize = 64;

/// One walk of one set of roots: what it costs so far, and what it has already
/// reported.
///
/// The limits that stop it are entries seen and wall clock — both measure cost.
/// The number of hits is deliberately not a limit: a hit is a path the user
/// asked to be shown, and stopping on them truncated the walk mid-root, so the
/// roots after it were never looked at at all.
struct Walk {
    tx: mpsc::Sender<Hit>,
    cancel: Arc<AtomicBool>,
    examined: usize,
    deadline: Instant,
    /// Device ids the walk may descend into. Anything else is another disk —
    /// an SMB share, a USB drive, or the data volume seen a second time through
    /// its own mount point — and is left alone.
    devs: HashSet<u64>,
    /// Paths already reported, so the roots overlapping `/` cannot list the same
    /// database twice.
    sent: HashSet<PathBuf>,
}

impl Walk {
    fn spent(&self) -> bool {
        self.examined > MAX_ENTRIES || Instant::now() > self.deadline
    }

    /// Whether `dir` may be descended into: not on the never-walk list, and on a
    /// disk this walk is allowed to touch.
    fn may_descend(&self, dir: &Path, name: &str) -> bool {
        if SKIP_DIRS.contains(&name) || SKIP_PATHS.contains(&dir.to_string_lossy().as_ref()) {
            return false;
        }
        match device(dir) {
            Some(d) => self.devs.contains(&d),
            // Unreadable: descending it would fail anyway.
            None => false,
        }
    }

    fn walk(&mut self, dir: &Path, depth: usize, max_depth: usize) -> ControlFlow<()> {
        if depth > max_depth || depth > MAX_DEPTH || self.cancel.load(Ordering::Relaxed) {
            return ControlFlow::Continue(());
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            // Unreadable directories are normal (permissions); just move on.
            Err(_) => return ControlFlow::Continue(()),
        };
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            self.examined += 1;
            if self.spent() || self.cancel.load(Ordering::Relaxed) {
                return ControlFlow::Break(());
            }
            let path = entry.path();
            // `file_type` comes from the directory entry, so it costs no extra
            // stat and does not follow symlinks — which is also how directory
            // cycles are avoided.
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                let name = entry.file_name();
                if self.may_descend(&path, &name.to_string_lossy()) {
                    subdirs.push(path);
                }
            } else if ft.is_file() && !self.sent.contains(&path) {
                if let Some(hit) = classify(&path) {
                    self.sent.insert(path);
                    if self.tx.send(hit).is_err() {
                        // The picker closed.
                        return ControlFlow::Break(());
                    }
                }
            }
        }
        for sub in subdirs {
            self.walk(&sub, depth + 1, max_depth)?;
        }
        ControlFlow::Continue(())
    }
}

/// The device id of `path`, or `None` when it cannot be read.
#[cfg(unix)]
fn device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.dev())
}

#[cfg(not(unix))]
fn device(_path: &Path) -> Option<u64> {
    Some(0)
}

/// The disks a default walk is allowed on: the one `/` is on and the one `$HOME`
/// is on. On macOS those differ — `/` is the sealed read-only system volume and
/// the home directory lives on the data volume reached through firmlinks — so
/// both are needed, and naming exactly these two is what keeps every other
/// mounted disk (network shares included) out of the walk.
#[cfg(unix)]
fn local_devices() -> HashSet<u64> {
    let mut devs = HashSet::new();
    devs.extend(device(Path::new("/")));
    if let Some(home) = std::env::var_os("HOME") {
        devs.extend(device(Path::new(&home)));
    }
    devs
}

#[cfg(not(unix))]
fn local_devices() -> HashSet<u64> {
    HashSet::from([0])
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

    /// A shard from a producer that is not zdbview's: the header
    /// `spec/rkyv-shard-header.md` specifies, written by a type declared here
    /// rather than copied from `formats`, under a tag no zdbview build ships.
    /// This is what a third party's cache actually looks like on disk.
    #[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
    #[archive(check_bytes)]
    struct ForeignHeader {
        magic: u32,
        format_version: u32,
        producer_version: String,
        pointer_width: u32,
        built_at_secs: u64,
    }

    #[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
    #[archive(check_bytes)]
    struct ForeignShard {
        header: ForeignHeader,
        entries: std::collections::HashMap<String, Vec<u8>>,
    }

    /// `LUAR`, a tag this build has never heard of.
    const FOREIGN_MAGIC: u32 = crate::magics::TEST_TAG;

    fn foreign_shard_bytes() -> Vec<u8> {
        let mut entries = std::collections::HashMap::new();
        entries.insert("/tmp/init.lua".to_string(), b"compiled".to_vec());
        let shard = ForeignShard {
            header: ForeignHeader {
                magic: FOREIGN_MAGIC,
                format_version: 1,
                producer_version: "0.1.0".into(),
                pointer_width: 8,
                built_at_secs: 7,
            },
            entries,
        };
        rkyv::to_bytes::<_, 1024>(&shard).unwrap().to_vec()
    }

    /// The walk offers a foreign producer's shard, by name, on the strength of a
    /// registry entry alone — no copied archive type, no code that knows `LUAR`.
    #[test]
    fn the_walk_offers_a_shard_whose_tag_only_the_user_registered() {
        assert!(
            !crate::formats::BUILTIN_MAGICS
                .iter()
                .any(|(m, _)| *m == FOREIGN_MAGIC),
            "LUAR must not be a built-in, or this proves nothing"
        );
        // Register it the way a user's config file does, then walk.
        crate::magics::install_test_registry();

        let root = tmpdir("foreign");
        write(&root.join("init.rkyv"), &foreign_shard_bytes());
        let hits = collect(&root);

        assert_eq!(hits.len(), 1, "the shard is offered");
        assert_eq!(hits[0].kind, Kind::Rkyv);
        assert_eq!(
            hits[0].format,
            Some(crate::magics::TEST_TAG_NAME),
            "named by the registry, not by a decoder"
        );
        assert_eq!(hits[0].rank, 0, "ranked as a recognized shard, not as noise");
        let _ = std::fs::remove_dir_all(&root);
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
    /// The cache is what stops a walk on every start: a saved scan must come
    /// back with every field intact, including the recognized format label.
    #[test]
    fn cache_round_trips_every_field() {
        let dir = tmpdir("cache_rt");
        let db = dir.join("real.db");
        write(&db, &sqlite_bytes());
        let shard = dir.join("scripts.rkyv");
        write(
            &shard,
            &crate::formats::test_script_shard_bytes("/tmp/x.sh", b"blob"),
        );
        let hits = collect(&dir);
        assert_eq!(hits.len(), 2);

        let file = tmpdir("idx1").join("scan");
        save_cache_to(
            &file,
            Save {
                hits: &hits,
                roots: &[Root::new(dir.clone(), DEEP)],
                complete: true,
                unfinished: &[],
            },
        );
        let back = load_cache_from(&file).expect("cache must load");
        assert_eq!(back.hits.len(), 2);
        for (a, b) in hits.iter().zip(&back.hits) {
            assert_eq!(a.path, b.path);
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.format, b.format, "format label must survive");
            assert_eq!(a.size, b.size);
            assert_eq!(a.rank, b.rank);
            // Whole seconds only, which is all the file stores.
            let secs = |t: SystemTime| {
                t.duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            };
            assert_eq!(secs(a.modified), secs(b.modified));
        }
        assert!(
            back.refresh_roots(&[]).is_empty(),
            "nothing has been touched, so the scan still describes the disk"
        );
        assert!(back.complete);
        assert!(back.age() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Entries whose file has gone away must not be offered from the cache.
    #[test]
    fn cache_drops_vanished_files() {
        let dir = tmpdir("cache_gone");
        let db = dir.join("gone.db");
        write(&db, &sqlite_bytes());
        let hits = collect(&dir);
        assert_eq!(hits.len(), 1);
        let file = tmpdir("idx2").join("scan");
        save_cache_to(
            &file,
            Save {
                hits: &hits,
                roots: &[],
                complete: true,
                unfinished: &[],
            },
        );
        std::fs::remove_file(&db).unwrap();
        let back = load_cache_from(&file).expect("header still parses");
        assert!(back.hits.is_empty(), "a deleted file must be dropped");
        assert!(
            !back.refresh_roots(&[]).is_empty(),
            "a file that went away invalidates its directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Age alone is not staleness: an index of a disk nobody touched is still a
    /// correct description of it, so no walk runs however old it is. This is the
    /// whole point of dropping the 24-hour expiry.
    #[test]
    fn an_untouched_scan_never_goes_stale_with_age() {
        let dir = tmpdir("cache_age");
        write(&dir.join("real.db"), &sqlite_bytes());
        let hits = collect(&dir);
        let file = tmpdir("idx3").join("scan");
        save_cache_to(
            &file,
            Save {
                hits: &hits,
                roots: &[Root::new(dir.clone(), DEEP)],
                complete: true,
                unfinished: &[],
            },
        );

        let mut back = load_cache_from(&file).expect("cache must load");
        // A scan from a year ago, over a directory that has not changed since.
        back.scanned_at -= Duration::from_secs(365 * 24 * 60 * 60);
        assert!(back.age() > Duration::from_secs(300 * 24 * 60 * 60));
        assert!(
            back.refresh_roots(&[]).is_empty(),
            "age is not evidence of anything"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A refresh walks the directories that moved, not the disk: on a machine
    /// being worked on something is always changing, and re-walking `/` for it
    /// would put the walk back on every start.
    #[test]
    fn only_the_changed_directories_are_offered_for_a_rewalk() {
        let dir = tmpdir("cache_changed");
        write(&dir.join("a/first.db"), &sqlite_bytes());
        write(&dir.join("b/second.db"), &sqlite_bytes());
        let hits = collect(&dir);
        assert_eq!(hits.len(), 2);
        let file = tmpdir("idx7").join("scan");
        save_cache_to(
            &file,
            Save {
                hits: &hits,
                roots: &[Root::new(dir.clone(), DEEP)],
                complete: true,
                unfinished: &[],
            },
        );
        assert!(load_cache_from(&file)
            .expect("loads")
            .changed_dirs()
            .is_empty());

        std::thread::sleep(Duration::from_millis(1100)); // mtime is whole seconds
        write(&dir.join("b/third.db"), &sqlite_bytes());
        assert_eq!(
            load_cache_from(&file).expect("loads").changed_dirs(),
            vec![dir.join("b")],
            "only the directory that gained a file is worth walking"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Quitting the picker while a refresh is running must cost the refresh, not
    /// the index: the directories it did not finish reading come back as ones to
    /// walk, and everything else is still trusted — otherwise every session that
    /// opens a file in the first seconds pays for a walk of the disk.
    #[test]
    fn an_interrupted_refresh_costs_only_the_directories_it_was_reading() {
        let dir = tmpdir("cache_interrupted");
        write(&dir.join("kept/real.db"), &sqlite_bytes());
        let busy = dir.join("busy");
        std::fs::create_dir_all(&busy).unwrap();
        let hits = collect(&dir);
        let file = tmpdir("idx8").join("scan");

        // What park_scan writes when a refresh of `busy` is cut short.
        save_cache_to(
            &file,
            Save {
                hits: &hits,
                roots: &[Root::new(dir.clone(), DEEP)],
                complete: true,
                unfinished: &[Root::new(busy.clone(), DEEP)],
            },
        );

        let back = load_cache_from(&file).expect("cache must load");
        assert!(back.complete, "the index as a whole is still good");
        assert_eq!(back.hits.len(), 1, "the rows are kept");
        assert_eq!(
            back.refresh_roots(&[Root::new(dir.clone(), DEEP)])
                .iter()
                .map(|r| r.path.clone())
                .collect::<Vec<_>>(),
            vec![busy],
            "only the directory the refresh was reading comes back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What does invalidate it: a directory it described has gained or lost an
    /// entry since it was read.
    #[test]
    fn a_changed_directory_makes_the_scan_stale() {
        let dir = tmpdir("cache_dirty");
        write(&dir.join("real.db"), &sqlite_bytes());
        let hits = collect(&dir);
        let file = tmpdir("idx4").join("scan");
        save_cache_to(
            &file,
            Save {
                hits: &hits,
                roots: &[Root::new(dir.clone(), DEEP)],
                complete: true,
                unfinished: &[],
            },
        );
        assert!(load_cache_from(&file)
            .expect("loads")
            .refresh_roots(&[])
            .is_empty());

        // A second database appears beside the first, which the index cannot
        // know about — but the directory's mtime moves, which it can.
        std::thread::sleep(Duration::from_millis(1100)); // mtime is whole seconds
        write(&dir.join("later.db"), &sqlite_bytes());
        assert!(
            !load_cache_from(&file)
                .expect("loads")
                .refresh_roots(&[])
                .is_empty(),
            "a new file in a watched directory must trigger a walk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A walk cut short saves what it had, and says so: the rows are worth
    /// keeping, but the index is not a complete description of the disk.
    #[test]
    fn a_partial_scan_is_kept_but_stays_stale() {
        let dir = tmpdir("cache_partial");
        write(&dir.join("real.db"), &sqlite_bytes());
        let hits = collect(&dir);
        let file = tmpdir("idx5").join("scan");
        save_cache_to(
            &file,
            Save {
                hits: &hits,
                roots: &[Root::new(dir.clone(), DEEP)],
                complete: false,
                unfinished: &[],
            },
        );

        let back = load_cache_from(&file).expect("cache must load");
        assert_eq!(back.hits.len(), 1, "the work is not thrown away");
        assert!(!back.complete);
        assert_eq!(
            back.refresh_roots(&[Root::new(dir.clone(), DEEP)]).len(),
            1,
            "an unfinished walk has to be finished, whatever changed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Garbage, a foreign version, or a missing file must all fall back to a walk
    /// rather than surfacing broken rows.
    #[test]
    fn unreadable_or_foreign_cache_yields_none() {
        let dir = tmpdir("cache_bad");
        let f = dir.join("c");
        write(&f, b"not a zdbview cache\n");
        assert!(load_cache_from(&f).is_none());
        write(&f, b"# zdbview scan 99 1700000000 1\n");
        assert!(load_cache_from(&f).is_none(), "version must be checked");
        // The pre-2 format, which had no completeness flag, is not read as if it
        // had one — it is simply walked again.
        write(
            &f,
            b"# zdbview scan 1 1700000000\nsqlite\t2\t4096\t1\t\t/x.db\n",
        );
        assert!(
            load_cache_from(&f).is_none(),
            "an older layout is not parsed"
        );
        assert!(load_cache_from(&dir.join("absent")).is_none());
        // A valid header with a corrupt row keeps the header and skips the row.
        write(&f, b"# zdbview scan 2 1700000000 1\nnonsense\n");
        let back = load_cache_from(&f).expect("header parses");
        assert!(back.hits.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// `~/Library/Application Support` is where a macOS application keeps its
    /// database — Ableton's `Live Database` being the case that exposed its
    /// absence — so it must be a default root, and it must come after the cheap
    /// ones, which it must never starve.
    #[test]
    fn application_support_is_walked_after_the_cheap_roots() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        let want = home.join("Library/Application Support");
        if !want.is_dir() {
            return; // Not macOS.
        }
        let want = want.canonicalize().unwrap_or(want);
        let roots = default_roots();
        let at = roots.iter().position(|r| r.path == want);
        assert_eq!(
            at,
            Some(roots.len() - 2),
            "expected it second-to-last, before `/`: {:?}",
            roots.iter().map(|r| &r.path).collect::<Vec<_>>()
        );
    }

    /// The whole disk is a root: a database is worth finding wherever it lives,
    /// and the index that says where they all are is kept until the filesystem
    /// invalidates it, so the walk is paid for about once.
    #[test]
    fn the_whole_filesystem_is_the_last_root() {
        let roots = default_roots();
        let last = roots.last().expect("there is always `/`");
        assert_eq!(last.path, PathBuf::from("/"));
        assert_eq!(last.depth, UNLIMITED, "a database can sit at any depth");
    }

    /// The mount points that would break a walk of `/`: the data volume seen a
    /// second time through `/System/Volumes/Data` (the whole disk again, under
    /// different paths), and anything under `/Volumes` — a USB disk or, worse, a
    /// network share walked over SMB.
    #[test]
    fn the_walk_refuses_other_volumes_and_the_second_view_of_this_one() {
        for p in [
            "/System/Volumes",
            "/Volumes",
            "/net",
            "/private/var/folders",
        ] {
            assert!(SKIP_PATHS.contains(&p), "{p} must never be descended into");
        }
        let w = Walk {
            tx: mpsc::channel().0,
            cancel: Arc::new(AtomicBool::new(false)),
            examined: 0,
            deadline: Instant::now() + TIME_BUDGET,
            devs: local_devices(),
            sent: HashSet::new(),
        };
        for p in SKIP_PATHS {
            let path = Path::new(p);
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            assert!(!w.may_descend(path, &name), "{p} was descended into");
        }
        // A directory on a disk this walk was never told about stays out of it,
        // whatever its name.
        let alien = Walk {
            devs: HashSet::new(),
            ..w
        };
        assert!(!alien.may_descend(Path::new("/usr"), "usr"));
    }

    /// What pressing Esc out of an open file costs: the saved index is loaded and
    /// checked again on every return to the picker, so the whole of it has to
    /// stay well inside a keypress. Ignored by default because it measures this
    /// machine's own index:
    ///
    /// ```text
    /// cargo test --bin zdbview -- --ignored --nocapture reopening
    /// ```
    #[test]
    #[ignore = "measures the machine's own saved index"]
    fn reopening_the_picker_reads_the_index_in_a_keypress() {
        if load_cache().is_none() {
            eprintln!("no saved index on this machine — nothing to measure");
            return;
        }
        let started = Instant::now();
        let cached = load_cache().expect("it loaded a moment ago");
        let loaded = started.elapsed();
        let roots = default_roots();
        let checking = Instant::now();
        let todo = cached.refresh_roots(&roots);
        let checked = checking.elapsed();
        eprintln!(
            "{} rows loaded in {loaded:.1?}, {} watched dirs checked in {checked:.1?}, {} to walk",
            cached.hits.len(),
            cached.dirs.len(),
            todo.len()
        );
        assert!(
            loaded + checked < Duration::from_millis(250),
            "reading the index took {:.1?}",
            loaded + checked
        );
    }

    /// The real thing, on the machine it is run on: a walk of every default root
    /// including `/`. Ignored by default because it reads the whole disk, which
    /// belongs in a deliberate run rather than in every `cargo test`:
    ///
    /// ```text
    /// cargo test --bin zdbview -- --ignored --nocapture whole_disk
    /// ```
    ///
    /// What it holds to is the part that cannot be checked on a temp directory:
    /// the walk finishes inside its own backstop, reports each path once, and
    /// never leaves the disks it was given — no mounted volume, and no second
    /// pass over this one through `/System/Volumes/Data`.
    #[test]
    #[ignore = "reads the whole disk"]
    fn a_whole_disk_walk_finishes_and_stays_on_its_own_disks() {
        let started = Instant::now();
        let allowed = local_devices();
        let scan = spawn(default_roots());
        let mut hits = Vec::new();
        while let Ok(h) = scan.rx.recv() {
            hits.push(h);
        }
        let took = started.elapsed();
        eprintln!("{} files in {took:.1?}", hits.len());

        assert!(
            took < TIME_BUDGET,
            "the walk hit its hang backstop instead of finishing"
        );
        let mut seen = HashSet::new();
        for h in &hits {
            assert!(seen.insert(h.path.clone()), "listed twice: {:?}", h.path);
            assert!(
                !h.path.starts_with("/System/Volumes"),
                "the data volume was walked a second time: {:?}",
                h.path
            );
            assert!(
                !h.path.starts_with("/Volumes"),
                "another volume was walked: {:?}",
                h.path
            );
            let dev = h.path.parent().and_then(device).expect("readable parent");
            assert!(
                allowed.contains(&dev),
                "{:?} is on device {dev}, which is not this machine's disk",
                h.path
            );
        }
    }

    /// Saving the cached rows together with what a later walk found must not
    /// leave two rows for one file: the walk cannot know what the cache already
    /// listed, so the index is what has to hold the line.
    #[test]
    fn a_file_saved_twice_is_listed_once() {
        let dir = tmpdir("cache_dup");
        write(&dir.join("real.db"), &sqlite_bytes());
        let hits = collect(&dir);
        assert_eq!(hits.len(), 1);
        // The shape park_scan writes: what was loaded, plus what the walk saw.
        let both: Vec<Hit> = hits.iter().chain(hits.iter()).cloned().collect();
        let file = tmpdir("idx6").join("scan");
        save_cache_to(
            &file,
            Save {
                hits: &both,
                roots: &[],
                complete: true,
                unfinished: &[],
            },
        );

        let back = load_cache_from(&file).expect("cache must load");
        assert_eq!(back.hits.len(), 1, "got {:?}", back.hits);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/` covers what the earlier roots already walked, so the same database is
    /// reachable twice — it must still be reported once.
    #[test]
    fn a_file_under_two_roots_is_reported_once() {
        let root = tmpdir("overlap");
        write(&root.join("sub/dup.db"), &sqlite_bytes());
        // The same tree as two roots, the way `~/.cache` and `/` overlap.
        let scan = spawn(vec![
            Root::new(root.join("sub"), DEEP),
            Root::new(root.clone(), DEEP),
        ]);
        let mut hits = Vec::new();
        while let Ok(h) = scan.rx.recv() {
            hits.push(h);
        }
        assert_eq!(hits.len(), 1, "got {hits:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A hit is not a cost, so producing one must never stop the walk: a cap on
    /// hits used to cut the walk mid-root and lose every root after it.
    #[test]
    fn hits_do_not_exhaust_the_budget() {
        let root = tmpdir("many_hits");
        for i in 0..600 {
            write(&root.join(format!("f{i}.db")), &sqlite_bytes());
        }
        assert_eq!(collect(&root).len(), 600);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The clock is the limit meant to bind; the entry cap is only a backstop
    /// for a pathological tree, so it must sit far above one machine's home.
    #[test]
    fn the_entry_cap_is_a_backstop_not_the_binding_limit() {
        // A constant, so this holds at build time rather than at test time.
        const {
            assert!(
                MAX_ENTRIES >= 500_000,
                "an entry cap this low stops the walk long before TIME_BUDGET"
            )
        };
    }
}
