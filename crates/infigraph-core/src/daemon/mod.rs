pub(crate) mod drain;
pub mod lifecycle;
pub mod queue;
pub mod task;

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::daemon::queue::IndexWorkQueue;
use crate::daemon::task::Task;
use crate::daemon_protocol::{WatchAction, WatchRole};
use crate::ops::{begin_index_op, IndexOpOutcome};
use crate::watch::{config, producer, CodeWatch, WatchEvent, WatchEventKind};
use crate::Infigraph;

/// How long one coordinator tick waits before looking again. The fsevent
/// half moved to `producer::run_producer`, so this loop no longer blocks on
/// a watch receiver -- but every remaining job it has (reaping finished
/// background work, serving `.request` files, scheduling drains) is still
/// polled, on the same ~200ms cadence `rx.recv_timeout` used to impose.
const COORDINATOR_TICK: Duration = Duration::from_millis(200);

/// Callback for in-process SCIP enrichment after a successful daemon full
/// reindex. Takes the daemon's own (already-open, already-reopened-post-swap)
/// connection -- the callback must NOT open a second `Infigraph`/`Database`
/// on the same live graph path; Kuzu only allows safe concurrent access
/// within one process's `Database` object, not across two, even in the same
/// process.
///
/// The loop that invokes this callback does NOT hold `index.lock` around
/// the call -- the callback is responsible for acquiring it itself, scoped
/// narrowly around whichever part of its own work actually touches the
/// graph. Holding it for the callback's entire duration (as an earlier
/// version of this design did) blocks every other daemon write for
/// however long any graph-independent work inside the callback takes --
/// e.g. running external SCIP indexer binaries, which can take minutes on
/// a real repo.
/// The `CancellationToken` is this callback's own child token (from the
/// `Task::spawn_blocking` it runs inside) -- a cooperative-cancellation
/// checkpoint for whatever synchronous, potentially long-running work the
/// callback does (e.g. `run_scip_indexers`' between-indexer-launch check).
pub type FullReindexCallback = dyn Fn(Arc<Infigraph>, Vec<String>, CancellationToken) + Send + Sync;

/// Caller-supplied hook that acts on a `WatchControl { role: Docs, .. }`
/// request. Doc-watching lives in `infigraph-docs`, a crate this one does
/// not depend on, and its loop is still driven by its own
/// `Arc<AtomicBool>`/thread shape rather than a `Task<()>` -- so the
/// coordinator dispatches the request and the owner of that thread (today:
/// `cmd_daemon`) decides what start/stop actually mean for it. `Err(msg)`
/// becomes the request's `WriteResult::Err`.
pub type DocsControl = dyn Fn(WatchAction) -> std::result::Result<(), String> + Send + Sync;

