pub mod batch;
pub mod daemon;
pub(crate) mod drain;
pub(crate) mod queue;

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::ops::{begin_index_op, IndexOpOutcome};
use crate::Infigraph;
use batch::ChangeBatch;

/// A single file-change event emitted by the watcher.
#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub kind: WatchEventKind,
    pub path: PathBuf,
    /// True if this file has cross-file CALLS edges — full reindex needed to re-resolve them.
    pub has_cross_file_calls: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEventKind {
    Modified,
    Created,
    Removed,
    WatcherRestarted,
    WatcherDied,
}

impl std::fmt::Display for WatchEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            WatchEventKind::Modified => "modified",
            WatchEventKind::Created => "created",
            WatchEventKind::Removed => "removed",
            WatchEventKind::WatcherRestarted => "watcher-restarted",
            WatchEventKind::WatcherDied => "watcher-died",
        };
        if self.has_cross_file_calls {
            write!(
                f,
                "{kind}: {} [cross-file calls detected — full reindex recommended]",
                self.path.display()
            )
        } else {
            write!(f, "{kind}: {}", self.path.display())
        }
    }
}

/// Watch a project directory and auto-reindex on file changes.
///
/// On non-Windows platforms, holds one DB connection open for the whole
/// watch session and reuses it across batches — see `watch_db`. Reopening a
/// fresh connection per batch was the original design, but each open/close
/// cycle forces a full Kuzu checkpoint (`forceCheckpointOnClose`), and at
/// sustained watch frequency (roughly once per second of active editing)
/// that turns a long session into thousands of forced checkpoints, which was
/// found to cause severe embedded-graph-file bloat over time (multi-GB
/// write amplification per cycle at scale, independent of actual data
/// growth). On Windows, mandatory file locking prevents a second concurrent
/// connection while another handle on the same file is open elsewhere, so
/// the original per-batch open/close behavior is kept there.
///
/// Blocks until `stop_rx` receives a signal.
pub fn watch_project<MR>(
    root: &Path,
    make_registry: MR,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    on_event: impl Fn(WatchEvent) + Send + 'static,
) -> Result<()>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry> + Send + 'static,
{
    watch_project_with_periodic(
        root,
        make_registry,
        debounce_ms,
        stop_rx,
        on_event,
        0,
        None::<fn(&crate::IndexResult)>,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn watch_project_with_periodic<MR, F>(
    root: &Path,
    make_registry: MR,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    on_event: impl Fn(WatchEvent) + Send + 'static,
    periodic_secs: u64,
    on_periodic: Option<F>,
    serve_requests: bool,
) -> Result<()>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry> + Send + 'static,
    F: Fn(&crate::IndexResult) + Send + 'static,
{
    // Some watch backends (e.g. FSEvents on macOS) deliver absolute,
    // symlink-resolved event paths regardless of how `root` was specified.
    // If `root` is relative, or traverses a symlink (macOS temp dirs live
    // under /var, itself a symlink to /private/var), `path.strip_prefix(root)`
    // below silently fails for every event and all changes are dropped.
    let root = &root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let ignore_dirs: &[&str] = &[
        ".infigraph",
        ".git",
        "node_modules",
        "__pycache__",
        ".venv",
        "venv",
        "target",
        "build",
        "dist",
        ".tox",
    ];

    // Build a registry once for file-extension filtering (no DB needed).
    let filter_registry = make_registry()?;

    let mut changes_since_periodic: usize = 0;
    let mut last_periodic = std::time::Instant::now();

    // Batch accumulator: collect file changes over a 1-second window
    // then index them all at once using the bulk write path.
    let mut batch = ChangeBatch::new(1000);

    // Shared DB connection for the watch session — see `watch_db`'s doc
    // comment for the platform split (held open on non-Windows, reopened
    // per call on Windows).
    let mut held_prism: Option<Arc<Infigraph>> = None;

    // Accumulates index-shaped work from every producer below (periodic
    // reindex, watch-triggered batch/removal, ad-hoc daemon-protocol
    // requests) so it's drained as one combined execution per tick instead
    // of each producer racing its own stale plan against the others -- see
    // docs/superpowers/specs/2026-08-03-daemon-index-work-queue-design.md.
    //
    // Shared rather than loop-local because the drain runs on a background
    // task: producers below keep filling it, under the mutex, while a drain
    // executes.
    let queue = Arc::new(Mutex::new(crate::watch::queue::IndexWorkQueue::new()));

    // Drains run here instead of inline so a large one (a whole-project
    // reindex can take minutes) doesn't stop this loop from accepting
    // fsevents, periodic ticks and further requests for its whole duration.
    // One worker is enough -- `drain_in_flight` allows at most one drain at
    // a time, and the drain itself is a `spawn_blocking` task.
    let drain_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("infigraph-drain")
        .build()?;
    let mut drain_in_flight: Option<InFlightDrain> = None;

    let sentinel = root.join(".infigraph").join("watch.stop");

    const MAX_RESTARTS: u32 = 3;
    let mut restart_count: u32 = 0;

    // Create initial watcher — factored into a closure for restart.
    let create_watcher =
        |root: &Path,
         ignore_dirs: &[&str]|
         -> Result<(RecommendedWatcher, mpsc::Receiver<notify::Result<Event>>)> {
            let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
            let config = Config::default().with_poll_interval(Duration::from_millis(debounce_ms));
            let mut watcher = RecommendedWatcher::new(tx, config)?;
            register_watch_dirs(&mut watcher, root, ignore_dirs)?;
            Ok((watcher, rx))
        };

    let (mut watcher, mut rx) = create_watcher(root, ignore_dirs)?;

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        if sentinel.exists() {
            let _ = std::fs::remove_file(&sentinel);
            break;
        }

        // Shared drain step, in two halves: reap whatever finished since the
        // last tick (here), then schedule the next one (at the end of the
        // tick). The drain itself combines everything every producer below
        // (periodic mark, ad-hoc requests, batch flush, watch-triggered
        // removal) contributed into ONE execution -- the actual fix for the
        // coalescing bug (see
        // docs/superpowers/specs/2026-08-03-daemon-index-work-queue-design.md).
        // Same lock, same role string, same cross-process contract the
        // batch-flush block used to acquire on its own; only *where* the
        // execution runs changed.
        //
        // Reaping runs first in the tick, before anything else that wants
        // `index.lock`: the task hands the guard back rather than dropping
        // it (so these downstream steps still run under it, exactly as they
        // did when the drain was inline), which means the lock stays held
        // until this block runs. Anything here that blocked on it first
        // would be waiting on itself.
        if drain_in_flight
            .as_ref()
            .is_some_and(|d| d.handle.is_finished())
        {
            let InFlightDrain {
                handle,
                prism,
                waiter_replies,
            } = drain_in_flight.take().expect("checked is_some just above");
            let (guard, outcome) = finish_drain(drain_rt.block_on(handle), &waiter_replies);
            match outcome {
                Some(outcome) => {
                    changes_since_periodic += outcome.extractions.len();
                    if let Some(ref cb) = on_periodic {
                        if !outcome.extractions.is_empty() {
                            cb(&crate::IndexResult {
                                total_files: outcome.extractions.len(),
                                indexed_files: outcome.extractions.len(),
                                extractions: outcome.extractions.clone(),
                                resolve_stats: outcome.resolve_stats.clone(),
                            });
                        }
                    }
                    if let Some(backend) = prism.backend() {
                        let changed: Vec<&str> = outcome
                            .extractions
                            .iter()
                            .map(|e| e.file.as_str())
                            .collect();
                        if !changed.is_empty() {
                            if let Err(e) = crate::embed::update_embeddings(backend, root, &changed)
                            {
                                eprintln!("[watch] embedding update failed: {e}");
                            }
                        }
                    }
                    for extraction in &outcome.extractions {
                        let cross = has_cross_file_calls(&prism, &extraction.file);
                        let abs_path = root.join(&extraction.file);
                        on_event(WatchEvent {
                            kind: WatchEventKind::Modified,
                            path: abs_path,
                            has_cross_file_calls: cross,
                        });
                    }
                }
                // `finish_drain` already logged and replied to the waiters.
                None => poison_watch_db(&mut held_prism),
            }
            drop(guard);
        }

        // Periodic SCIP refresh: if changes accumulated and enough time passed
        if periodic_secs > 0
            && changes_since_periodic > 0
            && last_periodic.elapsed() >= Duration::from_secs(periodic_secs)
        {
            if on_periodic.is_some() {
                // Marks the queue rather than indexing directly -- the
                // shared drain step below runs the actual scan/upsert/
                // resolve pass and invokes `on_periodic` with the real
                // `DrainOutcome`, folded into whatever else this tick's
                // other producers also contributed.
                queue.lock().unwrap().mark_whole_project();
                changes_since_periodic = 0;
                last_periodic = std::time::Instant::now();
            } else {
                changes_since_periodic = 0;
                last_periodic = std::time::Instant::now();
            }
        }

        // Serve file-dropped write requests -- daemon-mode only (never from
        // in-process MCP watcher threads, which always pass
        // serve_requests=false). Piggybacks on this loop's existing tick
        // (at least every 200ms via the rx.recv_timeout below) rather than
        // a separate notify-based watch on the requests directory --
        // submit_write_request's own poll-with-backoff starts at 10ms and
        // only reaches 200ms after several rounds, so this cadence is fine.
        if serve_requests {
            let requests_dir = root.join(".infigraph").join("requests");
            if let Ok(entries) = std::fs::read_dir(&requests_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "request") {
                        route_or_serve_request(
                            root,
                            &path,
                            &queue,
                            &make_registry,
                            &mut held_prism,
                            drain_in_flight.is_some(),
                        );
                    }
                }
            }
        }

        // Flush the batch when the window has closed -- feeds the shared
        // queue rather than indexing directly; the drain step below is what
        // actually executes it, combined with whatever else this tick's
        // other producers also contributed.
        if !batch.is_empty() && batch.is_ready() {
            let paths = batch.drain();
            let mut q = queue.lock().unwrap();
            for path in paths {
                let rel = path
                    .strip_prefix(root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"));
                q.add_raw(rel);
            }
        }

        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(event)) => {
                let watch_kind = match event.kind {
                    EventKind::Create(_) => WatchEventKind::Created,
                    EventKind::Modify(_) => WatchEventKind::Modified,
                    EventKind::Remove(_) => WatchEventKind::Removed,
                    _ => continue,
                };

                for path in event.paths {
                    if should_ignore(&path, ignore_dirs) {
                        continue;
                    }

                    let rel = match path.strip_prefix(root) {
                        Ok(r) => r.to_string_lossy().replace('\\', "/"),
                        Err(_) => continue,
                    };

                    match watch_kind {
                        WatchEventKind::Removed => {
                            // Deferred to the shared drain step below rather
                            // than touching the graph directly here -- this
                            // closes a pre-existing gap where watch-triggered
                            // removal never took `index.lock` at all.
                            queue.lock().unwrap().add_removal(rel);
                            changes_since_periodic += 1;
                            on_event(WatchEvent {
                                kind: watch_kind.clone(),
                                path,
                                has_cross_file_calls: false,
                            });
                        }
                        WatchEventKind::Created | WatchEventKind::Modified => {
                            if path.is_dir() {
                                register_subdirs(&mut watcher, &path, ignore_dirs);
                            } else if filter_registry.for_file(&rel).is_some() {
                                batch.add(path);
                            }
                        }
                        WatchEventKind::WatcherRestarted | WatchEventKind::WatcherDied => {}
                    }
                }
            }
            Ok(Err(e)) => eprintln!("watch error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Watcher's internal thread died (e.g. kqueue panic on dir deletion).
                // Attempt restart with backoff.
                restart_count += 1;
                if restart_count > MAX_RESTARTS {
                    eprintln!("[watch] watcher died {restart_count} times, giving up");
                    on_event(WatchEvent {
                        kind: WatchEventKind::WatcherDied,
                        path: root.to_path_buf(),
                        has_cross_file_calls: false,
                    });
                    break;
                }
                let backoff = Duration::from_secs(restart_count as u64);
                eprintln!(
                    "[watch] watcher disconnected, restarting ({restart_count}/{MAX_RESTARTS}) after {}s",
                    backoff.as_secs()
                );
                std::thread::sleep(backoff);
                match create_watcher(root, ignore_dirs) {
                    Ok((new_watcher, new_rx)) => {
                        watcher = new_watcher;
                        rx = new_rx;
                        eprintln!("[watch] watcher restarted successfully");
                        on_event(WatchEvent {
                            kind: WatchEventKind::WatcherRestarted,
                            path: root.to_path_buf(),
                            has_cross_file_calls: false,
                        });
                    }
                    Err(e) => {
                        eprintln!("[watch] watcher restart failed: {e}");
                        on_event(WatchEvent {
                            kind: WatchEventKind::WatcherDied,
                            path: root.to_path_buf(),
                            has_cross_file_calls: false,
                        });
                        break;
                    }
                }
            }
        }

        // Schedule: only when nothing's in flight, so at most one drain runs
        // at a time and this loop never waits on its own background task's
        // `index.lock`.
        if drain_in_flight.is_none() && !queue.lock().unwrap().is_empty() {
            match begin_index_op(root, "infigraph daemon", Duration::from_secs(30)) {
                Ok(IndexOpOutcome::Acquired(guard)) => {
                    match watch_db(root, &make_registry, &mut held_prism) {
                        Ok(prism) => {
                            // Drained here on the loop thread rather than
                            // inside the task, so a panicking task can't take
                            // its waiters down with it -- `waiter_replies`
                            // survives to be answered by `finish_drain`.
                            let drained = queue.lock().unwrap().drain();
                            let waiter_replies: Vec<PathBuf> = drained
                                .waiters
                                .iter()
                                .map(|w| w.reply_path.clone())
                                .collect();
                            let task_prism = Arc::clone(&prism);
                            let handle = drain_rt.spawn_blocking(move || DrainTaskOutput {
                                result: crate::watch::drain::execute_drain(&task_prism, drained),
                                guard,
                            });
                            drain_in_flight = Some(InFlightDrain {
                                handle,
                                prism,
                                waiter_replies,
                            });
                        }
                        Err(e) => {
                            eprintln!("[watch] failed to reopen graph connection, will retry: {e}")
                        }
                    }
                }
                Ok(o @ IndexOpOutcome::AlreadyRunning(_)) => {
                    // Queue contents are NOT cleared here -- queue.drain()
                    // only runs inside the Acquired arm above, so whatever
                    // was pending stays queued for the next tick's attempt.
                    eprintln!(
                        "[watch] index operation busy ({}), retrying next tick",
                        o.skip_note().unwrap_or_default()
                    );
                }
                Err(e) => {
                    eprintln!("[watch] index operation busy ({e}), retrying next tick");
                }
            }
        }
    }

    // A drain still running when the loop exits holds `index.lock` and a
    // connection to the graph. Wait it out rather than returning into a
    // process teardown that would drop both mid-write.
    if let Some(in_flight) = drain_in_flight.take() {
        let (guard, _) = finish_drain(
            drain_rt.block_on(in_flight.handle),
            &in_flight.waiter_replies,
        );
        drop(guard);
    }

    Ok(())
}

