//! The write-ahead log tail: what a SQLite database is about to become.
//!
//! A `-wal` file is a 32-byte header followed by fixed-size frames, each a
//! 24-byte frame header plus one page image. Every committed-but-uncheckpointed
//! transaction lives here — the pages are already durable, but the main database
//! file does not carry them yet. Reading it is therefore a look at the database's
//! immediate future, and it costs nothing: the frame headers alone say which
//! pages a transaction touched and where it committed, so the page images are
//! never read.
//!
//! Layout (SQLite file format, section 4.1):
//!
//! ```text
//! header  0..4   magic 0x377f0682 (LE checksums) or 0x377f0683 (BE)
//!         4..8   format version (3007000)
//!         8..12  page size
//!        12..16  checkpoint sequence number
//!        16..20  salt-1     ── a checkpoint bumps these, which is what
//!        20..24  salt-2     ── invalidates every older frame in place
//!        24..32  header checksum
//!
//! frame   0..4   page number
//!         4..8   database size in pages AFTER this frame, or 0
//!                → non-zero marks a COMMIT
//!         8..12  salt-1  ┐ must equal the header's, else the frame is
//!        12..16  salt-2  ┘ leftover from a previous, checkpointed log
//!        16..24  frame checksum
//!        24..    the page image itself (page-size bytes)
//! ```
//!
//! Frames whose salts do not match the header are stale: a checkpoint reset the
//! log and newer writes have only partly overwritten what was there. They are
//! reported as such rather than skipped silently, because "the tail stops here"
//! is the interesting fact — everything after it is a ghost of an older log.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The WAL header's two magic values; they differ only in checksum endianness.
const MAGIC_LE: u32 = 0x377f_0682;
const MAGIC_BE: u32 = 0x377f_0683;
const WAL_HEADER: usize = 32;
const FRAME_HEADER: usize = 24;

/// One frame of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// 1-based position in the log.
    pub index: u32,
    /// Which database page this frame carries.
    pub page: u32,
    /// Database size in pages after this frame — non-zero only on a commit.
    pub db_size: u32,
    /// Whether this frame ends a transaction.
    pub commit: bool,
    /// Whether the frame's salts match the current header. A `false` here means
    /// the live log ended and the rest is a previous log's remains.
    pub live: bool,
}

/// A parsed `-wal`, newest frames last.
#[derive(Debug, Clone)]
pub struct WalTail {
    pub path: PathBuf,
    pub page_size: u32,
    /// Bumped by every checkpoint, so it says how many times this log has been
    /// folded into the database.
    pub checkpoint_seq: u32,
    pub salt1: u32,
    pub salt2: u32,
    /// Frames still belonging to the current log.
    pub live_frames: u32,
    /// Frames present in the file, live or stale.
    pub total_frames: u32,
    /// Committed transactions in the live region.
    pub commits: u32,
    /// Bytes of `-wal` on disk.
    pub size: u64,
    /// The tail itself, oldest first, capped by the caller's window.
    pub frames: Vec<Frame>,
}

impl WalTail {
    /// Pages waiting to be checkpointed into the database file, counting a page
    /// once however many times it was rewritten.
    pub fn distinct_pages(&self) -> usize {
        let mut seen: Vec<u32> = self
            .frames
            .iter()
            .filter(|f| f.live)
            .map(|f| f.page)
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }

    /// Frames written since the last commit — an open transaction, if any.
    pub fn uncommitted(&self) -> usize {
        self.frames
            .iter()
            .rev()
            .take_while(|f| f.live && !f.commit)
            .count()
    }
}

/// The `-wal` companion of `db`, whether or not it exists.
pub fn wal_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push("-wal");
    PathBuf::from(name)
}