/// The daemon's write coordinator: reaps and schedules index-shaped work
/// (drains, full reindexes, SCIP enrichment), serves `.request` files, and
/// owns the code-watch producer `Task<()>`'s lifecycle.
///
/// Filesystem watching itself is NOT done here -- it runs in
/// `producer::run_producer` on `CodeWatch`'s own runtime, feeding the same
/// `queue` this loop drains. That separation is the point: a
/// `WatchControl { role: Code, action: Stop }` request stops the producer
/// while this loop keeps running and keeps serving writes.
///
/// `docs_control` lets a caller that owns a doc-watch loop (the CLI daemon)
/// have `WatchControl { role: Docs, .. }` requests dispatched to it; `None`
/// answers those requests with an error instead.
#[allow(clippy::too_many_arguments)]
pub fn run_write_coordinator<MR, F>(
    root: &Path,
    make_registry: MR,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    on_event: impl Fn(WatchEvent) + Send + Sync + 'static,
    periodic_secs: u64,
    on_periodic: Option<F>,
    serve_requests: bool,
    on_full_reindex: Option<Arc<FullReindexCallback>>,
    daemon_token: &CancellationToken,
    docs_control: Option<Arc<DocsControl>>,
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

    // Build the registry ONCE for the whole watch session (#58): it serves
    // both file-extension filtering here and every `watch_db` open below
    // via `Infigraph::open_shared`. It used to be built twice serially
    // (filter + first drain's open), which alone consumed ~5s in debug
    // builds and pushed the daemon's first request reply past callers'
    // timeouts. The full-reindex side-path build keeps its own fresh
    // `make_registry()` call -- a rebuild takes far longer than a registry
    // build, so sharing buys nothing there.
    let shared_registry: Arc<crate::lang::LanguageRegistry> = Arc::new(make_registry()?);

    let mut changes_since_periodic: usize = 0;
    let mut last_periodic = std::time::Instant::now();

    // Shared DB connection for the watch session — see `watch_db`'s doc
    // comment for the platform split (held open on non-Windows, reopened
    // per call on Windows).
    let mut held_prism: Option<Arc<Infigraph>> = None;

    // Accumulates index-shaped work from every producer (the code-watch
    // task, the periodic mark below, ad-hoc daemon-protocol requests) so
    // it's drained as one combined execution per tick instead of each
    // producer racing its own stale plan against the others -- see
    // docs/superpowers/specs/2026-08-03-daemon-index-work-queue-design.md.
    //
    // `Arc<Mutex<_>>` because it is the ONLY state the code-watch task and
    // this loop share -- no Kuzu connection, no `held_prism`, no drain
    // bookkeeping crosses that boundary -- and because the drain itself runs
    // on a background task while producers keep filling it under the mutex.
    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let infigraph_dir = root.join(".infigraph");

    // R3.3.5's dirty-set recovery now runs inside `run_producer` (once per
    // producer start), not here -- it is watch-side recovery, and keeping a
    // second copy on this side would double-enqueue every recovered path
    // the moment both halves run against the same queue.

    // Code-watching runs as its own cancellable task feeding `queue`, so a
    // `WatchControl { role: Code, .. }` request can stop and restart it
    // without this loop noticing. Its registry is the same one built above:
    // building a second would cost seconds in debug builds (#58).
    let on_event_shared: Arc<dyn Fn(WatchEvent) + Send + Sync> = Arc::new(on_event);
    let mut code_watch = CodeWatch::new(
        daemon_token,
        producer::ProducerConfig {
            root: root.to_path_buf(),
            registry: Arc::clone(&shared_registry),
            debounce_ms,
            // Callers with a periodic pass keep that pass's cadence for the
            // ignore-matcher rebuild, exactly as this loop used to do
            // inline. Callers without one (the daemon: `periodic_secs == 0`)
            // used to never rebuild it at all; a 5-minute floor gives them
            // mid-session `.gitignore` edits without a restart.
            ignore_rebuild_secs: if periodic_secs > 0 {
                periodic_secs
            } else {
                300
            },
        },
        Arc::clone(&queue),
        Arc::clone(&on_event_shared),
    )?;
    // Honor the persisted enable/disable policy on every start of this
    // loop (fresh daemon, crash-restart, `daemon-restart`) -- not just when
    // spawning a new daemon. Without this gate, `watch disable` stops a
    // *live* daemon's code-watching but a restart silently resumes it,
    // since the policy previously only gated whether a daemon got spawned
    // at all, never what an already-running one does once it's up.
    if config::watch_enabled_at(root, "watch") {
        code_watch.start();
    }

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
    let mut full_reindex_in_flight: Option<PendingFullReindex> = None;
    let mut scip_in_flight: Option<Task<()>> = None;

    let sentinel = root.join(".infigraph").join("watch.stop");

    // Set by a `WatchControl { role: Daemon, action: Stop|Restart }` request.
    // Deliberately NOT derived from `daemon_token.is_cancelled()`:
    // `daemon_token` is the root of the *background-work* cancellation
    // hierarchy, and a caller is entitled to hand this loop an
    // already-cancelled one to mean "run, but let nothing you spawn survive"
    // -- reading it as a loop-exit signal would make that caller's very first
    // tick a shutdown.
    let mut shutdown_requested = false;

    loop {
        if stop_rx.try_recv().is_ok() {
            eprintln!("[watch] stop channel signaled -- shutting down");
            break;
        }

        if sentinel.exists() {
            let _ = std::fs::remove_file(&sentinel);
            eprintln!("[watch] watch.stop sentinel found -- shutting down");
            break;
        }

        // Self-terminate once the watched root is gone (`rm -rf`'d project,
        // a test's tempdir, a removed worktree that skipped `worktree
        // teardown`). No dedicated poll timer needed for this -- the loop
        // already ticks every `COORDINATOR_TICK`, so this check rides that
        // existing cadence for free. Without it, a daemon whose target
        // directory disappeared keeps running forever: `prune_stale_holder`
        // only reaps a *dead* holder, and this process is very much alive,
        // just watching nothing. `infigraph gc --global` sweeps the
        // registry for the same condition as a backstop for a daemon that's
        // wedged and never reaches this check.
        if !root.exists() {
            eprintln!(
                "[watch] {} no longer exists -- shutting down",
                root.display()
            );
            break;
        }

        // Shared drain step, in two halves: reap whatever finished since the
        // last tick (here), then schedule the next one (at the end of the
        // tick). The drain itself combines everything every producer
        // (periodic mark, ad-hoc requests, and the code-watch task's batch
        // flushes and removals) contributed into ONE execution -- the fix for the
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
                removed_in_drain,
            } = drain_in_flight.take().expect("checked is_some just above");
            let (guard, outcome) = finish_drain(drain_rt.block_on(handle), &waiter_replies);
            match outcome {
                Some(outcome) => {
                    // Removals are counted here rather than off a raw
                    // fsevent (as they were before the producer split, when
                    // this loop owned the watcher): `add_watch_removal`
                    // writes into the same `queue` this drain came from, so
                    // what the drain actually removed is a strictly better
                    // signal than what an fsevent claimed -- and it is the
                    // only one still visible from this side of the split.
                    // Without it a removals-only session could never trip
                    // the periodic whole-project pass below.
                    changes_since_periodic += outcome.extractions.len() + removed_in_drain.len();

                    // R3.3.5: only extractions that actually made it into
                    // `outcome` were confirmed written (a per-file read/parse
                    // failure inside `extract_paths` silently drops that file
                    // rather than failing the whole drain -- see its own doc
                    // comment -- so it correctly stays dirty here for a later
                    // retry instead of being cleared alongside its batch).
                    let mut cleared: Vec<String> =
                        outcome.extractions.iter().map(|e| e.file.clone()).collect();
                    cleared.extend(removed_in_drain);
                    if !cleared.is_empty() {
                        if let Err(e) = crate::dirty::clear_dirty(&infigraph_dir, &cleared) {
                            eprintln!("[watch] failed to clear dirty set: {e}");
                        }
                    }
                    if let Some(ref cb) = on_periodic {
                        if !outcome.extractions.is_empty() {
                            cb(&crate::IndexResult {
                                total_files: outcome.extractions.len(),
                                indexed_files: outcome.extractions.len(),
                                extractions: outcome.extractions.clone(),
                                resolve_stats: outcome.resolve_stats.clone(),
                                skipped_errors: Vec::new(),
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
                        on_event_shared(WatchEvent {
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

        // Reap a finished full-reindex build the same way the regular drain
        // above is reaped: pull the handle, run the fast held-touching finish
        // step on this thread, then (only on full success) schedule SCIP
        // enrichment as its own background task -- never inside the same task,
        // so a `--full` reindex still returns to the client as soon as the
        // rebuild lands, not after SCIP also finishes.
        if full_reindex_in_flight
            .as_ref()
            .is_some_and(|f| f.task.is_finished())
        {
            let PendingFullReindex {
                task,
                request_path,
                reply_path,
            } = full_reindex_in_flight
                .take()
                .expect("checked is_some just above");
            let (guard, scheduled_languages) = finish_full_reindex(
                root,
                &reply_path,
                &shared_registry,
                &mut held_prism,
                drain_rt.block_on(task.join()),
            );
            std::fs::remove_file(&request_path).ok();
            drop(guard);

            if let Some(languages) = scheduled_languages {
                if scip_in_flight.is_some() {
                    // A previous full reindex's SCIP task is still running --
                    // don't overwrite its tracked handle (we'd lose the ability to
                    // reap/log it) or run two enrichment passes concurrently
                    // against the same connection. This round's enrichment is
                    // simply skipped; the graph itself is still correct (just not
                    // freshly re-enriched) and the next full reindex tries again.
                    eprintln!(
                        "[daemon] a previous SCIP-enrichment task is still running; skipping \
                         enrichment for this reindex (detected languages: {})",
                        languages.join(", ")
                    );
                } else if let (Some(cb), Some(prism)) =
                    (on_full_reindex.clone(), held_prism.clone())
                {
                    // `Task::spawn_blocking` dispatches via the ambient
                    // `tokio::task::spawn_blocking`, which needs a runtime
                    // context on this (plain OS) thread -- `drain_rt.enter()`
                    // scopes that context to just this call, matching
                    // `try_start_full_reindex`'s identical need.
                    let task = {
                        let _guard = drain_rt.enter();
                        Task::spawn_blocking(daemon_token, "scip-enrich", move |token| {
                            cb(prism, languages, token);
                        })
                    };
                    scip_in_flight = Some(task);
                }
            }
        }

        // Reap a finished SCIP-enrichment task. Nobody is waiting on a reply for
        // this one -- it isn't client-requested -- so this only needs to log a
        // panic and free the slot.
        if scip_in_flight.as_ref().is_some_and(|s| s.is_finished()) {
            let task = scip_in_flight.take().expect("checked is_some just above");
            if let Err(join_err) = drain_rt.block_on(task.join()) {
                eprintln!("[watch] scip-enrich task panicked: {join_err}");
            }
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
        // serve_requests=false). Piggybacks on this loop's `COORDINATOR_TICK`
        // cadence rather than a separate notify-based watch on the requests
        // directory -- submit_write_request's own poll-with-backoff starts at
        // 10ms and only reaches 200ms after several rounds, so this cadence
        // is fine. The spec's event-driven upgrade (a second `notify::Watcher`
        // scoped to `.infigraph/requests/`) is deferred: it needs a `select!`
        // arm, and this coordinator is deliberately synchronous.
        if serve_requests {
            // R3.1.4a/c: translate a pending dead-holder-WAL sentinel into a
            // synthetic FullReindex request (or a crash-loop refusal) before
            // scanning for requests below, so this same tick's scan picks
            // the synthetic request up immediately rather than waiting a
            // full COORDINATOR_TICK.
            if let Err(e) = crate::recovery::drain_recovery_sentinel(&infigraph_dir) {
                eprintln!("[watch] recovery-sentinel handling failed: {e}");
            }

            let requests_dir = infigraph_dir.join("requests");
            if let Ok(entries) = std::fs::read_dir(&requests_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "request") {
                        if let Some(started) = route_or_serve_request(
                            root,
                            &path,
                            &queue,
                            &shared_registry,
                            &make_registry,
                            &mut held_prism,
                            drain_in_flight.is_some(),
                            full_reindex_in_flight.is_some(),
                            &drain_rt,
                            daemon_token,
                            &mut code_watch,
                            docs_control.as_ref(),
                            &mut shutdown_requested,
                        ) {
                            full_reindex_in_flight = Some(started);
                        }
                    }
                }
            }
        }

        // Checked here rather than at the top of the next tick so a
        // `WatchControl { role: Daemon, action: Stop }` reply isn't followed
        // by another `COORDINATOR_TICK` of scheduling work the caller just
        // asked this process to stop doing.
        if shutdown_requested {
            eprintln!("[watch] daemon.stop request received -- shutting down");
            break;
        }

        // Schedule: only when nothing's in flight, so at most one drain runs
        // at a time and this loop never waits on its own background task's
        // `index.lock`.
        if drain_in_flight.is_none()
            && full_reindex_in_flight.is_none()
            && !queue.lock().unwrap().is_empty()
        {
            match begin_index_op(root, "infigraph daemon", Duration::from_secs(30)) {
                Ok(IndexOpOutcome::Acquired(guard)) => {
                    match watch_db(root, &shared_registry, &mut held_prism) {
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
                            // R3.3.5: captured before `drained` moves into the
                            // task -- `DrainOutcome` only reports successful
                            // extractions, not removals, and a removal's
                            // `backend.remove_file` error is already
                            // swallowed by `execute_drain` itself (best-effort,
                            // matching the rest of that code path), so "was
                            // part of a drain that returned Ok" is the same
                            // confidence level the removal path itself offers.
                            let removed_in_drain: Vec<String> =
                                drained.removals.iter().cloned().collect();
                            let task_prism = Arc::clone(&prism);
                            let handle = drain_rt.spawn_blocking(move || DrainTaskOutput {
                                result: crate::daemon::drain::execute_drain(&task_prism, drained),
                                guard,
                            });
                            drain_in_flight = Some(InFlightDrain {
                                handle,
                                prism,
                                waiter_replies,
                                removed_in_drain,
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

        std::thread::sleep(COORDINATOR_TICK);
    }

    // Stop the producer before waiting out the in-flight work below: it
    // shares `queue`, and anything it adds after this point would be
    // enqueued for a drain that is never going to run.
    code_watch.stop();

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

    // Same reasoning as the drain cleanup above -- a full reindex or a SCIP
    // task still running when the loop exits holds `index.lock` (and, for
    // the full reindex, a connection to the graph). Wait it out.
    if let Some(in_flight) = full_reindex_in_flight.take() {
        let (guard, _) = finish_full_reindex(
            root,
            &in_flight.reply_path,
            &shared_registry,
            &mut held_prism,
            drain_rt.block_on(in_flight.task.join()),
        );
        std::fs::remove_file(&in_flight.request_path).ok();
        drop(guard);
    }
    if let Some(in_flight) = scip_in_flight.take() {
        let _ = drain_rt.block_on(in_flight.join());
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
    /// R3.3.5: paths this drain's `DrainedQueue.removals` named, captured
    /// before the queue moved into the task -- cleared from the persistent
    /// dirty set alongside `outcome.extractions` once the drain confirms
    /// success. See the capture site's comment for why removals get this
    /// same best-effort treatment rather than a stricter per-path check.
    removed_in_drain: Vec<String>,
}

/// What the background drain task hands back. The `index.lock` guard rides
/// along so the loop thread keeps holding it across the post-drain steps
/// (embedding update, cross-file-call event emission) instead of those
/// running unlocked.
struct DrainTaskOutput {
    guard: crate::ops::IndexOpGuard,
    result: Result<crate::daemon::drain::DrainOutcome>,
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
    Option<crate::daemon::drain::DrainOutcome>,
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

// (open_transient below opens a short-lived Infigraph instance for batch work.)

fn open_transient(root: &Path, registry: &Arc<crate::lang::LanguageRegistry>) -> Result<Infigraph> {
    // Reuses the watch session's already-built registry (#58): building the
    // 62-pack registry takes seconds in debug builds, and doing it again
    // here put the daemon's first request reply right at the edge of
    // callers' timeouts. `Infigraph::open_shared` exists for exactly this.
    let mut prism = Infigraph::open_shared(root, Arc::clone(registry))?;
    prism.init()?;
    Ok(prism)
}

/// Acquires the watch session's shared DB connection, opening it if not
/// already held. On non-Windows platforms this connection is reused across
/// the whole watch session rather than reopened per batch/event — see
/// `run_write_coordinator`'s doc comment for why that matters. If an
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
fn watch_db(
    root: &Path,
    registry: &Arc<crate::lang::LanguageRegistry>,
    held: &mut Option<Arc<Infigraph>>,
) -> Result<Arc<Infigraph>> {
    if held.is_none() {
        *held = Some(Arc::new(open_transient(root, registry)?));
    }
    Ok(Arc::clone(held.as_ref().unwrap()))
}

/// Windows' mandatory file locking prevents a second concurrent connection
/// while another handle on the same file is open elsewhere, so each call
/// opens (and the previous one closes) fresh rather than holding one open
/// across the whole session — see `open_transient`.
#[cfg(windows)]
fn watch_db(
    root: &Path,
    registry: &Arc<crate::lang::LanguageRegistry>,
    held: &mut Option<Arc<Infigraph>>,
) -> Result<Arc<Infigraph>> {
    *held = Some(Arc::new(open_transient(root, registry)?));
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
fn serve_request_locked(
    root: &Path,
    path: &Path,
    registry: &Arc<crate::lang::LanguageRegistry>,
    held: &mut Option<Arc<Infigraph>>,
    drain_in_flight: bool,
) {
    if drain_in_flight {
        return;
    }
    match begin_index_op(root, "infigraph daemon", Duration::from_secs(30)) {
        Ok(IndexOpOutcome::Acquired(_guard)) => match watch_db(root, registry, held) {
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

/// What the background full-reindex build task computes: a verified-good
/// fresh graph sitting at `graph.rebuilding`, or (via the `Result` this is
/// wrapped in) the reason it isn't. The swap itself, and everything that
/// touches `held`, happens afterward on the loop thread
/// (`finish_full_reindex`) -- `spawn_blocking`'s `'static` bound can't
/// capture `&mut Option<Arc<Infigraph>>`.
struct FullReindexBuildOutcome {
    indexed_files: usize,
    detected_languages: Vec<String>,
}

/// What the background build task hands back. The `index.lock` guard rides
/// along so the loop thread keeps holding it across the swap, matching
/// `DrainTaskOutput`'s shape.
struct FullReindexTaskOutput {
    guard: crate::ops::IndexOpGuard,
    result: Result<FullReindexBuildOutcome>,
}

/// A full-reindex build executing on the background `Task<T>`, plus what the
/// loop thread needs to finish it once it completes. `Task<T>` itself
/// doesn't carry the request/reply paths -- those are this loop's own
/// bookkeeping, tracked alongside it.
struct PendingFullReindex {
    task: Task<FullReindexTaskOutput>,
    request_path: PathBuf,
    reply_path: PathBuf,
}

/// The expensive, `held`-independent part of a full reindex: build a fresh
/// database at `graph.rebuilding`, scan/extract/upsert/resolve every file,
/// derive TESTED_BY edges. Runs entirely against its own connection, opened
/// fresh inside this call -- deliberately takes no `&mut Option<Arc<Infigraph>>`
/// so it can run inside `spawn_blocking`'s `'static` closure. The live graph
/// is never touched here; only `finish_full_reindex` (loop thread) swaps it
/// in.
///
/// `token` is checked once, right after the cheap leftover-cleanup step and
/// before the expensive Kuzu open+scan+upsert+resolve sequence -- abandoning
/// the build at (or before) this point is always safe, since nothing that
/// touches the live graph has happened yet. Nothing inside the expensive
/// sequence itself is cancellation-aware; a cancellation observed after this
/// checkpoint still runs to completion (still safe, just not responsive).
fn build_full_reindex(
    root: &Path,
    registry: crate::lang::LanguageRegistry,
    token: &CancellationToken,
) -> Result<FullReindexBuildOutcome> {
    const REBUILDING_NAME: &str = "graph.rebuilding";
    let rebuilding_path = root.join(".infigraph").join(REBUILDING_NAME);

    // Clean up any stale leftover from a previously-interrupted rebuild
    // attempt (e.g. the daemon was killed mid-rebuild last time) before
    // starting a new one. Unconditional, and covering the WAL family as
    // well as the base image -- see the original function's comment (now
    // removed) for why a surviving WAL sibling permanently wedges every
    // future full reindex if left in place.
    let _ = std::fs::remove_dir_all(&rebuilding_path);
    let _ = std::fs::remove_file(&rebuilding_path);
    crate::graph::remove_wal_family(&rebuilding_path);

    if token.is_cancelled() {
        return Err(anyhow::anyhow!(
            "full reindex build cancelled before starting"
        ));
    }

    let build_result = Infigraph::open_local_kuzu_at(root, registry, rebuilding_path.clone())
        .and_then(|fresh| {
            let backend = fresh
                .backend()
                .ok_or_else(|| anyhow::anyhow!("freshly-opened backend was not initialized"))?;
            let scan = fresh.scan_changed_files(backend)?;
            let detected_languages: std::collections::HashSet<String> = scan
                .extractions
                .iter()
                .map(|e| e.language.clone())
                .collect();
            if !scan.extractions.is_empty() {
                backend.upsert_files_bulk(&scan.extractions, true)?;
            }
            let _resolve_stats = backend.resolve_calls(&scan.extractions, None)?;
            // The swap replaces the graph wholesale, so whatever TESTED_BY
            // edges the live graph had are about to be discarded -- derive
            // them here or they are gone for good. `None` scope means
            // "everything", matching how the local `--full` path calls it.
            // Non-fatal, mirroring that path's warn-and-continue.
            if let Err(e) = backend.derive_tested_by_edges(None) {
                eprintln!("[daemon] full-reindex: TESTED_BY derivation failed: {e}");
            }
            Ok(FullReindexBuildOutcome {
                indexed_files: scan.extractions.len(),
                detected_languages: detected_languages.into_iter().collect(),
            })
        });

    if build_result.is_err() {
        // The live graph was never touched -- clean up the incomplete
        // rebuild attempt so the next full reindex doesn't inherit a
        // half-built `graph.rebuilding` or a foreign-ID WAL.
        let _ = std::fs::remove_dir_all(&rebuilding_path);
        let _ = std::fs::remove_file(&rebuilding_path);
        crate::graph::remove_wal_family(&rebuilding_path);
    }

    build_result
}

/// Loop-thread entry point for a `WriteRequest::FullReindex`. Does the
/// cheap, synchronous gating (never overlap a queue drain or another full
/// reindex -- both write the same live graph) and, if clear, acquires
/// `index.lock` and hands the expensive build off to `drain_rt` so this loop
/// keeps ticking (accepting fsevents, other requests, the stop signal) for
/// the whole multi-minute rebuild instead of blocking on it. Returns `None`
/// if nothing was started (busy, or an early failure already replied and
/// cleaned up the request file itself); `Some` if a build was scheduled --
/// the caller must track the returned handle and reap it via
/// `finish_full_reindex` on a later tick.
#[allow(clippy::too_many_arguments)]
fn try_start_full_reindex<MR>(
    root: &Path,
    path: &Path,
    queue: &Arc<Mutex<crate::daemon::queue::IndexWorkQueue>>,
    make_registry: &MR,
    drain_in_flight: bool,
    full_reindex_in_flight: bool,
    drain_rt: &tokio::runtime::Runtime,
    daemon_token: &CancellationToken,
) -> Option<PendingFullReindex>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    let reply_path = path.with_extension("result");

    // Never overlap a queue drain or another full reindex -- same
    // invariant every locked write path in this loop preserves; both
    // touch the same live graph. Deferring here (rather than blocking)
    // gets "wait for whichever is in progress to finish first" for free:
    // `begin_index_op` below serializes on the same `index.lock` either
    // one takes regardless, but skipping the attempt when we already know
    // it's busy avoids spawning a task that's certain to lose the race.
    //
    // Deliberately does NOT check for an in-flight SCIP task: SCIP's
    // external-indexer phase touches nothing in the graph (see
    // `run_scip_indexers` in infigraph-cli), and its import phase acquires
    // `index.lock` itself, narrowly, around just that step -- so it
    // already serializes correctly against a concurrent full reindex
    // without needing a loop-level gate here too. Gating on it here would
    // reintroduce the exact regression this split fixed: SCIP's slow,
    // graph-independent indexer-running phase blocking every other write
    // for its entire duration.
    if drain_in_flight || full_reindex_in_flight {
        return None;
    }

    let guard = match begin_index_op(
        root,
        "infigraph daemon (full reindex)",
        Duration::from_secs(30),
    ) {
        Ok(IndexOpOutcome::Acquired(guard)) => guard,
        Ok(o @ IndexOpOutcome::AlreadyRunning(_)) => {
            eprintln!(
                "[daemon] full-reindex busy ({}), retrying next tick",
                o.skip_note().unwrap_or_default()
            );
            return None;
        }
        Err(e) => {
            eprintln!("[daemon] full-reindex busy ({e}), retrying next tick");
            return None;
        }
    };

    // Anything only queued (not yet executing) is genuinely moot -- the
    // full reindex is about to re-scan every file from disk regardless of
    // what was pending. Its waiters still get answered, just with a
    // superseded reply rather than silence.
    let superseded = queue.lock().unwrap().drain();
    for waiter in &superseded.waiters {
        let result = crate::daemon_protocol::WriteResult::Err {
            message: "superseded by a full reindex; resubmit if still needed".to_string(),
        };
        if let Ok(json) = serde_json::to_string(&result) {
            let _ = crate::daemon_protocol::write_atomic(&waiter.reply_path, &json);
        }
    }

    let registry = match make_registry() {
        Ok(r) => r,
        Err(e) => {
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!("full reindex failed: could not build language registry: {e}"),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(&reply_path, &json);
            }
            std::fs::remove_file(path).ok();
            drop(guard);
            return None;
        }
    };

    let root_buf = root.to_path_buf();
    // `Task::spawn_blocking` dispatches via the ambient `tokio::task::
    // spawn_blocking`, which needs a runtime context on this (plain OS)
    // thread -- `drain_rt.enter()` scopes that context to just this call, so
    // the task still runs on `drain_rt`'s blocking pool, matching every
    // other `spawn_blocking` in this loop.
    let task = {
        let _guard = drain_rt.enter();
        Task::spawn_blocking(daemon_token, "full-reindex-build", move |token| {
            FullReindexTaskOutput {
                result: build_full_reindex(&root_buf, registry, &token),
                guard,
            }
        })
    };

    Some(PendingFullReindex {
        task,
        request_path: path.to_path_buf(),
        reply_path,
    })
}

/// Loop-thread finish for a completed full-reindex build: poison the
/// daemon's own connection, swap the verified-good rebuilt graph in for the
/// live one (still under `graph.lock`, not `index.lock` -- see the
/// `graph_lock` comment below), reopen and reconcile embeddings, and reply.
/// Mirrors `finish_drain`'s "task does the heavy work, loop thread does the
/// `held`-touching part" split.
///
/// Returns the `index.lock` guard to drop (absent if the task panicked, since
/// it was dropped during the unwind -- same convention as `finish_drain`),
/// and `Some(detected_languages)` only when the swap fully succeeded --
/// that's the signal the caller uses to schedule SCIP enrichment. Any
/// failure path returns `None` for languages: don't enrich a reindex that
/// didn't actually land.
fn finish_full_reindex(
    root: &Path,
    reply_path: &Path,
    registry: &Arc<crate::lang::LanguageRegistry>,
    held: &mut Option<Arc<Infigraph>>,
    joined: std::result::Result<FullReindexTaskOutput, tokio::task::JoinError>,
) -> (Option<crate::ops::IndexOpGuard>, Option<Vec<String>>) {
    let FullReindexTaskOutput { guard, result } = match joined {
        Ok(output) => output,
        Err(join_err) => {
            eprintln!("[watch] full-reindex task panicked: {join_err}");
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!("daemon full-reindex task panicked: {join_err}"),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            return (None, None);
        }
    };

    let (indexed_files, detected_languages) = match result {
        Ok(outcome) => (outcome.indexed_files, outcome.detected_languages),
        Err(e) => {
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!("full reindex failed: {e:#}"),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            return (Some(guard), None);
        }
    };

    const LIVE_NAME: &str = "graph";
    const REBUILDING_NAME: &str = "graph.rebuilding";
    let infigraph_dir = root.join(".infigraph");
    let rebuilding_path = infigraph_dir.join(REBUILDING_NAME);
    let live_path = infigraph_dir.join(LIVE_NAME);

    // The live graph was never touched up to this point -- only now, with a
    // verified-good fresh build in hand, do we poison the daemon's own
    // handle and swap.
    poison_watch_db(held);

    // Replacing the live graph on disk is a graph-level write, so it takes
    // the same advisory lock every writer takes -- `index.lock` (held by
    // `guard`) does not cover writers that only take `graph.lock`, notably
    // `init()`'s corruption-retry calling `wipe_graph`. Scoped narrowly to
    // the destructive section, matching `wipe_graph`: the reopen further
    // down re-acquires this same lock through `GraphStore`, so holding it
    // any wider would deadlock against ourselves.
    let graph_lock = match crate::lockfile::acquire(
        &live_path.with_extension("lock"),
        "full-reindex-swap",
        Duration::from_secs(5),
    ) {
        Ok(l) => l,
        Err(e) => {
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!(
                    "full reindex rebuilt successfully but could not take the graph write lock \
                     to swap it in: {e:#}. The live graph at {} was left untouched; the rebuilt \
                     graph remains at {}",
                    live_path.display(),
                    rebuilding_path.display()
                ),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            return (Some(guard), None);
        }
    };

    // Bind the retirement destination rather than discarding it -- if the
    // move-aside succeeds but the rename below fails, `live_path` no longer
    // exists (it's now this destination), so an error message that names
    // `live_path` would point an operator at a path that's already gone and
    // never say where the real data actually went.
    let retired_path: Option<PathBuf> = if live_path.exists() {
        // Two backup mechanisms, deliberately layered. create_snapshot gives
        // a whole-`.infigraph/`-tree safety net matching the local
        // `infigraph index --full` path (R3.2.1/docs/DESIGN-hardening.md
        // §3.2), so a restore brings back graph and sidecars together.
        // retire_previous_graph then does the actual move-aside of the live
        // graph file -- a *rename*, not a delete, so a failure below (the
        // swap-in rename itself, or the swapped-in graph failing to reopen)
        // can restore the exact prior live graph with a single rename back
        // (see `roll_back_to_retired`), rather than the graph having been
        // permanently removed before the swap was even attempted -- the gap
        // an earlier version of this function had, caught by adversarial
        // review before it shipped.
        match crate::snapshot::create_snapshot(&infigraph_dir) {
            Ok(_snapshot_dest) => {
                match crate::quarantine::retire_previous_graph(&infigraph_dir, LIVE_NAME) {
                    Ok(dest) => Some(dest),
                    Err(e) => {
                        let result = crate::daemon_protocol::WriteResult::Err {
                            message: format!(
                                "full reindex rebuilt successfully but could not move the old \
                                 graph aside: {e:#}. The live graph at {} was left untouched; \
                                 the rebuilt graph remains at {}",
                                live_path.display(),
                                rebuilding_path.display()
                            ),
                        };
                        if let Ok(json) = serde_json::to_string(&result) {
                            let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
                        }
                        drop(graph_lock);
                        return (Some(guard), None);
                    }
                }
            }
            Err(e) => {
                let result = crate::daemon_protocol::WriteResult::Err {
                    message: format!(
                        "full reindex rebuilt successfully but could not snapshot the old graph \
                         aside: {e:#}. The live graph at {} was left untouched; the rebuilt \
                         graph remains at {}",
                        live_path.display(),
                        rebuilding_path.display()
                    ),
                };
                if let Ok(json) = serde_json::to_string(&result) {
                    let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
                }
                drop(graph_lock);
                return (Some(guard), None);
            }
        }
    } else {
        None
    };

    // Unconditional, regardless of which branch above ran: a `graph.wal*`
    // family can still be sitting at `live_path` even after a successful
    // retire (a copy-then-remove fallback can leave the original in place),
    // and when there was no base image to retire at all, an orphaned
    // `graph.wal*` from an earlier crash could still be here. Either way,
    // renaming the rebuilt graph on top of a foreign-ID WAL would make the
    // graph we just swapped in unopenable, so clear it unconditionally
    // right before the swap.
    crate::graph::remove_wal_family(&live_path);

    let swap = std::fs::rename(&rebuilding_path, &live_path);
    if swap.is_ok() {
        // The base image moved; its WAL-family siblings have to follow it
        // or a not-yet-checkpointed WAL belonging to the graph we just
        // swapped in is lost -- and worse, it stays behind at the rebuild
        // path for a later reindex to inherit. Same rename-then-copy
        // fallback the retirement path uses, for the same reason: a silent
        // failure here leaves a foreign WAL where it does damage.
        for src in crate::graph::wal_family_paths(&rebuilding_path) {
            let name = src.file_name().unwrap_or_default().to_string_lossy();
            let suffix = name.strip_prefix(REBUILDING_NAME).unwrap_or(&name);
            let dest = infigraph_dir.join(format!("{LIVE_NAME}{suffix}"));
            if let Err(e) = crate::quarantine::move_wal_sibling(&src, &dest) {
                eprintln!(
                    "[daemon] full-reindex: could not carry WAL sibling {} across to {} ({e:#}) \
                     -- the swapped-in graph may be missing uncheckpointed writes; the leftover \
                     is cleaned up by the next full reindex",
                    src.display(),
                    dest.display()
                );
            }
        }
    }
    if let Err(e) = &swap {
        // The rebuilt graph is verified-good and still sitting at
        // `rebuilding_path` -- nothing was lost. Roll the prior live graph
        // back into place immediately, while `graph_lock` is still held
        // (this is a plain rename, not a `GraphStore::open`, so no
        // deadlock risk) -- a failed swap must not leave the project
        // without a live graph at all, closing the outage window an
        // earlier version of this function had (caught by adversarial
        // review before it shipped).
        let rollback_note = roll_back_to_retired(&live_path, &retired_path);
        drop(graph_lock);
        eprintln!(
            "[daemon] full-reindex swap failed after a successful rebuild: {e} -- \
             {rollback_note}; the rebuilt graph is at {} -- check both by hand",
            rebuilding_path.display()
        );
        let result = crate::daemon_protocol::WriteResult::Err {
            message: format!(
                "full reindex rebuilt successfully but the swap failed: {e}. {rollback_note}; \
                 the rebuilt graph is at {}",
                rebuilding_path.display()
            ),
        };
        if let Ok(json) = serde_json::to_string(&result) {
            let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
        }
        return (Some(guard), None);
    }
    // graph_lock must be released before watch_db below -- it opens a fresh
    // GraphStore, which takes the same lock for schema init, and this
    // process already holding it would deadlock against itself.
    drop(graph_lock);

    // The swap succeeded -- verify the new graph actually opens before
    // declaring success and discarding the ability to roll back.
    match watch_db(root, registry, held) {
        Ok(prism) => {
            // R3.1.4d/#100: this is a *verified* healthy checkpoint -- the
            // swap succeeded and the swapped-in graph just reopened -- so
            // it's the right moment to refresh the growth-ratio breaker's
            // baseline. Ordinary incremental writes deliberately do not
            // (see `stamp_healthy_graph_size`'s doc comment).
            crate::graph::stamp_healthy_graph_size(&infigraph_dir, &live_path);

            // Reconcile embeddings against the NEW graph -- update_embeddings
            // queries the live symbol set and prunes anything not in it, so
            // this converges embeddings.bin to the rebuilt graph regardless
            // of whether it was wiped first (it wasn't, deliberately).
            if let Some(backend) = prism.backend() {
                if let Err(e) = crate::embed::update_embeddings(backend, root, &[]) {
                    eprintln!("[daemon] full-reindex: embedding update failed: {e}");
                }
            }
            let result = crate::daemon_protocol::WriteResult::FullReindexOk {
                total_files: indexed_files,
                indexed_files,
                detected_languages: detected_languages.clone(),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            (Some(guard), Some(detected_languages))
        }
        Err(reopen_err) => {
            // The swapped-in graph doesn't even reopen. Quarantine it as
            // verified-bad (R3.1.2) and roll the prior live graph back into
            // place rather than leaving a broken graph live or the project
            // graph-less. Re-acquire graph_lock just for these renames (no
            // GraphStore::open happens here, so no deadlock risk against
            // the lock dropped above) -- otherwise a concurrent single-file
            // write could race the rollback, the same class of gap
            // `full_reindex_wipe` closes on the local path.
            let rollback_note = match crate::lockfile::acquire(
                &live_path.with_extension("lock"),
                "full-reindex-rollback",
                std::time::Duration::from_secs(5),
            ) {
                Ok(_lock) => {
                    let _ = crate::quarantine::quarantine_graph(&infigraph_dir, LIVE_NAME);
                    roll_back_to_retired(&live_path, &retired_path)
                }
                Err(e) => format!(
                    "could not acquire the graph lock to roll back: {e:#} -- manual recovery needed"
                ),
            };
            eprintln!(
                "[daemon] full-reindex: swapped-in graph failed to reopen: {reopen_err:#} -- \
                 quarantined it; {rollback_note}"
            );
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!(
                    "full reindex swap completed but the new graph failed to reopen: \
                     {reopen_err:#}. The broken graph was quarantined; {rollback_note}"
                ),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            (Some(guard), None)
        }
    }
}

/// Attempt to restore the graph `retire_previous_graph` moved aside (see
/// `finish_full_reindex`'s `retired_path`) back to `live_path`, after
/// either the swap-in rename failed or the swapped-in graph failed to
/// reopen. Returns a human-readable note for both the daemon log and the
/// `WriteResult` error message.
fn roll_back_to_retired(live_path: &Path, retired_path: &Option<PathBuf>) -> String {
    match retired_path {
        Some(prev) if prev.exists() => match std::fs::rename(prev, live_path) {
            Ok(()) => "the prior live graph was restored".to_string(),
            Err(e) => format!(
                "the prior live graph at {} could NOT be restored: {e:#} -- manual recovery needed",
                prev.display()
            ),
        },
        Some(prev) => format!(
            "no prior graph to restore from (expected at {}, already gone)",
            prev.display()
        ),
        None => "there was no prior graph to restore (this was a from-scratch build)".to_string(),
    }
}

/// Parses a `.request` file and either enqueues it (for the four
/// index-shaped `WriteRequest` variants this design coordinates) or falls
/// through to `serve_request_locked` (unchanged `serve_one_request`
/// dispatch, still under `index.lock`) for everything else. Enqueued
/// requests' `.request` file is deleted immediately (the daemon has already
/// accepted responsibility for serving it the moment it's queued) -- the
/// reply arrives later, written by `execute_drain`.
#[allow(clippy::too_many_arguments)]
fn route_or_serve_request<MR>(
    root: &Path,
    path: &Path,
    queue: &Arc<Mutex<crate::daemon::queue::IndexWorkQueue>>,
    registry: &Arc<crate::lang::LanguageRegistry>,
    make_registry: &MR,
    held: &mut Option<Arc<Infigraph>>,
    drain_in_flight: bool,
    full_reindex_in_flight: bool,
    drain_rt: &tokio::runtime::Runtime,
    daemon_token: &CancellationToken,
    code_watch: &mut CodeWatch,
    docs_control: Option<&Arc<DocsControl>>,
    // Set to `true` when the request asks the whole daemon to stop; the
    // coordinator's loop reads it to decide whether to break.
    shutdown_requested: &mut bool,
) -> Option<PendingFullReindex>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    let reply_path = path.with_extension("result");
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return None, // transient; will be retried next tick if the file reappears
    };
    let request: crate::daemon_protocol::WriteRequest = match serde_json::from_str(&contents) {
        Ok(r) => r,
        Err(_) => {
            // Malformed request JSON -- not this design's concern to
            // recover; hand off to serve_one_request, whose existing
            // corrupt-JSON handling (WriteResult::Err) already covers it.
            serve_request_locked(root, path, registry, held, drain_in_flight);
            return None;
        }
    };

    use crate::daemon::queue::{Waiter, WaiterKind};
    use crate::daemon_protocol::WriteRequest;

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
                paths: None,
            });
            drop(q);
            std::fs::remove_file(path).ok();
            None
        }
        WriteRequest::Index { paths: Some(paths) } => {
            let mut q = queue.lock().unwrap();
            let mut rel_paths = Vec::with_capacity(paths.len());
            for p in paths {
                // `p` may be absolute -- `Infigraph::index_file`/`index_files`
                // both forward the caller's path verbatim (absolute paths are
                // an explicitly documented option), and `extract_paths` below
                // joins it onto `root` unnormalized, which for an absolute
                // path is a no-op join that leaves it absolute. Mirrors the
                // batch-flush block above.
                let rel = p
                    .strip_prefix(root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| p.to_string_lossy().replace('\\', "/"));
                q.add_raw(rel.clone());
                rel_paths.push(rel);
            }
            q.add_waiter(Waiter {
                kind: WaiterKind::Index,
                use_learned: false,
                reply_path,
                paths: Some(rel_paths),
            });
            drop(q);
            std::fs::remove_file(path).ok();
            None
        }
        WriteRequest::UpsertFilesBulk {
            extractions_path,
            // `existing_hashes_empty` is a snapshot the client captured when
            // it built this request; it's not threaded through the queue
            // because `execute_drain` recomputes the same flag itself, at
            // actual drain time, from the backend's live file-hash state --
            // strictly fresher than the client's snapshot, especially once
            // other producers' work has been folded into the same drain.
            // Dropped deliberately, not an oversight.
            ..
        } => match crate::daemon_protocol::read_extractions_json(&extractions_path) {
            Ok(extractions) => {
                let mut q = queue.lock().unwrap();
                let rel_paths: Vec<String> = extractions.iter().map(|e| e.file.clone()).collect();
                for extraction in extractions {
                    q.add_structured(extraction);
                }
                q.add_waiter(Waiter {
                    kind: WaiterKind::UpsertFilesBulk,
                    use_learned: false,
                    reply_path,
                    paths: Some(rel_paths),
                });
                drop(q);
                std::fs::remove_file(&extractions_path).ok();
                std::fs::remove_file(path).ok();
                None
            }
            Err(_) => {
                // Sibling extractions file missing/corrupt -- fall
                // through to serve_one_request's existing error path.
                serve_request_locked(root, path, registry, held, drain_in_flight);
                None
            }
        },
        WriteRequest::RemoveFiles { files } => {
            let mut q = queue.lock().unwrap();
            let rel_paths = files.clone();
            for f in files {
                q.add_removal(f);
            }
            q.add_waiter(Waiter {
                kind: WaiterKind::RemoveFiles,
                use_learned: false,
                reply_path,
                paths: Some(rel_paths),
            });
            drop(q);
            std::fs::remove_file(path).ok();
            None
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
                    // ResolveCalls replies carry `ResolveStats` (call-edge
                    // counts), not a file count -- not path-attributable the
                    // way Index/UpsertFilesBulk/RemoveFiles are.
                    paths: None,
                });
                drop(q);
                std::fs::remove_file(&extractions_path).ok();
                std::fs::remove_file(path).ok();
                None
            }
            Err(_) => {
                serve_request_locked(root, path, registry, held, drain_in_flight);
                None
            }
        },
        WriteRequest::FullReindex => try_start_full_reindex(
            root,
            path,
            queue,
            make_registry,
            drain_in_flight,
            full_reindex_in_flight,
            drain_rt,
            daemon_token,
        ),
        WriteRequest::WatchControl { role, action } => {
            let outcome = match role {
                // `Enable`/`Disable` differ from `Start`/`Stop` only in
                // whether the *caller* also wrote the persisted flag in
                // config.toml (Phase 4). Their effect on the live task is
                // identical, so this arm treats them the same.
                WatchRole::Code => {
                    match action {
                        WatchAction::Stop | WatchAction::Disable => code_watch.stop(),
                        WatchAction::Start | WatchAction::Enable => code_watch.start(),
                        WatchAction::Restart => {
                            code_watch.stop();
                            code_watch.start();
                        }
                    }
                    Ok(())
                }
                WatchRole::Docs => match docs_control {
                    Some(control) => control(action),
                    None => {
                        Err("this watcher does not own a doc-watch loop to control".to_string())
                    }
                },
                // Only the process's own exit is expressible here: `Start`
                // is meaningless (you are talking to a daemon, so one
                // exists), and a real `Restart` is the *client's* job --
                // this process can only stop itself. Both stop; the reply
                // is written before cancelling so the caller still gets it.
                WatchRole::Daemon => match action {
                    WatchAction::Stop | WatchAction::Restart => Ok(()),
                    _ => {
                        Err("WatchControl { role: Daemon } only supports Stop/Restart".to_string())
                    }
                },
            };
            let daemon_stop = matches!(
                (role, action, &outcome),
                (
                    WatchRole::Daemon,
                    WatchAction::Stop | WatchAction::Restart,
                    Ok(())
                )
            );
            reply_to_watch_control(&reply_path, outcome);
            std::fs::remove_file(path).ok();
            if daemon_stop {
                // Two separate signals, deliberately: the token tears down
                // whatever background work is still spawned beneath it, and
                // the flag tells the coordinator's own loop to stop ticking.
                // The loop must not infer the second from the first -- see
                // `shutdown_requested`'s declaration.
                *shutdown_requested = true;
                daemon_token.cancel();
            }
            None
        }
        _ => {
            serve_request_locked(root, path, registry, held, drain_in_flight);
            None
        }
    }
}

/// Answers a `WatchControl` request. Same `write_atomic`/`WriteResult`
/// shape every other reply in this module uses; the counts are zero because
/// watch-control moves no files through the graph.
fn reply_to_watch_control(reply_path: &Path, outcome: std::result::Result<(), String>) {
    let result = match outcome {
        Ok(()) => crate::daemon_protocol::WriteResult::Ok {
            total_files: 0,
            indexed_files: 0,
        },
        Err(message) => crate::daemon_protocol::WriteResult::Err { message },
    };
    if let Ok(json) = serde_json::to_string(&result) {
        let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
    }
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

        let mut queue = crate::daemon::queue::IndexWorkQueue::new();
        queue.add_waiter(crate::daemon::queue::Waiter {
            kind: crate::daemon::queue::WaiterKind::Index,
            use_learned: false,
            reply_path: ok_reply.clone(),
            paths: None,
        });
        queue.add_waiter(crate::daemon::queue::Waiter {
            kind: crate::daemon::queue::WaiterKind::Index,
            use_learned: false,
            reply_path: fail_reply.clone(),
            paths: None,
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
        let drain_result = crate::daemon::drain::execute_drain(&prism, drained);
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

    /// Regression test for a final-review Critical finding: `WriteRequest::Index
    /// { paths: Some(paths) }` used to enqueue each path via
    /// `p.to_string_lossy()` without stripping `root` first, so an absolute
    /// path -- an explicitly documented option on `Infigraph::index_file`/
    /// `index_files`, both of which forward the caller's path verbatim into
    /// this same request type -- ended up keyed by its absolute string in
    /// the queue. `extract_paths` then joins that onto `root`, which for an
    /// absolute path is a no-op join that leaves it absolute, so
    /// `FileExtraction.file` (and therefore the resulting graph node's id)
    /// ended up absolute too: a second `File` node alongside the real
    /// relative one, un-deduplicated and never cleaned up by `remove_file`.
    #[test]
    fn route_or_serve_index_request_normalizes_an_absolute_path_to_relative_before_enqueuing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::write(root.join("foo.py"), "def foo():\n    pass\n").unwrap();

        let abs_path = root.join("foo.py");
        let request = crate::daemon_protocol::WriteRequest::Index {
            paths: Some(vec![abs_path]),
        };
        let request_path = root.join("test.request");
        std::fs::write(&request_path, serde_json::to_string(&request).unwrap()).unwrap();

        let queue = Arc::new(Mutex::new(crate::daemon::queue::IndexWorkQueue::new()));
        let mut held: Option<Arc<Infigraph>> = None;
        let drain_rt = tokio::runtime::Runtime::new().unwrap();
        let daemon_token = CancellationToken::new();
        let registry = Arc::new(crate::lang::LanguageRegistry::new());
        // Never started -- this test exercises request routing only, and a
        // live producer would race it by queueing its own fsevents.
        let mut code_watch = CodeWatch::new(
            &daemon_token,
            producer::ProducerConfig {
                root: root.clone(),
                registry: Arc::clone(&registry),
                debounce_ms: 50,
                ignore_rebuild_secs: 300,
            },
            Arc::clone(&queue),
            Arc::new(|_evt| {}),
        )
        .unwrap();
        route_or_serve_request(
            &root,
            &request_path,
            &queue,
            &registry,
            &|| Ok(crate::lang::LanguageRegistry::new()),
            &mut held,
            false,
            false,
            &drain_rt,
            &daemon_token,
            &mut code_watch,
            None,
            &mut false,
        );

        let drained = queue.lock().unwrap().drain();
        assert_eq!(drained.items.len(), 1, "expected exactly one queued item");
        assert!(
            drained.items.contains_key("foo.py"),
            "an absolute input path must be normalized to a root-relative key \
             before enqueuing, got keys: {:?}",
            drained.items.keys().collect::<Vec<_>>()
        );

        assert_eq!(
            drained.waiters[0].paths,
            Some(vec!["foo.py".to_string()]),
            "the waiter's own scoped paths must also be relative"
        );
    }
}