/// A drain executing on the background task, plus everything the loop
/// thread needs to finish it: the shared graph connection its downstream
/// steps run against, and the reply paths of the waiters folded into it --
/// retained here, outside the task, precisely so a panic in the task still
/// leaves someone able to answer them.
struct InFlightDrain {
    handle: tokio::task::JoinHandle<DrainTaskOutput>,
    prism: Arc<Infigraph>,
    waiter_replies: Vec<PathBuf>,
}

/// What the background drain task hands back. The `index.lock` guard rides
/// along so the loop thread keeps holding it across the post-drain steps
/// (embedding update, cross-file-call event emission) instead of those
/// running unlocked.
struct DrainTaskOutput {
    guard: crate::ops::IndexOpGuard,
    result: Result<crate::watch::drain::DrainOutcome>,
}

/// Collects a finished drain task. Returns the index-op guard to keep held
/// (absent if the task panicked, since it was dropped during the unwind)
/// and the outcome, if the drain produced one.
///
/// Both failure modes -- a panic, and an `execute_drain` error -- answer
/// every waiter with `WriteResult::Err`. Without that a client blocks until
/// its own multi-minute timeout with nothing explaining why: `execute_drain`
/// writes replies as its last step, so a failure anywhere before that
/// leaves them unwritten.
fn finish_drain(
    joined: std::result::Result<DrainTaskOutput, tokio::task::JoinError>,
    waiter_replies: &[PathBuf],
) -> (
    Option<crate::ops::IndexOpGuard>,
    Option<crate::watch::drain::DrainOutcome>,
) {
    match joined {
        Ok(DrainTaskOutput {
            guard,
            result: Ok(outcome),
        }) => (Some(guard), Some(outcome)),
        Ok(DrainTaskOutput {
            guard,
            result: Err(e),
        }) => {
            eprintln!("[watch] drain failed: {e}");
            reply_err_to_waiters(waiter_replies, &format!("daemon drain failed: {e}"));
            (Some(guard), None)
        }
        Err(join_err) => {
            eprintln!("[watch] drain task panicked: {join_err}");
            reply_err_to_waiters(
                waiter_replies,
                &format!("daemon drain task panicked: {join_err}"),
            );
            (None, None)
        }
    }
}

