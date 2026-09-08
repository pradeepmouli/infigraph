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

pub struct ReadService {
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
    endpoint: ReadEndpoint,
}

impl ReadService {
    /// Bind the endpoint for `root` and start serving reads from `store`.
    ///
    /// Binding happens before this returns, so a client that connects
    /// immediately afterwards cannot race the listener into existence.
    pub fn start(
        root: &Path,
        store: Arc<crate::graph::GraphStore>,
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
                let store = store.clone();
                pool.execute(move || {
                    if let Err(e) = serve_one(&store, stream) {
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

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock `accept` by connecting to ourselves once.
        let _ = self.endpoint.connect();
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

fn serve_one<S: std::io::Read + std::io::Write>(
    store: &crate::graph::GraphStore,
    mut stream: S,
) -> Result<()> {
    let req = read_request(&mut stream)?;

    if req.store != Store::Graph {
        write_frame(
            &mut stream,
            &ReadFrame::Error(
                "this read service serves the code graph only; no document store is registered"
                    .to_string(),
            ),
        )?;
        return Ok(());
    }

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
