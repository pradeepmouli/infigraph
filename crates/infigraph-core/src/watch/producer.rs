//! The fsevent-watching producer: owns a `notify::Watcher`, marks dirty
//! paths, and feeds `IndexWorkQueue` -- nothing else. Never touches Kuzu,
//! `held_prism`, or drain-scheduling state; see
//! docs/superpowers/specs/2026-08-21-daemon-watch-command-split-design.md
//! "What a code/docs `Task<()>` never touches directly".
//!
//! This is the async counterpart of the fsevent half of
//! `run_write_coordinator`, which now spawns it as the `code` producer
//! `Task<()>` so watch activity can be started and stopped independently of
//! the daemon process's own lifetime.

use crate::daemon::queue::IndexWorkQueue;
use crate::watch::{WatchEvent, WatchEventKind};
use clap::Parser;
use notify::{Config, Event, EventKind, RecommendedWatcher, Watcher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_util::sync::CancellationToken;

const MAX_RESTARTS: u32 = 3;

/// Everything a producer needs that is fixed for its whole lifetime. A
/// struct rather than four more positional parameters because
/// `run_write_coordinator` both spawns the initial producer and respawns
/// one per `WatchControl { role: Code, action: Start|Restart }` request, and
/// two u64s side by side are exactly the kind of argument pair a respawn
/// site can silently transpose.
#[derive(Clone)]
pub struct ProducerConfig {
    pub root: PathBuf,
    /// Gates which Created/Modified paths get marked dirty and batched. A
    /// path with no language pack behind it never produces a
    /// `FileExtraction`, so marking it dirty leaves it dirty forever:
    /// `clear_dirty` only clears what a drain's `outcome.extractions`
    /// reported. Without this filter every README/lockfile/image touched
    /// under `root` leaks into a permanently-growing dirty set.
    pub registry: Arc<crate::lang::LanguageRegistry>,
    /// Only read by notify's `PollWatcher` fallback -- the native backends
    /// (FSEvents/inotify/ReadDirectoryChangesW) ignore it -- but the
    /// synchronous coordinator passed it, so it is passed here too rather
    /// than silently regressing whichever platform falls back to polling.
    pub debounce_ms: u64,
    /// How often to rebuild the ignore matcher so mid-watch edits to
    /// `.gitignore`/`.infigraphignore` take effect without a daemon restart.
    pub ignore_rebuild_secs: u64,
}

/// Cadence for the two things that still need a timer rather than an event:
/// closing the `ChangeBatch` debounce window, and noticing the watched root
/// disappeared. 200ms matches the `recv_timeout` tick the synchronous loop
/// used, so batch flushes land on the same schedule they always have.
const TICK: Duration = Duration::from_millis(200);

/// How often to re-check that `root` still exists. Far slower than `TICK`
/// because the check is only meaningful in the rare `rm -rf`-the-project
/// case, and the whole point of the async rewrite is to stop polling for
/// things that essentially never change.
const ROOT_CHECK: Duration = Duration::from_secs(2);

/// Watch `cfg.root` for filesystem changes and feed them into `queue`.
///
/// Runs until `token` is cancelled, the watcher dies past `MAX_RESTARTS`
/// restart attempts, or the root stops existing. Spawnable via
/// `Task::spawn(parent, role, |token| run_producer(cfg, queue, on_event, token))`.
pub async fn run_producer(
    cfg: ProducerConfig,
    queue: Arc<Mutex<IndexWorkQueue>>,
    on_event: impl Fn(WatchEvent) + Send + 'static,
    token: CancellationToken,
) {
    let ProducerConfig {
        root,
        registry,
        debounce_ms,
        ignore_rebuild_secs,
    } = cfg;
    // Some watch backends (e.g. FSEvents on macOS) deliver absolute,
    // symlink-resolved event paths regardless of how `root` was specified.
    // If `root` is relative, or traverses a symlink (macOS temp dirs live
    // under /var, itself a symlink to /private/var), `path.strip_prefix(root)`
    // below silently fails for every event and all changes are dropped.
    let root = root.canonicalize().unwrap_or(root);
    let infigraph_dir = root.join(".infigraph");

    // R3.3.5: recover any dirty marks a prior run of this watcher left
    // behind -- e.g. it crashed between a raw fsevent and the debounce
    // window's flush, so the change never made it into a drain. A path
    // still present on disk needs a fresh extraction; one that's gone was
    // a removal that never got applied (`add_watch_removal` is safe either
    // way -- its directory-prefix scan is a harmless no-op for a path that
    // turns out to have been a file). This is what makes the recovery
    // targeted at exactly the paths left mid-flight instead of requiring a
    // full rescan.
    match crate::dirty::pending_dirty(&infigraph_dir) {
        Ok(leftover) if !leftover.is_empty() => {
            eprintln!(
                "[watch-producer] recovering {} dirty path(s) left over from a prior run",
                leftover.len()
            );
            let mut q = queue.lock().unwrap();
            for rel in leftover {
                if root.join(&rel).exists() {
                    q.add_raw(rel);
                } else {
                    q.add_watch_removal(rel);
                }
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("[watch-producer] failed to read dirty set, skipping recovery: {e}"),
    }

    let mut ignore_matcher = crate::ignore_rules::IgnoreMatcher::build(&root);

    // Batch accumulator: collect file changes over a 1-second window so the
    // consumer indexes them all at once via the bulk write path.
    let mut batch = crate::watch::batch::ChangeBatch::new(1000);

    // R7.4 (#84): above this many files in one debounce window, the batch
    // coalesces into a single whole-project pass instead of per-file
    // updates. Overridable via INFIGRAPH_WATCH_STORM_THRESHOLD for tests/tuning.
    let cli = crate::watch::RawWatch::parse_from(std::iter::empty::<String>());
    let storm_threshold: usize = crate::watch::Watch::resolve(cli, None).storm_threshold as usize;

    let (mut watcher, mut rx) = match create_watcher(&root, debounce_ms) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[watch-producer] failed to start watcher: {e}");
            // Same event every other terminal path emits. Without it a
            // producer whose watcher never started is indistinguishable,
            // from the coordinator's side, from one that shut down cleanly
            // on cancellation -- the task just completes and nothing says
            // watching never actually began.
            on_event(WatchEvent {
                kind: WatchEventKind::WatcherDied,
                path: root.clone(),
                has_cross_file_calls: false,
            });
            return;
        }
    };
    let mut restart_count: u32 = 0;

    // `Delay` rather than the default `Burst`: these are idle cadences, not a
    // fixed grid. After a stall (the restart backoff below sleeps for
    // seconds) `Burst` would fire every tick it missed back to back, where
    // the synchronous loop's `recv_timeout` simply restarted its 200ms wait
    // from wherever the iteration actually ended.
    let mut tick = interval_after(TICK);
    let mut root_check = interval_after(ROOT_CHECK);
    let mut ignore_rebuild = interval_after(Duration::from_secs(ignore_rebuild_secs.max(1)));

    loop {
        tokio::select! {
            _ = token.cancelled() => break,

            _ = tick.tick() => {
                flush_ready_batch(&mut batch, &queue, &root, storm_threshold);
            }

            _ = root_check.tick() => {
                // Self-terminate once the watched root is gone (`rm -rf`'d
                // project, a test's tempdir, a removed worktree that skipped
                // `worktree teardown`). Without it, a daemon whose target
                // directory disappeared keeps watching nothing forever:
                // `prune_stale_holder` only reaps a *dead* holder, and this
                // process is very much alive. `infigraph gc --global` sweeps
                // the registry for the same condition as a backstop.
                if !root.exists() {
                    eprintln!("[watch-producer] {} no longer exists -- shutting down", root.display());
                    break;
                }
            }

            _ = ignore_rebuild.tick() => {
                ignore_matcher = crate::ignore_rules::IgnoreMatcher::build(&root);
            }

            maybe_evt = rx.recv() => {
                match maybe_evt {
                    Some(Ok(event)) => {
                        let watch_kind = match event.kind {
                            EventKind::Create(_) => WatchEventKind::Created,
                            EventKind::Modify(_) => WatchEventKind::Modified,
                            EventKind::Remove(_) => WatchEventKind::Removed,
                            _ => continue,
                        };

                        for path in event.paths {
                            if ignore_matcher.is_ignored(&path, path.is_dir()) {
                                continue;
                            }

                            let rel = match path.strip_prefix(&root) {
                                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                                Err(_) => continue,
                            };

                            match watch_kind {
                                WatchEventKind::Removed => {
                                    // R3.3.5: persisted before the in-memory
                                    // queue add below, so a crash between the
                                    // two still leaves this path recoverable
                                    // at next startup.
                                    if let Err(e) = crate::dirty::mark_dirty(
                                        &infigraph_dir,
                                        std::slice::from_ref(&rel),
                                    ) {
                                        eprintln!("[watch-producer] failed to persist dirty mark for {rel}: {e}");
                                    }
                                    // `add_watch_removal` (not `add_removal`)
                                    // because this is a real filesystem
                                    // removal: `path` is already gone from
                                    // disk, so there is no way to tell here
                                    // whether it named a file or a directory
                                    // -- the drain scans for and removes
                                    // anything nested under it too.
                                    queue.lock().unwrap().add_watch_removal(rel);
                                    on_event(WatchEvent {
                                        kind: watch_kind.clone(),
                                        path,
                                        has_cross_file_calls: false,
                                    });
                                }
                                WatchEventKind::Created | WatchEventKind::Modified => {
                                    if path.is_dir() {
                                        // A new subdirectory needs its own
                                        // subscription (these are registered
                                        // NonRecursive). Log a failure rather
                                        // than discarding it: silently not
                                        // watching a new directory means
                                        // every later change under it is
                                        // missed with nothing to explain why.
                                        if let Err(e) =
                                            crate::watch::register_watch_dirs(&mut watcher, &path)
                                        {
                                            eprintln!(
                                                "[watch-producer] failed to watch new directory {}: {e}",
                                                path.display()
                                            );
                                        }
                                    } else if registry.for_file(&rel).is_some() {
                                        // R3.3.5: persisted before this event
                                        // enters `batch` -- the in-memory
                                        // accumulator a crash could still lose
                                        // between now and the flush below.
                                        if let Err(e) = crate::dirty::mark_dirty(
                                            &infigraph_dir,
                                            std::slice::from_ref(&rel),
                                        ) {
                                            eprintln!("[watch-producer] failed to persist dirty mark for {rel}: {e}");
                                        }
                                        batch.add(path);
                                    }
                                }
                                WatchEventKind::WatcherRestarted | WatchEventKind::WatcherDied => {}
                            }
                        }
                    }
                    Some(Err(e)) => eprintln!("[watch-producer] watch error: {e}"),
                    None => {
                        // Every sender for this generation's channel is gone,
                        // i.e. the watcher's internal thread died (e.g. kqueue
                        // panic on dir deletion) -- the async equivalent of
                        // the synchronous loop's `RecvTimeoutError::Disconnected`.
                        // Attempt restart with backoff.
                        restart_count += 1;
                        if restart_count > MAX_RESTARTS {
                            eprintln!("[watch-producer] watcher died {restart_count} times, giving up");
                            on_event(WatchEvent {
                                kind: WatchEventKind::WatcherDied,
                                path: root.clone(),
                                has_cross_file_calls: false,
                            });
                            break;
                        }
                        let backoff = Duration::from_secs(restart_count as u64);
                        eprintln!(
                            "[watch-producer] watcher disconnected, restarting ({restart_count}/{MAX_RESTARTS}) after {}s",
                            backoff.as_secs()
                        );
                        tokio::select! {
                            _ = token.cancelled() => break,
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        match create_watcher(&root, debounce_ms) {
                            Ok((new_watcher, new_rx)) => {
                                watcher = new_watcher;
                                rx = new_rx;
                                eprintln!("[watch-producer] watcher restarted successfully");
                                on_event(WatchEvent {
                                    kind: WatchEventKind::WatcherRestarted,
                                    path: root.clone(),
                                    has_cross_file_calls: false,
                                });
                            }
                            Err(e) => {
                                eprintln!("[watch-producer] watcher restart failed: {e}");
                                on_event(WatchEvent {
                                    kind: WatchEventKind::WatcherDied,
                                    path: root.clone(),
                                    has_cross_file_calls: false,
                                });
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// An interval whose first tick lands one full `period` from now, rather
/// than immediately as `tokio::time::interval` does -- startup already did
/// the work each of these timers exists to repeat.
fn interval_after(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// Builds a watcher registered on every non-ignored directory under `root`,
/// paired with the receiver for its events.
///
/// A fresh channel per watcher generation is what makes `rx.recv()`
/// returning `None` mean "this watcher died" -- notify moves the event
/// handler into the backend, so the handler (and with it the only sender)
/// is dropped exactly when the synchronous version's `Sender` was, which is
/// what produced `RecvTimeoutError::Disconnected` there.
fn create_watcher(
    root: &Path,
    debounce_ms: u64,
) -> anyhow::Result<(
    RecommendedWatcher,
    tokio_mpsc::UnboundedReceiver<notify::Result<Event>>,
)> {
    let (tx, rx) = tokio_mpsc::unbounded_channel::<notify::Result<Event>>();
    // An unbounded sender never blocks, so this handler is safe to call from
    // notify's own (synchronous, non-async) backend thread.
    let mut watcher = RecommendedWatcher::new(
        move |evt| {
            let _ = tx.send(evt);
        },
        Config::default().with_poll_interval(Duration::from_millis(debounce_ms)),
    )?;
    crate::watch::register_watch_dirs(&mut watcher, root)?;
    Ok((watcher, rx))
}

/// Flushes the batch into the shared queue once its debounce window closes.
/// Runs on a timer rather than off the back of an event: the window only
/// elapses *after* the last event, so an event-triggered check would leave
/// a batch stranded whenever changes stop arriving -- which is the common
/// case (someone saves a file and pauses).
fn flush_ready_batch(
    batch: &mut crate::watch::batch::ChangeBatch,
    queue: &Arc<Mutex<IndexWorkQueue>>,
    root: &Path,
    storm_threshold: usize,
) {
    if batch.is_empty() || !batch.is_ready() {
        return;
    }
    let paths = batch.drain();
    let mut q = queue.lock().unwrap();
    crate::watch::flush_batch_into_queue(&mut q, paths, root, storm_threshold);
}
