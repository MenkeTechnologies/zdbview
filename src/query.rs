//! Queries the grid needs, run off the render thread.
//!
//! Every SQLite call used to happen on the thread that draws, so a scan the user
//! could not see froze the UI for as long as it took. On a 23 GB database that is
//! not a stutter: filtering a 6.5M-row table meant a 4.16 s full scan per
//! keystroke, with one core pegged and no way to stop it.
//!
//! Three workers fix that, each with a read-only connection of its own — so a
//! database another process is writing can never be made to checkpoint its WAL
//! just because it is being browsed:
//!
//! * **pages** — one page of rows. Must stay responsive, so it never runs
//!   anything unbounded: the total it reports is what one extra fetched row
//!   proves, never something counted.
//! * **counts** — the exact total behind a filter, which is a full scan by
//!   definition and is parallelised across cores inside
//!   [`SqliteStore::count_exact`].
//! * **searches** — one `n` / `N` step, which scans for the match and then counts
//!   to it to work out which page to load. Kept off the count worker so a search
//!   is never queued behind one.
//!
//! All three are cancelled the same way. Every request carries a generation; the UI
//! bumps the generation when it asks for something newer, and each connection
//! runs a progress handler that aborts the statement as soon as its own
//! generation is stale. A burst of keystrokes therefore costs one query, not one
//! per key — and no timer, no debounce guess.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;

use crate::sqlite::{PageHint, RowsView, Sort, SqliteStore};

/// How often a connection checks whether its work is still wanted, in SQLite VM
/// instructions. Small enough that an abandoned scan dies within milliseconds,
/// large enough not to matter to a query that runs to completion.
const CANCEL_CHECK_OPS: i32 = 2_000;

/// What page the grid wants.
#[derive(Debug, Clone)]
pub struct PageReq {
    pub table: String,
    pub limit: i64,
    pub offset: i64,
    pub sort: Option<Sort>,
    pub filter: String,
    pub hint: Option<PageHint>,
    /// A counted total for this table and filter, when the UI already has one.
    pub known_total: Option<i64>,
    /// Per-column display formats — see [`crate::browse::Format`]. Cloned into
    /// the request because the worker runs on its own thread.
    pub formats: std::collections::HashMap<String, crate::browse::Format>,
}

impl PageReq {
    /// The store query this request runs. Shared by the worker thread and by the
    /// synchronous path the app takes while an edit is unwritten, so both fetch
    /// the same page for the same request.
    pub fn query(&self) -> crate::sqlite::PageQuery<'_> {
        crate::sqlite::PageQuery {
            table: &self.table,
            limit: self.limit,
            offset: self.offset,
            sort: self.sort.as_ref(),
            filter: &self.filter,
            hint: self.hint.as_ref(),
            known_total: self.known_total,
            formats: &self.formats,
        }
    }
}

/// What total the title wants.
#[derive(Debug, Clone)]
pub struct CountReq {
    pub table: String,
    pub filter: String,
}

/// A whole-table search for `n` / `N`.
#[derive(Debug, Clone)]
pub struct SearchReq {
    pub table: String,
    pub columns: Vec<String>,
    pub term: String,
    pub sort: Option<Sort>,
    pub filter: String,
    /// Row the search starts from, or `None` to come in from the edge.
    pub from: Option<i64>,
    pub forward: bool,
}

/// Where a search landed: the row and its 1-based position in the display order,
/// which is what says which page to load.
pub struct SearchDone {
    pub generation: u64,
    pub result: Result<Option<(i64, i64)>, String>,
}

/// A finished page, tagged with the generation that asked for it.
pub struct PageDone {
    pub generation: u64,
    pub result: Result<RowsView, String>,
}

/// A finished exact count. The table and filter come back too, so a result that
/// no longer describes what is on screen can be dropped.
pub struct CountDone {
    pub generation: u64,
    pub table: String,
    pub filter: String,
    pub result: Result<i64, String>,
}

/// One worker: a thread, its request channel, and the generation it is allowed
/// to be working on.
struct Worker<Req, Out> {
    jobs: Sender<(u64, Req)>,
    out: Receiver<Out>,
    /// The newest generation the UI has asked for. The worker's progress handler
    /// compares against this, so bumping it cancels whatever is running.
    live: Arc<AtomicU64>,
    /// A request is outstanding, so the UI should say so.
    inflight: bool,
}

impl<Req, Out> Worker<Req, Out> {
    fn send(&mut self, generation: u64, req: Req) {
        self.live.store(generation, Ordering::Relaxed);
        if self.jobs.send((generation, req)).is_ok() {
            self.inflight = true;
        }
    }