/// `execute_drain` writes each waiter's reply as the last step of its own
/// internal loop, using `?` to propagate a `write_atomic` failure -- so an
/// ordinary `execute_drain` error can still leave some waiters *earlier* in
/// that loop already answered correctly on disk. Skipping any `reply_path`
/// that already exists avoids clobbering those real answers with a false
/// `Err`; `write_atomic` writes to a temp file and renames into place, so an
/// existing reply file is always a complete, correctly-written one, never a
/// partial write a caller could race against.
fn reply_err_to_waiters(waiter_replies: &[PathBuf], message: &str) {
    let result = crate::daemon_protocol::WriteResult::Err {
        message: message.to_string(),
    };
    let json = match serde_json::to_string(&result) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[watch] could not encode drain failure reply: {e}");
            return;
        }
    };
    for reply_path in waiter_replies {
        if reply_path.exists() {
            continue;
        }
        if let Err(e) = crate::daemon_protocol::write_atomic(reply_path, &json) {
            eprintln!(
                "[watch] could not write drain failure reply to {}: {e}",
                reply_path.display()
            );
        }
    }
}

/// Like `watch_project` but automatically re-resolves cross-file call edges
/// when affected by a change, keeping call resolution accurate without user intervention.
///
/// Instead of running a full `prism.index()` (re-parsing every file), this collects
/// the changed file plus its cross-file dependents and uses `prism.index_files()` to
/// re-index only the affected subset, then runs targeted re-resolution via
/// `resolve::re_resolve_for_files()`.
pub fn watch_project_auto_resolve<MR>(
    root: &Path,
    make_registry: MR,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    log_prefix: &str,
) -> Result<()>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry> + Send + Sync + 'static,
{
    let root_owned = root.to_path_buf();
    let prefix = log_prefix.to_string();
    let factory: Arc<dyn Fn() -> Result<crate::lang::LanguageRegistry> + Send + Sync> =
        Arc::new(make_registry);
    let factory_for_event = Arc::clone(&factory);
    watch_project(root, move || factory(), debounce_ms, stop_rx, {
        move |evt: WatchEvent| {
            match evt.kind {
                WatchEventKind::WatcherRestarted => {
                    eprintln!("[watch {prefix}] watcher restarted after internal failure");
                    return;
                }
                WatchEventKind::WatcherDied => {
                    eprintln!("[watch {prefix}] watcher died permanently");
                    return;
                }
                _ => {}
            }
            if evt.has_cross_file_calls {
                eprintln!("[watch {prefix}] {evt}");
                if let Ok(reg) = factory_for_event() {
                    if let Ok(mut p) = Infigraph::open(&root_owned, reg) {
                        if p.init().is_ok() {
                            let changed_rel = evt
                                .path
                                .strip_prefix(&root_owned)
                                .map(|r| r.to_string_lossy().replace('\\', "/"))
                                .unwrap_or_else(|_| evt.path.to_string_lossy().replace('\\', "/"));
                            let mut affected_files = vec![evt.path.clone()];

                            if let Some(backend) = p.backend() {
                                let deps = get_cross_file_dependents(backend, &changed_rel);
                                for dep_rel in deps {
                                    let dep_abs = root_owned.join(&dep_rel);
                                    if dep_abs.exists() {
                                        affected_files.push(dep_abs);
                                    }
                                }
                            }

                            match p.index_files(&affected_files) {
                                Ok(r) => {
                                    eprintln!(
                                        "[watch {prefix}] targeted reindex: {}/{} affected files",
                                        r.indexed_files, r.total_files
                                    );

                                    if let Some(backend) = p.backend() {
                                        let file_strs: Vec<String> =
                                            r.extractions.iter().map(|e| e.file.clone()).collect();
                                        match backend.re_resolve_for_files(
                                            &file_strs,
                                            &r.extractions,
                                            None,
                                        ) {
                                            Ok(stats) => {
                                                eprintln!("[watch {prefix}] re-resolved: {stats}")
                                            }
                                            Err(e) => {
                                                eprintln!("[watch {prefix}] re-resolve failed: {e}")
                                            }
                                        }

                                        let changed: Vec<&str> =
                                            r.extractions.iter().map(|e| e.file.as_str()).collect();
                                        if let Some(eb) = p.backend() {
                                            match crate::embed::update_embeddings(
                                                eb,
                                                &root_owned,
                                                &changed,
                                            ) {
                                                Ok(n) => {
                                                    eprintln!(
                                                        "[watch {prefix}] updated {n} embeddings"
                                                    )
                                                }
                                                Err(e) => eprintln!(
                                                    "[watch {prefix}] embedding update failed: {e}"
                                                ),
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[watch {prefix}] targeted reindex failed: {e}")
                                }
                            }
                            // p drops here, releasing the DB lock
                        }
                    }
                }
            } else {
                eprintln!("[watch {prefix}] {evt}");
            }
        }
    })
}

/// Open a short-lived Infigraph instance for batch work.
fn open_transient<MR>(root: &Path, make_registry: &MR) -> Result<Infigraph>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    let registry = make_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    Ok(prism)
}

/// Acquires the watch session's shared DB connection, opening it if not
/// already held. On non-Windows platforms this connection is reused across
/// the whole watch session rather than reopened per batch/event — see
/// `watch_project_with_periodic`'s doc comment for why that matters. If an
/// operation on the returned connection fails, call `poison_watch_db` so the
/// next call reopens fresh (e.g. after the on-disk database was replaced out
/// from under a live connection, such as a concurrent `infigraph index
/// --full` against a project this watcher is also watching).
///
/// Returns an `Arc` clone rather than a borrow so the background drain task
/// can hold the *same* `Infigraph` -- and therefore the same in-process
/// `kuzu::Database` -- that the loop thread keeps using. Each graph
/// operation opens its own short-lived `Connection` from that shared
/// `Database` (see `GraphStore::connection`), which is the concurrency
/// pattern lbug documents as safe. Opening a second `Database` on the same
/// file for the drain would be a materially weaker guarantee.
#[cfg(not(windows))]
fn watch_db<MR>(
    root: &Path,
    make_registry: &MR,
    held: &mut Option<Arc<Infigraph>>,
) -> Result<Arc<Infigraph>>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    if held.is_none() {
        *held = Some(Arc::new(open_transient(root, make_registry)?));
    }
    Ok(Arc::clone(held.as_ref().unwrap()))
}

/// Windows' mandatory file locking prevents a second concurrent connection
/// while another handle on the same file is open elsewhere, so each call
/// opens (and the previous one closes) fresh rather than holding one open
/// across the whole session — see `open_transient`.
#[cfg(windows)]
fn watch_db<MR>(
    root: &Path,
    make_registry: &MR,
    held: &mut Option<Arc<Infigraph>>,
) -> Result<Arc<Infigraph>>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    *held = Some(Arc::new(open_transient(root, make_registry)?));
    Ok(Arc::clone(held.as_ref().unwrap()))
}

