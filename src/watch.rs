//! Live write monitoring for the stores zdbview knows about.
//!
//! The write-monitor screen (`w`) is a `top` for caches: which shard or database
//! is being written to right now, how fast, and how much since the screen opened.
//!
//! Detection is by polling `stat` on a watched set rather than by a filesystem
//! notification API: it needs no extra dependency, behaves the same on macOS and
//! Linux, and a few hundred `stat` calls per tick cost far less than a frame.
//! What it gives up is sub-tick resolution — a write that lands and is undone
//! inside one interval is invisible — which is an acceptable trade for a monitor
//! whose job is showing sustained activity.
//!
//! Two details matter for accuracy:
//!
//! * **SQLite writes land in the `-wal` file first**, often growing it by
//!   megabytes while the database itself is untouched until a checkpoint. The
//!   sidecar is sampled with the main file and its growth is attributed to the
//!   database, or a busy database would look idle.
//! * **rkyv shards are rewritten atomically** (temp file plus rename), so their
//!   size changes in one step and the inode changes with it. Growth is therefore
//!   measured as "bytes that appeared", not as an append offset.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::store::Kind;

/// Samples kept per file for the activity sparkline.
pub const HISTORY: usize = 32;
/// Default gap between samples.
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(500);
/// A file counts as "active" if it was written within this long.
pub const ACTIVE_WINDOW: Duration = Duration::from_secs(5);

/// How the monitor's rows are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Most recently written first — the default, and what a `top` implies.
    Recent,
    /// Highest current write rate first.
    Rate,
    /// Most bytes written since the monitor opened.
    Total,
    /// Largest file first.
    Size,
    /// Path order, for a stable list to read.
    Name,
}

impl Order {
    pub const ALL: &'static [Order] = &[
        Order::Recent,
        Order::Rate,
        Order::Total,
        Order::Size,
        Order::Name,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Order::Recent => "recent",
            Order::Rate => "rate",
            Order::Total => "written",
            Order::Size => "size",
            Order::Name => "name",
        }
    }

    pub fn next(self) -> Order {
        let i = Order::ALL.iter().position(|&o| o == self).unwrap_or(0);
        Order::ALL[(i + 1) % Order::ALL.len()]
    }
}

/// One watched file.
#[derive(Debug, Clone)]
pub struct Target {
    pub path: PathBuf,
    pub kind: Kind,
    /// Size of the file itself, last sample.
    pub size: u64,
    /// Size of the SQLite `-wal` sidecar, last sample (0 when there is none).
    pub wal: u64,
    pub mtime: SystemTime,
    /// Bytes that appeared since the monitor opened (file plus sidecar).
    pub written: u64,
    /// When growth was last seen.
    pub last_write: Option<Instant>,
    /// Bytes per sample, oldest first, for the sparkline.
    pub history: VecDeque<u64>,
    /// `false` once the file goes away (deleted or renamed over).
    pub present: bool,
}

impl Target {
    fn new(path: PathBuf, kind: Kind) -> Self {
        let (size, wal, mtime, present) = probe(&path, kind);
        Target {
            path,
            kind,
            size,
            wal,
            mtime,
            written: 0,
            last_write: None,
            history: VecDeque::new(),
            present,
        }
    }

    /// Bytes per second over the samples still inside `window`.
    pub fn rate(&self, interval: Duration) -> f64 {
        if self.history.is_empty() || interval.is_zero() {
            return 0.0;
        }
        // Average the last four samples so a single quiet tick does not read as
        // "stopped" while a write is still in progress.
        let take = 4.min(self.history.len());
        let sum: u64 = self.history.iter().rev().take(take).sum();
        sum as f64 / (interval.as_secs_f64() * take as f64)
    }

    /// Was this file written within `window`?
    pub fn active(&self, window: Duration) -> bool {
        self.last_write.is_some_and(|t| t.elapsed() < window)
    }

    /// The name shown in the monitor.
    pub fn name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
    }
}