    /// Take a finished result, if one has arrived.
    fn poll(&mut self) -> Option<Out> {
        match self.out.try_recv() {
            Ok(v) => {
                self.inflight = false;
                Some(v)
            }
            Err(TryRecvError::Empty) => None,
            // The thread is gone; nothing more will arrive.
            Err(TryRecvError::Disconnected) => {
                self.inflight = false;
                None
            }
        }
    }
}

/// The grid's query engine: what the UI holds instead of a connection.
pub struct Engine {
    pages: Worker<PageReq, PageDone>,
    counts: Worker<CountReq, CountDone>,
    /// `n` / `N`, which scans until it finds a match and then counts to work out
    /// which page that is — both full scans in the worst case.
    searches: Worker<SearchReq, SearchDone>,
    /// Monotonic request number, shared by every worker so results are easy to
    /// correlate.
    next_generation: u64,
    /// Generation of the page currently on screen.
    pub page_generation: u64,
}

impl Engine {
    /// Start the workers against `path`. They open the file read-only, so a
    /// database another process is writing is never disturbed by being browsed.
    pub fn new(path: &std::path::Path) -> Engine {
        Engine {
            pages: spawn_worker(path.to_path_buf(), |store, generation, req: PageReq| {
                PageDone {
                    generation,
                    result: store.rows(&req.query()).map_err(|e| e.to_string()),
                }
            }),
            counts: spawn_worker(path.to_path_buf(), |store, generation, req: CountReq| {
                CountDone {
                    generation,
                    result: store
                        .count_exact(&req.table, &req.filter)
                        .map_err(|e| e.to_string()),
                    table: req.table,
                    filter: req.filter,
                }
            }),
            searches: spawn_worker(path.to_path_buf(), |store, generation, req: SearchReq| {
                SearchDone {
                    generation,
                    result: search(store, &req),
                }
            }),
            next_generation: 1,
            page_generation: 0,
        }
    }

    /// Ask for a page and, unless the total is already known, for the count that
    /// goes with it. Returns the generation the page will come back under.
    pub fn request(&mut self, page: PageReq, count: Option<CountReq>) -> u64 {
        let generation = self.next_generation;
        self.next_generation += 1;
        self.pages.send(generation, page);
        if let Some(count) = count {
            self.counts.send(generation, count);
        }
        generation
    }

    /// Ask only for an exact total — what pressing `G` needs before it can know
    /// which page the last one is.
    pub fn request_count(&mut self, count: CountReq) {
        let generation = self.next_generation;
        self.next_generation += 1;
        self.counts.send(generation, count);
    }

    /// Start a search. Its result arrives through [`Self::poll_search`].
    pub fn request_search(&mut self, req: SearchReq) {
        let generation = self.next_generation;
        self.next_generation += 1;
        self.searches.send(generation, req);
    }

    pub fn poll_search(&mut self) -> Option<SearchDone> {
        let live = self.searches.live.load(Ordering::Relaxed);
        while let Some(done) = self.searches.poll() {
            if done.generation == live {
                return Some(done);
            }
        }
        None
    }

    pub fn searching(&self) -> bool {
        self.searches.inflight
    }

    pub fn page_inflight(&self) -> bool {
        self.pages.inflight
    }

    pub fn count_inflight(&self) -> bool {
        self.counts.inflight
    }

    /// The newest page result, if one has arrived. Results older than the
    /// newest request are dropped here rather than bothering the caller.
    pub fn poll_page(&mut self) -> Option<PageDone> {
        let live = self.pages.live.load(Ordering::Relaxed);
        while let Some(done) = self.pages.poll() {
            if done.generation == live {
                self.page_generation = done.generation;
                return Some(done);
            }
        }
        None
    }

    /// Wait up to `grace` for the newest page, then give up on it.
    ///
    /// Most pages are cheap — a bounded count plus a cursor fetch is milliseconds
    /// even on a multi-gigabyte table — and waiting that long for them means the
    /// grid is drawn once with its rows rather than blank and then filled. A page
    /// that needs longer than this is the one that must not block the render
    /// thread, so it is left to [`Self::poll_page`].
    pub fn wait_page(&mut self, grace: std::time::Duration) -> Option<PageDone> {
        let deadline = std::time::Instant::now() + grace;
        loop {
            if let Some(done) = self.poll_page() {
                return Some(done);
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.pages.out.recv_timeout(left) {
                Ok(done) => {
                    self.pages.inflight = false;
                    if done.generation == self.pages.live.load(Ordering::Relaxed) {
                        self.page_generation = done.generation;
                        return Some(done);
                    }
                }
                // The worker is gone — a database it could not open — so nothing
                // is outstanding any more and the grid must stop saying one is.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.pages.inflight = false;
                    return None;
                }
                // Still working: the page missed its grace and is left to
                // `poll_page`, which is what the grace period is for.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return None,
            }
        }
    }