/// Drops the watch session's shared DB connection so the next `watch_db`
/// call reopens fresh. See `watch_db`'s doc comment for when to call this.
fn poison_watch_db(held: &mut Option<Arc<Infigraph>>) {
    *held = None;
}

/// Serves a single `.request` file via `serve_one_request`, wrapped in the
/// same `index.lock` acquisition (`begin_index_op`) the pre-`IndexWorkQueue`
/// per-request loop used. `route_or_serve_request`'s in-scope `WriteRequest`
/// variants are coordinated through the shared queue and drain step instead,
/// but its fallback paths (out-of-scope variants, malformed JSON, corrupt
/// sibling extractions files) still execute immediately here -- doing so
/// unlocked would let them race the periodic reindex, the queue's own drain,
/// or a concurrent CLI `infigraph index --full`, violating the single-writer
/// invariant. On contention the `.request` file is left in place (not
/// deleted) so it's retried on a later tick, matching the old behavior.
/// Does nothing while this daemon's own drain is in flight. That drain holds
/// `index.lock` until the loop thread reaps it -- so blocking here to wait
/// for that lock would park the only thread that can release it, a
/// self-deadlock broken only by the 30s acquire timeout. Returning instead
/// leaves the `.request` file in place, which is already this function's
/// contention behaviour: a later tick, after the drain is reaped, serves it.
fn serve_request_locked<MR>(
    root: &Path,
    path: &Path,
    make_registry: &MR,
    held: &mut Option<Arc<Infigraph>>,
    drain_in_flight: bool,
) where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    if drain_in_flight {
        return;
    }
    match begin_index_op(root, "infigraph daemon", Duration::from_secs(30)) {
        Ok(IndexOpOutcome::Acquired(_guard)) => match watch_db(root, make_registry, held) {
            Ok(prism) => {
                if let Err(e) = crate::daemon_protocol::serve_one_request(&prism, path) {
                    eprintln!("[daemon] failed to serve request {}: {e}", path.display());
                }
            }
            Err(e) => {
                eprintln!("[daemon] failed to reopen graph connection, will retry: {e}");
            }
        },
        Ok(o @ IndexOpOutcome::AlreadyRunning(_)) => {
            eprintln!(
                "[daemon] request-serving busy ({}), retrying next tick",
                o.skip_note().unwrap_or_default()
            );
        }
        Err(e) => {
            eprintln!("[daemon] request-serving busy ({e}), retrying next tick");
        }
    }
}

