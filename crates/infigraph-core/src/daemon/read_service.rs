//! The daemon's read service.
//!
//! Structurally parallel to the write coordinator and sharing nothing with
//! it but the process and the `GraphStore` handle. The write pipeline is a
//! coalescing pipeline -- queue, dedupe, serialize, lock -- because writes
//! want fan-in. Reads want fan-out, so this takes no lock, joins no queue,
//! and never runs on the watch loop.
//!
//! It takes `Arc<GraphStore>`, never a bare `kuzu::Database`. Opening
//! through the store is what carries `validate_db_file`'s truncation
//! preflight, `refuse_newer_schema`, the bounded write buffer pool and the
//! `write_phase` breadcrumbs -- and, more importantly, a second `Database`
//! handle on one graph file cannot see the writer's uncommitted WAL even
//! inside a single process. That is #149 reproduced inside the daemon, and
//! it would pass every test that has no concurrent writer.

use anyhow::Result;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use super::read_endpoint::ReadEndpoint;
use super::read_protocol::{read_request, write_frame, ReadFrame, Store};

/// Resolves the store to serve a request from, at request time.
///
/// Deliberately not a captured `Arc<GraphStore>`: the daemon opens its
/// `Infigraph` lazily and drops it again on `poison_watch_db` (after a full
/// reindex swaps the graph file, say), so a service holding the store it saw
/// at startup would go on serving a replaced `Database`. Resolving per
/// request is what keeps the "exactly one `Database`" invariant true over
/// the daemon's whole lifetime, not just at its first instant.
pub type StoreSource = Arc<dyn Fn() -> Option<Arc<crate::graph::GraphStore>> + Send + Sync>;

/// Executes one read query against a store this crate cannot name.
///
/// `infigraph-docs` depends on `infigraph-core`, not the reverse, so the
/// daemon cannot hold a `DocStore` directly. It holds this instead: a
/// closure supplied by whoever owns that store, which runs the query and
/// returns the same stringly rows the graph path does. The closure is
/// responsible for its own read-only guard -- it has the connection, and
/// `read_guard::ensure_read_only` is public.
pub type RowSource = Arc<dyn Fn(&str) -> Result<Vec<Vec<String>>> + Send + Sync>;

/// Marker in the error frame sent when the daemon is up and listening but
/// has not opened its graph yet.
///
/// A distinct, retryable condition rather than a generic failure: the
/// endpoint binds before the daemon builds its language registry (seconds in
/// a debug build), so a client can connect successfully and still arrive
/// before there is anything to read. `RemoteExec` waits this out within the
/// same bounded, daemon-alive-gated grace it uses for connect failures.
pub const NOT_READY: &str = "the daemon has no graph open yet";

pub struct ReadService {
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
    endpoint: ReadEndpoint,
}

impl ReadService {
    /// Bind the endpoint for `root` and serve reads from one fixed store.
    ///
    /// For callers that genuinely own the store for the service's whole
    /// lifetime -- tests, mostly. The daemon uses [`start_with_source`].
    ///
    /// [`start_with_source`]: ReadService::start_with_source
    pub fn start(
        root: &Path,
        store: Arc<crate::graph::GraphStore>,
        workers: usize,
    ) -> Result<Self> {
        Self::start_with_source(root, Arc::new(move || Some(store.clone())), workers)
    }

    /// Bind the endpoint for `root` and resolve the store per request.
    ///
    /// Binding happens before this returns, so a client that connects
    /// immediately afterwards cannot race the listener into existence.
    pub fn start_with_source(root: &Path, source: StoreSource, workers: usize) -> Result<Self> {
        Self::start_with_sources(root, source, None, workers)
    }

    /// As [`start_with_source`], plus a document-store source.
    ///
    /// `search` with `scope='all'` touches both stores in one call, so
    /// routing only the graph would leave that read still opening
    /// `docs.kuzu` directly -- which has its own lock file and its own
    /// wipe-on-any-open-failure history (#143).
    ///
    /// [`start_with_source`]: ReadService::start_with_source
    pub fn start_with_sources(
        root: &Path,
        source: StoreSource,
        docs: Option<RowSource>,
        workers: usize,
    ) -> Result<Self> {
        let endpoint = ReadEndpoint::for_root(root);
        let listener = endpoint.bind()?;
        let stop = Arc::new(AtomicBool::new(false));

        let pool = Pool::new(workers);
        let accept_stop = stop.clone();
        let accept = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if accept_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let source = source.clone();
                let docs = docs.clone();
                pool.execute(move || {
                    if let Err(e) = serve_one(&source, docs.as_ref(), stream) {
                        eprintln!("[read] connection failed: {e:#}");
                    }
                });
            }
            // Dropping the pool closes the job channel and joins the
            // workers, so an in-flight read finishes before the daemon
            // tears the store down under it.
            drop(pool);
        });

        Ok(Self {
            stop,
            accept: Some(accept),
            endpoint,
        })
    }

    /// Stop accepting and join the accept thread.
    ///
    /// Also runs on drop, so every early return from the daemon's
    /// coordinator tears the service down without a bespoke exit path.
    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        // Idempotent: once the handle is taken there is nothing to join, so
        // an explicit `shutdown()` followed by the drop is a no-op.
        let Some(h) = self.accept.take() else { return };
        self.stop.store(true, Ordering::Relaxed);
        // Unblock `accept` by connecting to ourselves once.
        let _ = self.endpoint.connect();
        let _ = h.join();
    }
}