    pub fn poll_count(&mut self) -> Option<CountDone> {
        let live = self.counts.live.load(Ordering::Relaxed);
        while let Some(done) = self.counts.poll() {
            if done.generation == live {
                return Some(done);
            }
        }
        None
    }
}

/// One `n` / `N` step: find the next matching row in display order, wrapping at
/// the end, then work out its position so the caller knows which page to load.
fn search(store: &SqliteStore, req: &SearchReq) -> Result<Option<(i64, i64)>, String> {
    let query = crate::sqlite::RowQuery {
        table: &req.table,
        columns: &req.columns,
        term: &req.term,
        sort: req.sort.as_ref(),
        filter: &req.filter,
    };
    let first = match req.from {
        Some(from) => store.find_row(&query, from, req.forward),
        None => store.find_row_edge(&query, req.forward),
    };
    let found = match first {
        Err(e) => return Err(e.to_string()),
        Ok(Some(r)) => Some(r),
        // Nothing ahead: wrap to the first/last match in display order.
        Ok(None) => store
            .find_row_edge(&query, req.forward)
            .map_err(|e| e.to_string())?,
    };
    let Some(rowid) = found else {
        return Ok(None);
    };
    let ordinal = store
        .rowid_ordinal(&req.table, rowid, req.sort.as_ref(), &req.filter)
        .unwrap_or(1);
    Ok(Some((rowid, ordinal)))
}