/// Parses a `.request` file and either enqueues it (for the four
/// index-shaped `WriteRequest` variants this design coordinates) or falls
/// through to `serve_request_locked` (unchanged `serve_one_request`
/// dispatch, still under `index.lock`) for everything else. Enqueued
/// requests' `.request` file is deleted immediately (the daemon has already
/// accepted responsibility for serving it the moment it's queued) -- the
/// reply arrives later, written by `execute_drain`.
fn route_or_serve_request<MR>(
    root: &Path,
    path: &Path,
    queue: &Arc<Mutex<crate::watch::queue::IndexWorkQueue>>,
    make_registry: &MR,
    held: &mut Option<Arc<Infigraph>>,
    drain_in_flight: bool,
) where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    let reply_path = path.with_extension("result");
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return, // transient; will be retried next tick if the file reappears
    };
    let request: crate::daemon_protocol::WriteRequest = match serde_json::from_str(&contents) {
        Ok(r) => r,
        Err(_) => {
            // Malformed request JSON -- not this design's concern to
            // recover; hand off to serve_one_request, whose existing
            // corrupt-JSON handling (WriteResult::Err) already covers it.
            serve_request_locked(root, path, make_registry, held, drain_in_flight);
            return;
        }
    };

    use crate::daemon_protocol::WriteRequest;
    use crate::watch::queue::{Waiter, WaiterKind};

    // Each arm holds the queue lock across its work-items-plus-waiter pair
    // so a drain scheduled concurrently can never take the items without
    // the waiter that's blocked on them (which would leave that client
    // waiting for a reply no later drain owes it).
    match request {
        WriteRequest::Index { paths: None } => {
            let mut q = queue.lock().unwrap();
            q.mark_whole_project();
            q.add_waiter(Waiter {
                kind: WaiterKind::Index,
                use_learned: false,
                reply_path,
            });
            drop(q);
            std::fs::remove_file(path).ok();
        }
        WriteRequest::Index { paths: Some(paths) } => {
            let mut q = queue.lock().unwrap();
            for p in paths {
                let rel = p.to_string_lossy().replace('\\', "/");
                q.add_raw(rel);
            }
            q.add_waiter(Waiter {
                kind: WaiterKind::Index,
                use_learned: false,
                reply_path,
            });
            drop(q);
            std::fs::remove_file(path).ok();
        }
        WriteRequest::UpsertFilesBulk {
            extractions_path, ..
        } => match crate::daemon_protocol::read_extractions_json(&extractions_path) {
            Ok(extractions) => {
                let mut q = queue.lock().unwrap();
                for extraction in extractions {
                    q.add_structured(extraction);
                }
                q.add_waiter(Waiter {
                    kind: WaiterKind::UpsertFilesBulk,
                    use_learned: false,
                    reply_path,
                });
                drop(q);
                std::fs::remove_file(&extractions_path).ok();
                std::fs::remove_file(path).ok();
            }
            Err(_) => {
                // Sibling extractions file missing/corrupt -- fall
                // through to serve_one_request's existing error path.
                serve_request_locked(root, path, make_registry, held, drain_in_flight);
            }
        },
        WriteRequest::RemoveFiles { files } => {
            let mut q = queue.lock().unwrap();
            for f in files {
                q.add_removal(f);
            }
            q.add_waiter(Waiter {
                kind: WaiterKind::RemoveFiles,
                use_learned: false,
                reply_path,
            });
            drop(q);
            std::fs::remove_file(path).ok();
        }
        WriteRequest::ResolveCalls {
            extractions_path,
            use_learned,
        } => match crate::daemon_protocol::read_extractions_json(&extractions_path) {
            Ok(extractions) => {
                let mut q = queue.lock().unwrap();
                for extraction in extractions {
                    q.add_resolve_only(extraction);
                }
                q.add_waiter(Waiter {
                    kind: WaiterKind::ResolveCalls,
                    use_learned,
                    reply_path,
                });
                drop(q);
                std::fs::remove_file(&extractions_path).ok();
                std::fs::remove_file(path).ok();
            }
            Err(_) => {
                serve_request_locked(root, path, make_registry, held, drain_in_flight);
            }
        },
        _ => {
            // The other 9 variants: unchanged behavior, immediate execution
            // (still under index.lock, matching pre-IndexWorkQueue behavior).
            serve_request_locked(root, path, make_registry, held, drain_in_flight);
        }
    }
}