/// Read the last `window` frame headers of `db`'s write-ahead log.
///
/// Returns `None` when there is no log, when it is too short to hold a header,
/// or when the header is not a WAL header at all. Only frame *headers* are read
/// — one seek and 24 bytes per frame — so tailing a 190 MB log costs the same as
/// tailing an empty one.
pub fn read_tail(db: &Path, window: usize) -> Option<WalTail> {
    let path = wal_path(db);
    let size = std::fs::metadata(&path).ok()?.len();
    if size < WAL_HEADER as u64 {
        return None;
    }
    let mut f = std::fs::File::open(&path).ok()?;
    let mut head = [0u8; WAL_HEADER];
    f.read_exact(&mut head).ok()?;

    let magic = be32(&head[0..4]);
    if magic != MAGIC_LE && magic != MAGIC_BE {
        return None;
    }
    let page_size = be32(&head[8..12]);
    // A page size outside SQLite's range means this is not a log we can walk.
    if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
        return None;
    }
    let (salt1, salt2) = (be32(&head[16..20]), be32(&head[20..24]));

    let stride = FRAME_HEADER as u64 + page_size as u64;
    let total_frames = ((size - WAL_HEADER as u64) / stride) as u32;

    // Walk every frame header to count the live region and its commits, then
    // keep only the requested window for display.
    let mut frames: Vec<Frame> = Vec::new();
    let (mut live_frames, mut commits) = (0u32, 0u32);
    let mut buf = [0u8; FRAME_HEADER];
    for i in 0..total_frames {
        let at = WAL_HEADER as u64 + i as u64 * stride;
        if f.seek(SeekFrom::Start(at)).is_err() || f.read_exact(&mut buf).is_err() {
            break;
        }
        let page = be32(&buf[0..4]);
        let db_size = be32(&buf[4..8]);
        let live = be32(&buf[8..12]) == salt1 && be32(&buf[12..16]) == salt2;
        if live {
            live_frames += 1;
            if db_size != 0 {
                commits += 1;
            }
        }
        frames.push(Frame {
            index: i + 1,
            page,
            db_size,
            commit: db_size != 0,
            live,
        });
    }
    if frames.len() > window {
        frames.drain(..frames.len() - window);
    }

    Some(WalTail {
        path,
        page_size,
        checkpoint_seq: be32(&head[12..16]),
        salt1,
        salt2,
        live_frames,
        total_frames,
        commits,
        size,
        frames,
    })
}

/// Frames written since frame `after`, for a caller tailing a log it has already
/// read part of. Reading only the new frame headers is what keeps attribution cheap
/// on a log with tens of thousands of frames: the alternative walks all of them
/// every sample.
///
/// A salt change means the log was checkpointed and restarted, so the caller's
/// `after` no longer refers to anything. That is reported rather than papered over
/// — `restarted` is the signal to start counting from zero again.
pub struct NewFrames {
    pub page_size: u32,
    pub salt1: u32,
    pub salt2: u32,
    /// The log was reset since the caller last looked.
    pub restarted: bool,
    /// Frames after the caller's mark, oldest first.
    pub frames: Vec<Frame>,
    /// Frames present in the file now.
    pub total_frames: u32,
}

pub fn read_frames_after(db: &Path, after: u32, salts: Option<(u32, u32)>) -> Option<NewFrames> {
    let path = wal_path(db);
    let size = std::fs::metadata(&path).ok()?.len();
    if size < WAL_HEADER as u64 {
        return None;
    }
    let mut f = std::fs::File::open(&path).ok()?;
    let mut head = [0u8; WAL_HEADER];
    f.read_exact(&mut head).ok()?;
    let magic = be32(&head[0..4]);
    if magic != MAGIC_LE && magic != MAGIC_BE {
        return None;
    }
    let page_size = be32(&head[8..12]);
    if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
        return None;
    }
    let (salt1, salt2) = (be32(&head[16..20]), be32(&head[20..24]));
    let restarted = salts.is_some_and(|(s1, s2)| s1 != salt1 || s2 != salt2);
    let start = if restarted { 0 } else { after };

    let stride = FRAME_HEADER as u64 + page_size as u64;
    let total_frames = ((size - WAL_HEADER as u64) / stride) as u32;
    let mut frames = Vec::new();
    let mut buf = [0u8; FRAME_HEADER];
    for i in start..total_frames {
        let at = WAL_HEADER as u64 + i as u64 * stride;
        if f.seek(SeekFrom::Start(at)).is_err() || f.read_exact(&mut buf).is_err() {
            break;
        }
        let db_size = be32(&buf[4..8]);
        frames.push(Frame {
            index: i + 1,
            page: be32(&buf[0..4]),
            db_size,
            commit: db_size != 0,
            live: be32(&buf[8..12]) == salt1 && be32(&buf[12..16]) == salt2,
        });
    }
    Some(NewFrames {
        page_size,
        salt1,
        salt2,
        restarted,
        frames,
        total_frames,
    })
}