/// Size, sidecar size, mtime and existence of `path`.
fn probe(path: &Path, kind: Kind) -> (u64, u64, SystemTime, bool) {
    match std::fs::metadata(path) {
        Ok(m) => {
            let wal = if kind == Kind::Sqlite {
                wal_size(path)
            } else {
                0
            };
            (
                m.len(),
                wal,
                m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                true,
            )
        }
        Err(_) => (0, 0, SystemTime::UNIX_EPOCH, false),
    }
}

/// Size of a SQLite write-ahead log beside `path`, if any.
fn wal_size(path: &Path) -> u64 {
    let mut name = path.as_os_str().to_os_string();
    name.push("-wal");
    std::fs::metadata(PathBuf::from(name))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// The watched set plus its sampling state.
pub struct Watcher {
    pub targets: Vec<Target>,
    pub interval: Duration,
    pub paused: bool,
    /// When the monitor opened, for the session total's rate.
    started: Instant,
    last_tick: Instant,
    /// Samples taken, so the view can say whether anything has happened yet.
    pub ticks: u64,
}

impl Watcher {
    pub fn new(targets: impl IntoIterator<Item = (PathBuf, Kind)>) -> Self {
        let mut seen = std::collections::HashSet::new();
        let targets: Vec<Target> = targets
            .into_iter()
            .filter(|(p, _)| seen.insert(p.clone()))
            .map(|(p, k)| Target::new(p, k))
            .collect();
        Watcher {
            targets,
            interval: DEFAULT_INTERVAL,
            paused: false,
            started: Instant::now(),
            last_tick: Instant::now(),
            ticks: 0,
        }
    }

    /// Sample every target if the interval has elapsed. Returns `true` when a
    /// sample was taken, which is the view's cue to redraw.
    pub fn tick(&mut self) -> bool {
        if self.paused || self.last_tick.elapsed() < self.interval {
            return false;
        }
        self.last_tick = Instant::now();
        self.ticks += 1;
        for t in &mut self.targets {
            let (size, wal, mtime, present) = probe(&t.path, t.kind);
            // Only growth counts as a write. A shrink (a checkpoint folding the
            // WAL back, or a rewritten shard) is real activity but its size is
            // not the number of bytes written, so it is recorded as a touch
            // without inflating the totals.
            let grew = size.saturating_sub(t.size) + wal.saturating_sub(t.wal);
            let touched = grew > 0 || mtime != t.mtime || present != t.present;
            if grew > 0 {
                t.written += grew;
            }
            if touched {
                t.last_write = Some(Instant::now());
            }
            t.size = size;
            t.wal = wal;
            t.mtime = mtime;
            t.present = present;
            t.history.push_back(grew);
            if t.history.len() > HISTORY {
                t.history.pop_front();
            }
        }
        true
    }

    /// Row indices in `order`, newest/busiest first as appropriate.
    pub fn sorted(&self, order: Order) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.targets.len()).collect();
        let interval = self.interval;
        match order {
            Order::Recent => idx.sort_by_key(|&i| {
                // Never-written files sort last, most recent first.
                match self.targets[i].last_write {
                    Some(t) => (0u8, t.elapsed()),
                    None => (1, Duration::MAX),
                }
            }),
            Order::Rate => idx.sort_by(|&a, &b| {
                self.targets[b]
                    .rate(interval)
                    .total_cmp(&self.targets[a].rate(interval))
            }),
            Order::Total => idx.sort_by_key(|&i| std::cmp::Reverse(self.targets[i].written)),
            Order::Size => idx.sort_by_key(|&i| std::cmp::Reverse(self.targets[i].size)),
            Order::Name => idx.sort_by(|&a, &b| self.targets[a].path.cmp(&self.targets[b].path)),
        }
        idx
    }

    /// Bytes seen across every file since the monitor opened.
    pub fn total_written(&self) -> u64 {
        self.targets.iter().map(|t| t.written).sum()
    }

    /// Aggregate bytes per second over the recent samples.
    pub fn total_rate(&self) -> f64 {
        self.targets.iter().map(|t| t.rate(self.interval)).sum()
    }

    /// How many files were written within `window`.
    pub fn active_count(&self, window: Duration) -> usize {
        self.targets.iter().filter(|t| t.active(window)).count()
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Eight-level block bar for one sample, scaled against `peak`.
pub fn spark(sample: u64, peak: u64) -> char {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if sample == 0 || peak == 0 {
        return ' ';
    }
    // Ceil so any non-zero sample shows at least the lowest bar.
    let level = ((sample as f64 / peak as f64) * BARS.len() as f64).ceil() as usize;
    BARS[level.clamp(1, BARS.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("zdbview_watch_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
    }

    /// Force a sample regardless of the interval.
    fn sample(w: &mut Watcher) {
        w.interval = Duration::ZERO;
        assert!(w.tick());
        w.interval = DEFAULT_INTERVAL;
    }

    #[test]
    fn growth_is_counted_as_written_bytes() {
        let d = dir("growth");
        let f = d.join("shard.rkyv");
        append(&f, b"0123456789");
        let mut w = Watcher::new([(f.clone(), Kind::Rkyv)]);
        assert_eq!(w.targets[0].size, 10);
        assert_eq!(w.targets[0].written, 0, "the starting size is not a write");
        assert!(w.targets[0].last_write.is_none());

        append(&f, &[b'x'; 90]);
        sample(&mut w);
        assert_eq!(w.targets[0].size, 100);
        assert_eq!(w.targets[0].written, 90, "only the growth counts");
        assert!(w.targets[0].last_write.is_some());
        assert!(w.targets[0].active(ACTIVE_WINDOW));
        assert_eq!(w.total_written(), 90);

        // A quiet tick adds nothing but keeps the total.
        sample(&mut w);
        assert_eq!(w.targets[0].written, 90);
        assert_eq!(*w.targets[0].history.back().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// SQLite writes land in the `-wal` sidecar first, so its growth has to be
    /// attributed to the database or a busy database reads as idle.
    #[test]
    fn sqlite_wal_growth_counts_for_the_database() {
        let d = dir("wal");
        let db = d.join("data.db");
        append(&db, b"SQLite format 3\0");
        let mut w = Watcher::new([(db.clone(), Kind::Sqlite)]);
        assert_eq!(w.targets[0].wal, 0);

        // The database file does not change; only the WAL grows.
        let wal = d.join("data.db-wal");
        append(&wal, &[b'w'; 4096]);
        sample(&mut w);
        assert_eq!(w.targets[0].size, 16, "the database itself is untouched");
        assert_eq!(w.targets[0].wal, 4096);
        assert_eq!(w.targets[0].written, 4096, "WAL growth is the write");
        assert!(w.targets[0].active(ACTIVE_WINDOW));

        // A checkpoint shrinks the WAL and grows the database: only the growth is
        // added, so the total does not double-count the same bytes.
        let before = w.targets[0].written;
        std::fs::write(&wal, b"").unwrap();
        append(&db, &[b'd'; 1000]);
        sample(&mut w);
        assert_eq!(w.targets[0].wal, 0);
        assert_eq!(w.targets[0].written, before + 1000);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A rkyv shard is rewritten by rename, so it can shrink; that is activity
    /// but not a byte count.
    #[test]
    fn a_rewritten_shard_is_activity_without_inflating_the_total() {
        let d = dir("rewrite");
        let f = d.join("s.rkyv");
        append(&f, &[b'a'; 5000]);
        let mut w = Watcher::new([(f.clone(), Kind::Rkyv)]);
        sample(&mut w);
        let before = w.targets[0].written;

        // Replace with something smaller, as an atomic rewrite would.
        std::fs::write(&f, [b'b'; 100]).unwrap();
        sample(&mut w);
        assert_eq!(w.targets[0].size, 100);
        assert_eq!(w.targets[0].written, before, "a shrink adds no bytes");
        assert!(
            w.targets[0].last_write.is_some(),
            "but it still counts as a touch"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_vanished_file_is_marked_absent_without_erroring() {
        let d = dir("gone");
        let f = d.join("s.rkyv");
        append(&f, b"x");
        let mut w = Watcher::new([(f.clone(), Kind::Rkyv)]);
        assert!(w.targets[0].present);
        std::fs::remove_file(&f).unwrap();
        sample(&mut w);
        assert!(!w.targets[0].present);
        assert_eq!(w.targets[0].size, 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pausing_stops_sampling() {
        let d = dir("pause");
        let f = d.join("s.rkyv");
        append(&f, b"x");
        let mut w = Watcher::new([(f.clone(), Kind::Rkyv)]);
        w.paused = true;
        w.interval = Duration::ZERO;
        append(&f, &[b'y'; 50]);
        assert!(!w.tick(), "a paused watcher must not sample");
        assert_eq!(w.targets[0].written, 0);
        w.paused = false;
        assert!(w.tick());
        assert_eq!(w.targets[0].written, 50);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn history_is_bounded_and_rate_averages_recent_samples() {
        let d = dir("rate");
        let f = d.join("s.rkyv");
        append(&f, b"x");
        let mut w = Watcher::new([(f.clone(), Kind::Rkyv)]);
        w.interval = Duration::from_secs(1);
        for _ in 0..HISTORY + 10 {
            append(&f, &[b'z'; 1000]);
            w.interval = Duration::ZERO;
            w.tick();
            w.interval = Duration::from_secs(1);
        }
        assert_eq!(
            w.targets[0].history.len(),
            HISTORY,
            "history must be capped"
        );
        // 1000 bytes per one-second sample.
        let rate = w.targets[0].rate(Duration::from_secs(1));
        assert!((rate - 1000.0).abs() < 1.0, "rate was {rate}");
        assert!(w.total_rate() > 0.0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn duplicate_paths_are_watched_once() {
        let d = dir("dupes");
        let f = d.join("s.rkyv");
        append(&f, b"x");
        let w = Watcher::new([
            (f.clone(), Kind::Rkyv),
            (f.clone(), Kind::Rkyv),
            (d.join("other.db"), Kind::Sqlite),
        ]);
        assert_eq!(w.targets.len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ordering_puts_the_interesting_row_first() {
        let d = dir("order");
        let (a, b, c) = (d.join("a.rkyv"), d.join("b.rkyv"), d.join("c.rkyv"));
        append(&a, &[b'a'; 10]);
        append(&b, &[b'b'; 20000]);
        append(&c, &[b'c'; 100]);
        let mut w = Watcher::new([
            (a.clone(), Kind::Rkyv),
            (b.clone(), Kind::Rkyv),
            (c.clone(), Kind::Rkyv),
        ]);
        // `c` is written a lot, `a` a little, `b` not at all.
        append(&c, &[b'c'; 9000]);
        append(&a, &[b'a'; 10]);
        sample(&mut w);

        let by = |o: Order| -> Vec<String> {
            w.sorted(o)
                .iter()
                .map(|&i| w.targets[i].name().to_string())
                .collect()
        };
        assert_eq!(by(Order::Total)[0], "c.rkyv", "most bytes first");
        assert_eq!(by(Order::Rate)[0], "c.rkyv", "fastest first");
        assert_eq!(by(Order::Size)[0], "b.rkyv", "largest file first");
        assert_eq!(by(Order::Name), vec!["a.rkyv", "b.rkyv", "c.rkyv"]);
        // Written files come before untouched ones under Recent.
        let recent = by(Order::Recent);
        assert_eq!(recent.last().unwrap(), "b.rkyv", "idle file sorts last");
        assert_eq!(w.active_count(ACTIVE_WINDOW), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn order_cycles_through_every_mode() {
        let mut o = Order::Recent;
        let mut seen = vec![o];
        for _ in 0..Order::ALL.len() - 1 {
            o = o.next();
            assert!(!seen.contains(&o), "{o:?} repeated early");
            seen.push(o);
        }
        assert_eq!(o.next(), Order::Recent, "wraps back to the start");
        for o in Order::ALL {
            assert!(!o.label().is_empty());
        }
    }

    #[test]
    fn spark_scales_and_shows_any_write() {
        assert_eq!(spark(0, 100), ' ', "no write is blank");
        assert_eq!(spark(100, 100), '█', "the peak is full height");
        assert_eq!(spark(1, 1_000_000), '▁', "a tiny write still shows");
        assert_eq!(spark(50, 100), '▄');
        assert_eq!(spark(10, 0), ' ', "no peak yet");
        // Never panics or wraps for a sample above the peak.
        assert_eq!(spark(500, 100), '█');
    }
}