/// Returns the relative paths of files that have cross-file CALLS edges to/from the given file.
fn get_cross_file_dependents(
    backend: &dyn crate::graph::GraphBackend,
    rel_path: &str,
) -> Vec<String> {
    let escaped = rel_path.replace('\'', "\\'");
    let mut dependents = std::collections::HashSet::new();

    let q1 = format!(
        "MATCH (a:Symbol)-[:CALLS]->(b:Symbol) WHERE a.file = '{escaped}' AND b.file <> '{escaped}' RETURN DISTINCT b.file"
    );
    if let Ok(result) = backend.raw_query(&q1) {
        for row in result {
            if let Some(val) = row.first() {
                dependents.insert(val.to_string());
            }
        }
    }

    let q2 = format!(
        "MATCH (a:Symbol)-[:CALLS]->(b:Symbol) WHERE b.file = '{escaped}' AND a.file <> '{escaped}' RETURN DISTINCT a.file"
    );
    if let Ok(result) = backend.raw_query(&q2) {
        for row in result {
            if let Some(val) = row.first() {
                dependents.insert(val.to_string());
            }
        }
    }

    dependents.into_iter().collect()
}

/// Returns true if the file has any resolved CALLS edges to/from symbols in other files.
fn has_cross_file_calls(prism: &Infigraph, rel_path: &str) -> bool {
    let backend = match prism.backend() {
        Some(b) => b,
        None => return false,
    };
    let escaped = rel_path.replace('\'', "\\'");
    let q = format!(
        "MATCH (a:Symbol)-[:CALLS]->(b:Symbol) WHERE a.file = '{escaped}' AND b.file <> '{escaped}' RETURN count(*) LIMIT 1"
    );
    if let Ok(result) = backend.raw_query(&q) {
        if let Some(row) = result.first() {
            if let Some(val) = row.first() {
                if val.to_string().parse::<u64>().unwrap_or(0) > 0 {
                    return true;
                }
            }
        }
    }
    let q2 = format!(
        "MATCH (a:Symbol)-[:CALLS]->(b:Symbol) WHERE b.file = '{escaped}' AND a.file <> '{escaped}' RETURN count(*) LIMIT 1"
    );
    if let Ok(result) = backend.raw_query(&q2) {
        if let Some(row) = result.first() {
            if let Some(val) = row.first() {
                return val.to_string().parse::<u64>().unwrap_or(0) > 0;
            }
        }
    }
    false
}