/// The newest image of every page the live log holds, with the database size the
/// last commit recorded.
///
/// In WAL mode this *is* the current state of those pages: the database file still
/// holds the pre-write version until a checkpoint. Anything reading the file
/// directly — the page map behind write attribution, a recovery pass — has to
/// apply this on top or it is reading the past.
///
/// Only the newest image of each page is kept, so the cost is bounded by the
/// database's page count rather than by the log's length.
pub fn latest_pages(db: &Path) -> Option<(u32, std::collections::HashMap<u32, Vec<u8>>, u32)> {
    let path = wal_path(db);
    let size = std::fs::metadata(&path).ok()?.len();
    if size < WAL_HEADER as u64 {
        return None;
    }
    let mut f = std::fs::File::open(&path).ok()?;
    let mut head = [0u8; WAL_HEADER];
    f.read_exact(&mut head).ok()?;
    let magic = be32(&head[0..4]);
    if magic != MAGIC_LE && magic != MAGIC_BE {
        return None;
    }
    let page_size = be32(&head[8..12]);
    if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
        return None;
    }
    let (salt1, salt2) = (be32(&head[16..20]), be32(&head[20..24]));
    let stride = FRAME_HEADER as u64 + page_size as u64;
    let total = ((size - WAL_HEADER as u64) / stride) as u32;

    let mut pages: std::collections::HashMap<u32, Vec<u8>> = std::collections::HashMap::new();
    let mut db_size = 0u32;
    let mut fh = [0u8; FRAME_HEADER];
    let mut image = vec![0u8; page_size as usize];
    for i in 0..total {
        let at = WAL_HEADER as u64 + i as u64 * stride;
        if f.seek(SeekFrom::Start(at)).is_err() || f.read_exact(&mut fh).is_err() {
            break;
        }
        // A frame from an older log is not part of the current state.
        if be32(&fh[8..12]) != salt1 || be32(&fh[12..16]) != salt2 {
            break;
        }
        if f.read_exact(&mut image).is_err() {
            break;
        }
        let page = be32(&fh[0..4]);
        let after = be32(&fh[4..8]);
        if after != 0 {
            db_size = after;
        }
        // A later frame for the same page supersedes an earlier one.
        pages.insert(page, image.clone());
    }
    Some((page_size, pages, db_size))
}

/// The page image a frame carries, which is the state of that page as of that
/// write. This is the only place in zdbview that reads a frame's payload rather
/// than its header.
pub fn frame_page(db: &Path, index: u32) -> Option<(u32, Vec<u8>)> {
    let path = wal_path(db);
    let mut f = std::fs::File::open(&path).ok()?;
    let mut head = [0u8; WAL_HEADER];
    f.read_exact(&mut head).ok()?;
    let page_size = be32(&head[8..12]);
    if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() || index == 0 {
        return None;
    }
    let stride = FRAME_HEADER as u64 + page_size as u64;
    let at = WAL_HEADER as u64 + (index as u64 - 1) * stride;
    f.seek(SeekFrom::Start(at)).ok()?;
    let mut fh = [0u8; FRAME_HEADER];
    f.read_exact(&mut fh).ok()?;
    let mut page = vec![0u8; page_size as usize];
    f.read_exact(&mut page).ok()?;
    Some((be32(&fh[0..4]), page))
}