/// Spawn a worker thread that answers `Req` with `Out` over its own read-only
/// connection.
///
/// The thread coalesces: when several requests are already queued only the last
/// is run, because an intermediate keystroke's page is never wanted. Together
/// with the progress-handler cancellation that makes a fast typist cost one
/// query rather than one per key.
fn spawn_worker<Req, Out, F>(path: PathBuf, work: F) -> Worker<Req, Out>
where
    Req: Send + 'static,
    Out: Send + 'static,
    F: Fn(&SqliteStore, u64, Req) -> Out + Send + 'static,
{
    let (jobs_tx, jobs_rx) = std::sync::mpsc::channel::<(u64, Req)>();
    let (out_tx, out_rx) = std::sync::mpsc::channel::<Out>();
    let live = Arc::new(AtomicU64::new(0));
    let live_thread = Arc::clone(&live);

    std::thread::spawn(move || {
        let mut store = match SqliteStore::open_readonly(&path) {
            Ok(s) => s,
            // Nothing can be answered; dropping the sender tells the UI.
            Err(_) => return,
        };
        // The generation this connection is working on. The handler compares it
        // with `live`, so the UI cancels by bumping `live` alone.
        let mine = Arc::new(AtomicU64::new(0));
        let (live_cb, mine_cb) = (Arc::clone(&live_thread), Arc::clone(&mine));
        store.set_cancel(
            CANCEL_CHECK_OPS,
            Arc::new(move || live_cb.load(Ordering::Relaxed) != mine_cb.load(Ordering::Relaxed)),
        );

        while let Ok(job) = jobs_rx.recv() {
            // Skip to the newest queued request.
            let (generation, req) = {
                let mut latest = job;
                while let Ok(next) = jobs_rx.try_recv() {
                    latest = next;
                }
                latest
            };
            // Already superseded while it sat in the queue.
            if generation != live_thread.load(Ordering::Relaxed) {
                continue;
            }
            mine.store(generation, Ordering::Relaxed);
            if out_tx.send(work(&store, generation, req)).is_err() {
                return;
            }
        }
    });

    Worker {
        jobs: jobs_tx,
        out: out_rx,
        live,
        inflight: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{search, CountReq, Engine, PageReq, SearchReq};
    use crate::sqlite::SqliteStore;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// A database of `n` rows, `a` counting up and `b` constant.
    fn db(name: &str, n: i64) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("zdbview_query_{}_{}.db", std::process::id(), name));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE t (a TEXT, b TEXT)", []).unwrap();
        for i in 0..n {
            conn.execute("INSERT INTO t VALUES (?1, 'same')", [i.to_string()])
                .unwrap();
        }
        path
    }

    fn page(offset: i64) -> PageReq {
        PageReq {
            table: "t".into(),
            limit: 10,
            offset,
            sort: None,
            filter: String::new(),
            hint: None,
            known_total: None,
            formats: std::collections::HashMap::new(),
        }
    }

    /// A burst of keystrokes asks for several pages; only the newest is ever
    /// handed back, and it is the one the grid draws.
    #[test]
    fn only_the_newest_page_reaches_the_caller() {
        let path = db("burst", 200);
        let mut engine = Engine::new(&path);

        let mut last = 0;
        for offset in [0, 10, 20, 30, 40] {
            last = engine.request(page(offset), None);
        }
        let done = engine
            .wait_page(Duration::from_secs(5))
            .expect("the newest page arrives");
        assert_eq!(done.generation, last, "the newest generation, not an older one");
        assert_eq!(engine.page_generation, last);
        let view = done.result.expect("rows");
        assert_eq!(view.rows[0][0], "40", "the page that was asked for last");

        // Nothing stale is queued behind it.
        assert!(engine.poll_page().is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// Generations are handed out in order and identify the request, which is
    /// what lets a late result be dropped.
    #[test]
    fn every_request_gets_the_next_generation() {
        let path = db("gens", 5);
        let mut engine = Engine::new(&path);
        let first = engine.request(page(0), None);
        let second = engine.request(page(0), None);
        assert_eq!(second, first + 1);
        engine.request_count(CountReq {
            table: "t".into(),
            filter: String::new(),
        });
        let third = engine.request(page(0), None);
        assert_eq!(third, second + 2, "the count consumed one too");
        let _ = std::fs::remove_file(&path);
    }

    /// A count comes back saying what it counted, so a total that no longer
    /// describes the screen can be thrown away instead of shown.
    #[test]
    fn a_count_says_what_it_counted() {
        let path = db("counts", 60);
        let mut engine = Engine::new(&path);
        engine.request_count(CountReq {
            table: "t".into(),
            filter: "4".into(),
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let done = loop {
            if let Some(d) = engine.poll_count() {
                break d;
            }
            assert!(Instant::now() < deadline, "the count never arrived");
        };
        assert_eq!(done.table, "t");
        assert_eq!(done.filter, "4");
        // 4, 14, 24, 34, 40..49, 54.
        assert_eq!(done.result.unwrap(), 15);
        let _ = std::fs::remove_file(&path);
    }

    /// `wait_page` is a grace period, not a block: with nothing outstanding it
    /// gives up and lets the frame draw.
    #[test]
    fn waiting_for_a_page_nobody_asked_for_gives_up() {
        let path = db("idle", 1);
        let mut engine = Engine::new(&path);
        let start = Instant::now();
        assert!(engine.wait_page(Duration::from_millis(80)).is_none());
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(70), "it waited: {waited:?}");
        assert!(waited < Duration::from_secs(2), "but not forever: {waited:?}");
        let _ = std::fs::remove_file(&path);
    }

    /// A database the workers cannot open must not hang the UI: the threads exit
    /// and the grid is told there is no page rather than waiting on one.
    #[test]
    fn a_database_that_cannot_be_opened_does_not_hang_the_grid() {
        let path = std::env::temp_dir().join(format!(
            "zdbview_query_{}_absent.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut engine = Engine::new(&path);
        engine.request(page(0), None);
        let start = Instant::now();
        assert!(engine.wait_page(Duration::from_secs(2)).is_none());
        assert!(start.elapsed() < Duration::from_secs(2), "it did not sit out the grace");
        assert!(!engine.page_inflight(), "and it stopped saying it was working");
    }

    /// `n` wraps at the end of the table rather than reporting no match, and
    /// reports the match's position in display order so the caller knows which
    /// page to load.
    #[test]
    fn a_search_wraps_and_reports_the_position() {
        let path = db("search", 30);
        let store = SqliteStore::open_readonly(&path).unwrap();
        let req = |from: Option<i64>, forward: bool| SearchReq {
            table: "t".into(),
            columns: vec!["a".into(), "b".into()],
            term: "29".into(),
            sort: None,
            filter: String::new(),
            from,
            forward,
        };

        // Row 30 holds "29" — the last row, found from the start.
        let (rowid, ordinal) = search(&store, &req(Some(1), true)).unwrap().unwrap();
        assert_eq!(ordinal, 30, "1-based position in display order");

        // Searching forward from the only match wraps back to it.
        let (again, _) = search(&store, &req(Some(rowid), true)).unwrap().unwrap();
        assert_eq!(again, rowid, "the search wrapped instead of giving up");

        // A term nothing holds is no match, not an error.
        let none = search(
            &store,
            &SearchReq {
                term: "no such value".into(),
                ..req(None, true)
            },
        )
        .unwrap();
        assert!(none.is_none());
        let _ = std::fs::remove_file(&path);
    }
}