impl Drop for ReadService {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn serve_one<S: std::io::Read + std::io::Write>(
    source: &StoreSource,
    docs: Option<&RowSource>,
    mut stream: S,
) -> Result<()> {
    let req = read_request(&mut stream)?;

    // The document store is a separate `Database` with its own lock file and
    // its own wipe-on-open-failure history (#143), reached through a closure
    // because this crate cannot name `DocStore`.
    if req.store == Store::Docs {
        let Some(docs) = docs else {
            write_frame(
                &mut stream,
                &ReadFrame::Error(
                    "this daemon has no document store registered; it serves the code graph only"
                        .to_string(),
                ),
            )?;
            return Ok(());
        };
        match docs(&req.query) {
            Ok(rows) => {
                for chunk in rows.chunks(req.chunk_size.max(1)) {
                    write_frame(&mut stream, &ReadFrame::Rows(chunk.to_vec()))?;
                }
                write_frame(&mut stream, &ReadFrame::End)?;
            }
            Err(e) => write_frame(&mut stream, &ReadFrame::Error(e.to_string()))?,
        }
        return Ok(());
    }

    // Resolved now, not at startup, and held for this one request -- so a
    // concurrent `poison_watch_db` cannot close the `Database` underneath a
    // read already in flight.
    let Some(store) = source() else {
        write_frame(
            &mut stream,
            // Advice, not a promise. "retry once indexing has started" named
            // a precondition that could never arrive whenever the daemon was
            // not going to index again -- with the #100 breaker engaged, for
            // instance, every drain is refused and there is no "once" to wait
            // for. `RemoteExec` matches on `NOT_READY` alone, so this text is
            // free to say something true instead.
            &ReadFrame::Error(format!(
                "{NOT_READY}; the endpoint binds before the graph opens, so this usually \
                 clears within seconds. If it persists, check `infigraph ps` and the \
                 daemon log for why the graph did not open"
            )),
        )?;
        return Ok(());
    };
    let store = store.as_ref();

    // Same call the write path uses -- one Database, one buffer pool, one
    // WAL.
    let conn = store.connection()?;
    if let Err(e) = super::read_guard::ensure_read_only(&conn, &req.query) {
        write_frame(&mut stream, &ReadFrame::Error(e.to_string()))?;
        return Ok(());
    }
    drop(conn);

    // Execute through the EXISTING backend body, not a hand-rolled
    // connection + stringify: `raw_query_on` already returns
    // `Vec<Vec<String>>` -- the exact wire shape -- and already handles bare
    // BEGIN/COMMIT/ROLLBACK, which a hand-rolled path here would drop.
    match crate::graph::raw_query_on(store, &req.query) {
        Ok(rows) => {
            for chunk in rows.chunks(req.chunk_size.max(1)) {
                write_frame(&mut stream, &ReadFrame::Rows(chunk.to_vec()))?;
            }
            // Always sent, including for zero rows: this frame is what makes
            // an empty result distinguishable from a truncated stream.
            write_frame(&mut stream, &ReadFrame::End)?;
        }
        Err(e) => write_frame(&mut stream, &ReadFrame::Error(e.to_string()))?,
    }
    Ok(())
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A fixed-size worker pool.
///
/// Fixed, not unbounded: a burst of clients must wait for a free worker
/// rather than spawn unbounded threads inside the daemon. Hand-rolled
/// rather than a new dependency -- the crate spawns threads directly
/// elsewhere and this is a channel plus N workers.
struct Pool {
    tx: Option<mpsc::Sender<Job>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl Pool {
    fn new(workers: usize) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let workers = (0..workers.max(1))
            .map(|_| {
                let rx = rx.clone();
                std::thread::spawn(move || loop {
                    // The guard is scoped to this statement so the next
                    // worker can take the lock while this one runs its job.
                    let job = {
                        let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
                        guard.recv()
                    };
                    match job {
                        Ok(job) => job(),
                        Err(_) => break,
                    }
                })
            })
            .collect();
        Self {
            tx: Some(tx),
            workers,
        }
    }

    fn execute(&self, job: impl FnOnce() + Send + 'static) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Box::new(job));
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // Closing the channel is how workers learn to exit.
        self.tx.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