/// WAL integers are big-endian regardless of the host.
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("zdbview_wal_{}_{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(wal_path(&p));
        p
    }

    /// A real database in WAL mode, left uncheckpointed so the log survives the
    /// connection: `journal_size_limit`/autocheckpoint off, connection kept open
    /// by the caller.
    fn wal_db(name: &str, rows: usize) -> (PathBuf, rusqlite::Connection) {
        let path = scratch(name);
        let conn = rusqlite::Connection::open(&path).expect("open");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        conn.pragma_update(None, "wal_autocheckpoint", 0)
            .expect("no autockpt");
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .expect("create");
        for i in 0..rows {
            conn.execute("INSERT INTO t (v) VALUES (?1)", [format!("row-{i}")])
                .expect("insert");
        }
        (path, conn)
    }

    #[test]
    fn no_wal_file_is_not_an_error() {
        let p = scratch("absent");
        std::fs::write(&p, b"not a database").unwrap();
        assert!(
            read_tail(&p, 32).is_none(),
            "a missing log reads as None, not a failure"
        );
    }

    #[test]
    fn a_non_wal_file_is_rejected_by_its_magic() {
        let p = scratch("garbage");
        std::fs::write(&p, b"db").unwrap();
        std::fs::write(wal_path(&p), vec![0xab; 4096]).unwrap();
        assert!(
            read_tail(&p, 32).is_none(),
            "the header magic gates the walk"
        );
    }

    #[test]
    fn frames_carry_pages_and_commit_boundaries() {
        let (path, _conn) = wal_db("frames", 5);

        let tail = read_tail(&path, 64).expect("a log exists");
        assert!(tail.page_size >= 512 && tail.page_size.is_power_of_two());
        assert!(tail.total_frames > 0, "writes produced frames");
        assert_eq!(
            tail.live_frames, tail.total_frames,
            "nothing checkpointed yet"
        );
        assert!(tail.commits >= 5, "each insert commits: {}", tail.commits);
        assert!(
            tail.frames.iter().any(|f| f.commit && f.db_size > 0),
            "a commit frame carries the post-commit page count"
        );
        assert!(
            tail.frames.iter().all(|f| f.page > 0),
            "page numbers are 1-based"
        );
        assert!(tail.distinct_pages() > 0);
    }

    #[test]
    fn the_window_keeps_the_newest_frames() {
        let (path, _conn) = wal_db("window", 12);

        let all = read_tail(&path, 1_000).expect("log");
        let last3 = read_tail(&path, 3).expect("log");
        assert_eq!(last3.frames.len(), 3, "the window caps what is returned");
        assert_eq!(
            last3.frames.last().map(|f| f.index),
            all.frames.last().map(|f| f.index),
            "the tail is the NEWEST frames, not the oldest"
        );
        assert_eq!(
            last3.total_frames, all.total_frames,
            "the counts describe the whole log, not the window"
        );
    }

    #[test]
    fn a_checkpoint_makes_older_frames_stale() {
        let (path, conn) = wal_db("checkpoint", 4);
        let before = read_tail(&path, 1_000).expect("log");
        assert_eq!(before.live_frames, before.total_frames);

        // TRUNCATE would remove the file; RESTART keeps it and bumps the salts,
        // which is exactly the state where old frames linger with dead salts.
        conn.pragma_update(None, "wal_checkpoint", "RESTART")
            .expect("checkpoint");
        conn.execute("INSERT INTO t (v) VALUES ('after')", [])
            .expect("insert");

        let after = read_tail(&path, 1_000).expect("log");
        assert_ne!(
            (after.salt1, after.salt2),
            (before.salt1, before.salt2),
            "a checkpoint rolls the salts"
        );
        assert!(
            after.live_frames < after.total_frames,
            "frames from the previous log are still present but no longer live: {} of {}",
            after.live_frames,
            after.total_frames
        );
        assert!(
            after.frames.iter().any(|f| !f.live),
            "the stale region is reported, not hidden"
        );
    }
}