fn should_ignore(path: &Path, ignore_dirs: &[&str]) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        ignore_dirs.contains(&s.as_ref()) || s.starts_with('.')
    })
}

fn register_watch_dirs(
    watcher: &mut RecommendedWatcher,
    root: &Path,
    ignore_dirs: &[&str],
) -> Result<()> {
    watcher.watch(root, RecursiveMode::NonRecursive)?;
    register_subdirs(watcher, root, ignore_dirs);
    Ok(())
}

fn register_subdirs(watcher: &mut RecommendedWatcher, dir: &Path, ignore_dirs: &[&str]) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if ignore_dirs.contains(&name_str.as_ref()) || name_str.starts_with('.') {
            continue;
        }
        let _ = watcher.watch(&path, RecursiveMode::NonRecursive);
        register_subdirs(watcher, &path, ignore_dirs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drain runs on a background task, so a panic inside it unwinds on a
    /// thread the watch loop never sees. Nothing else would ever answer the
    /// ad-hoc requests folded into that drain: `execute_drain` writes every
    /// reply as its final step, so a panic before that point leaves the
    /// clients blocking on a `.result` file that no longer has an author.
    /// They'd sit there until their own multi-minute timeout with no
    /// explanation. `finish_drain` is what turns that silence into a real
    /// `WriteResult::Err`.
    #[test]
    fn drain_task_panic_surfaces_as_write_result_err_not_a_hang() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("waiter-a.result");
        let second = tmp.path().join("waiter-b.result");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();

        // The panic is the point of the test, so its default backtrace
        // message would just look like a failure in the log. Suppressed only
        // across the join -- by the time `block_on` returns, the panic has
        // already been caught and reported through the `JoinError`.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let joined = rt
            .block_on(rt.spawn_blocking(|| -> DrainTaskOutput { panic!("simulated drain panic") }));
        std::panic::set_hook(previous_hook);

        assert!(
            joined.is_err(),
            "test setup is wrong: the drain task did not actually panic"
        );

        let (guard, outcome) = finish_drain(joined, &[first.clone(), second.clone()]);
        assert!(
            guard.is_none(),
            "a panicking task drops its index-op guard during the unwind, \
             so there is none left to hand back"
        );
        assert!(outcome.is_none(), "a panicked drain produced no outcome");

        for reply_path in [&first, &second] {
            let contents = std::fs::read_to_string(reply_path).unwrap_or_else(|e| {
                panic!(
                    "a panicked drain must still answer every waiter folded into it, \
                     but {} was never written ({e}) -- that client would block until \
                     its own timeout",
                    reply_path.display()
                )
            });
            let reply: crate::daemon_protocol::WriteResult =
                serde_json::from_str(&contents).unwrap();
            match reply {
                crate::daemon_protocol::WriteResult::Err { message } => assert!(
                    message.contains("panic"),
                    "the reply must say the drain panicked, got: {message}"
                ),
                other => panic!("expected WriteResult::Err, got {other:?}"),
            }
        }
    }

    /// Regression test for a review finding on `finish_drain`: an *ordinary*
    /// `execute_drain` failure (not a panic) used to answer every retained
    /// waiter with `WriteResult::Err`, even though `execute_drain` writes
    /// each waiter's reply sequentially as its own last step -- so a
    /// `write_atomic` failure partway through that loop left *earlier*
    /// waiters already holding a correct, real `Ok` reply on disk.
    /// `finish_drain`'s blanket overwrite told those already-succeeded
    /// clients their write had failed, when it had actually succeeded.
    #[test]
    fn finish_drain_does_not_overwrite_a_reply_execute_drain_already_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let mut prism = Infigraph::open(root, crate::lang::LanguageRegistry::new()).unwrap();
        prism.init().unwrap();

        let ok_reply = root.join("waiter-ok.result");
        let fail_dir = root.join("readonly");
        std::fs::create_dir(&fail_dir).unwrap();
        let fail_reply = fail_dir.join("waiter-fail.result");

        let mut queue = crate::watch::queue::IndexWorkQueue::new();
        queue.add_waiter(crate::watch::queue::Waiter {
            kind: crate::watch::queue::WaiterKind::Index,
            use_learned: false,
            reply_path: ok_reply.clone(),
        });
        queue.add_waiter(crate::watch::queue::Waiter {
            kind: crate::watch::queue::WaiterKind::Index,
            use_learned: false,
            reply_path: fail_reply.clone(),
        });
        let drained = queue.drain();

        // `write_atomic` calls `File::create` in the reply's parent
        // directory -- read-only permissions make that fail for the second
        // waiter only, after the first waiter's reply has already been
        // written successfully.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fail_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        let drain_result = crate::watch::drain::execute_drain(&prism, drained);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fail_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let err = match drain_result {
            Err(e) => e,
            Ok(_) => panic!("test setup is wrong: expected execute_drain to fail on fail_reply"),
        };

        // Confirm the partial-success setup: the first waiter really did
        // get a correct Ok reply from execute_drain itself, and the second
        // was never reached.
        let ok_before: crate::daemon_protocol::WriteResult =
            serde_json::from_str(&std::fs::read_to_string(&ok_reply).unwrap()).unwrap();
        assert!(
            matches!(ok_before, crate::daemon_protocol::WriteResult::Ok { .. }),
            "test setup is wrong: execute_drain should have answered the first waiter"
        );
        assert!(
            !fail_reply.exists(),
            "test setup is wrong: execute_drain should not have reached the second waiter"
        );

        // Route that failure through the real recovery path a background
        // drain task uses, and confirm it does not clobber the first
        // waiter's already-correct reply.
        let guard = match begin_index_op(root, "test", Duration::ZERO).unwrap() {
            IndexOpOutcome::Acquired(g) => g,
            IndexOpOutcome::AlreadyRunning(_) => panic!("test setup is wrong: lock contended"),
        };
        let joined: std::result::Result<DrainTaskOutput, tokio::task::JoinError> =
            Ok(DrainTaskOutput {
                guard,
                result: Err(err),
            });
        let (guard, outcome) = finish_drain(joined, &[ok_reply.clone(), fail_reply.clone()]);
        assert!(guard.is_some());
        assert!(outcome.is_none());

        let ok_after: crate::daemon_protocol::WriteResult =
            serde_json::from_str(&std::fs::read_to_string(&ok_reply).unwrap()).unwrap();
        match ok_after {
            crate::daemon_protocol::WriteResult::Ok { .. } => {}
            other => panic!(
                "finish_drain overwrote the first waiter's already-correct Ok reply with {other:?}"
            ),
        }

        let fail_after: crate::daemon_protocol::WriteResult =
            serde_json::from_str(&std::fs::read_to_string(&fail_reply).unwrap()).unwrap();
        match fail_after {
            crate::daemon_protocol::WriteResult::Err { .. } => {}
            other => {
                panic!("expected the never-answered waiter to get WriteResult::Err, got {other:?}")
            }
        }
    }

    /// Regression test for a macOS-specific bug: `watch_project_with_periodic`
    /// used to compare raw filesystem-watch event paths against the caller's
    /// `root` exactly as given. FSEvents delivers absolute, symlink-resolved
    /// event paths, so a non-canonical `root` (e.g. a relative path, or one
    /// that traverses a symlink) made `path.strip_prefix(root)` fail for
    /// every event, silently dropping all changes with no error. This is the
    /// same class of bug that made the `infigraph watch` CLI command — which
    /// watched the unresolved `.` — appear to receive no events at all,
    /// prompting a workaround (the kqueue backend) that caused a much larger
    /// file-descriptor leak.
    ///
    /// A custom-prefixed `tempfile::TempDir` reproduces a non-canonical root
    /// deterministically on macOS: it lives under `/var/folders/...`, itself
    /// a symlink to `/private/var/folders/...`. (The default `TempDir::new()`
    /// prefix starts with a dot, which the watcher's own hidden-file filter
    /// would ignore regardless of this bug, so a custom prefix is used to
    /// keep the test isolated to the canonicalization behavior.)
    #[test]
    #[cfg(target_os = "macos")]
    fn watch_project_detects_changes_through_symlinked_root() {
        let tmp = tempfile::Builder::new()
            .prefix("infigraph-watch-test-")
            .tempdir()
            .unwrap();
        let raw_root = tmp.path().to_path_buf();
        let canonical_root = raw_root.canonicalize().unwrap();
        assert_ne!(
            raw_root, canonical_root,
            "test assumption broken: TempDir root is already canonical on this machine"
        );

        let file_path = raw_root.join("watched.txt");
        std::fs::write(&file_path, "v1").unwrap();

        let events: Arc<Mutex<Vec<WatchEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);
        let (stop_tx, stop_rx) = mpsc::channel();

        let handle = std::thread::spawn(move || {
            watch_project(
                &raw_root,
                || Ok(crate::lang::LanguageRegistry::new()),
                50,
                stop_rx,
                move |evt| events_clone.lock().unwrap().push(evt),
            )
        });

        // Give the watcher time to register before triggering a change.
        std::thread::sleep(Duration::from_millis(300));
        std::fs::remove_file(&file_path).unwrap();

        // Poll rather than a single fixed sleep: fast on a quiet machine,
        // robust on a loaded one.
        let mut seen = false;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            if !events.lock().unwrap().is_empty() {
                seen = true;
                break;
            }
        }

        let _ = stop_tx.send(());
        let _ = handle.join();

        assert!(
            seen,
            "watch_project delivered no events for a change under a non-canonical \
             (symlinked) root — the root.canonicalize() call in \
             watch_project_with_periodic may have regressed"
        );
    }
}
