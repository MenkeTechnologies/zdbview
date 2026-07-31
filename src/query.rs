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
                Err(_) => return None,
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
