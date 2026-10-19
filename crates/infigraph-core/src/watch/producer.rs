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
use crate::watch::registration::Registration;
use crate::watch::{WatchEvent, WatchEventKind};
use clap::Parser;
use notify::{Config, Event, EventKind, RecommendedWatcher, Watcher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_util::sync::CancellationToken;

const MAX_RESTARTS: u32 = 3;

/// The producer's watcher, shared with the thread registering a new
/// directory on it: a cancel stops the wait for that thread, not the thread,
/// which drops its handle -- and with the producer gone, the watcher -- when
/// it finishes.
type SharedWatcher = Arc<Mutex<RecommendedWatcher>>;

/// Everything a producer needs that is fixed for its whole lifetime. A
/// struct rather than four more positional parameters because
/// `run_write_coordinator` both spawns the initial producer and respawns
/// one per `Control { role: Code, action: Start|Restart }` request, and
/// two u64s side by side are exactly the kind of argument pair a respawn
/// site can silently transpose.
#[derive(Clone)]
pub struct ProducerConfig {
    pub root: PathBuf,
    /// Gates which Created/Modified paths get marked dirty and batched. A
    /// path with no language pack behind it never produces a
    /// `FileExtraction`, so marking it dirty leaves it dirty forever:
    /// `clear_dirty` only clears what a drain's `outcome.extractions`
    /// reported. Without this filter every README/image touched under `root`
    /// leaks into a permanently-growing dirty set.
    ///
    /// It does **not** cover dependency lockfiles, and used to claim it did:
    /// `pnpm-lock.yaml` and `package-lock.json` are claimed by the bundled
    /// YAML and JSON packs, so they pass this gate and were indexed in full
    /// (3,498 symbols from one lockfile on sittir). `store_util::is_lockfile`,
    /// checked beside this at the call site, is what stops those.
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

    let mut ignore_matcher =
        crate::ignore_rules::IgnoreMatcher::build(&root, crate::ignore_rules::Domain::Code);

    // Batch accumulator: collect file changes over a 1-second window so the
    // consumer indexes them all at once via the bulk write path.
    let mut batch = crate::watch::batch::ChangeBatch::new(1000);

    // R7.4 (#84): above this many files in one debounce window, the batch
    // coalesces into a single whole-project pass instead of per-file
    // updates. Overridable via INFIGRAPH_WATCH_STORM_THRESHOLD for tests/tuning.
    let cli = crate::watch::RawWatch::parse_from(std::iter::empty::<String>());
    let storm_threshold: usize = crate::watch::Watch::resolve_or_default(
        cli,
        crate::settings_file::ConfigScope::Project(&root),
    )
    .storm_threshold as usize;

    let (mut watcher, mut rx) =
        match create_watcher_unless_cancelled(&root, debounce_ms, &token).await {
            None => return,
            Some(Ok(pair)) => pair,
            Some(Err(e)) => {
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
    // Registered, so nothing can fall between the two: events from here on
    // are seen, and this pass finds whatever happened before them.
    let mut dropped = DroppedEvents::default();
    queue_true_up(&queue, "the watcher started");
    dropped.queued(std::time::Instant::now());
    crate::degraded::live::clear(&root, crate::degraded::CODE_WATCHER_DEAF);
    let canary_path = infigraph_dir.join(CANARY_FILE);
    let mut canary = Canary::new(std::time::Instant::now());
    let canary_deaf_for_test = std::env::var_os("INFIGRAPH_TEST_WATCH_CANARY_DEAF").is_some();

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
                let now = std::time::Instant::now();
                if dropped.due(now) {
                    let rate = dropped.report(now);
                    queue_true_up(&queue, &format!("the watcher reported dropped events: {rate}"));
                    dropped.queued(now);
                }
                match canary.step(now) {
                    CanaryStep::Idle => {}
                    CanaryStep::Probe => {
                        // No `.infigraph/` (a removed project) means nothing
                        // to probe; the root check ends the producer.
                        if infigraph_dir.is_dir()
                            && std::fs::write(&canary_path, canary_stamp()).is_ok()
                        {
                            canary.probed(now);
                        } else {
                            canary.skipped(now);
                        }
                    }
                    CanaryStep::Missed(misses) => {
                        eprintln!(
                            "[watch-producer] {} the watcher did not report its own probe write \
                             within {}s (miss {misses}); it may be deaf, rebuilding its subscription",
                            crate::watch::log_stamp(),
                            canary.wait.as_secs_f32()
                        );
                        crate::degraded::live::set(
                            &root,
                            crate::degraded::DegradedMode::CodeWatcherDeaf { misses },
                        );
                        // Whatever it missed is found by the true-up the
                        // debounce allows, not by one per miss.
                        dropped.note();
                        match create_watcher_unless_cancelled(&root, debounce_ms, &token).await {
                            None => break,
                            Some(Ok((new_watcher, new_rx))) => {
                                watcher = new_watcher;
                                rx = new_rx;
                            }
                            Some(Err(e)) => {
                                eprintln!("[watch-producer] rebuilding the watcher failed: {e}");
                            }
                        }
                    }
                }
                if canary.take_recovered() {
                    eprintln!(
                        "[watch-producer] {} the watcher reports its own probe write again",
                        crate::watch::log_stamp()
                    );
                    crate::degraded::live::clear(&root, crate::degraded::CODE_WATCHER_DEAF);
                }
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
                ignore_matcher = crate::ignore_rules::IgnoreMatcher::build(&root, crate::ignore_rules::Domain::Code);
            }

            maybe_evt = rx.recv() => {
                match maybe_evt {
                    Some(Ok(event)) => {
                        if events_were_dropped(&event) {
                            dropped.note();
                        }
                        let watch_kind = match event.kind {
                            EventKind::Create(_) => WatchEventKind::Created,
                            EventKind::Modify(_) => WatchEventKind::Modified,
                            EventKind::Remove(_) => WatchEventKind::Removed,
                            _ => continue,
                        };

                        for path in event.paths {
                            // Spelled as the root is: a backend that reports
                            // resolved paths would otherwise fail the
                            // `strip_prefix` below and drop the event.
                            let path = ignore_matcher.respelled(&path).into_owned();
                            // Its own probe: proof the stream delivers, and
                            // nothing to index.
                            if path == canary_path {
                                if !canary_deaf_for_test {
                                    canary.seen();
                                }
                                continue;
                            }
                            // What the walk would not visit, hidden files
                            // included: a `.foo.py` it never indexes would
                            // otherwise leave a dirty mark nothing clears.
                            if ignore_matcher.skips(&path, path.is_dir()) {
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
                                        // Under a recursive subscription
                                        // (macOS) a new directory is already
                                        // watched.
                                        if !crate::watch::WATCHES_EACH_DIRECTORY {
                                            continue;
                                        }
                                        // A new subdirectory needs its own
                                        // subscription (these are registered
                                        // NonRecursive), and registering it
                                        // is the same blocking `fseventsd`
                                        // call as startup's, so it runs off
                                        // this task too. Log a failure rather
                                        // than discarding it: silently not
                                        // watching a new directory means
                                        // every later change under it is
                                        // missed with nothing to explain why.
                                        let shared = Arc::clone(&watcher);
                                        let dir = path.clone();
                                        let registered = Registration::start(&root, move || {
                                            let mut watcher = shared
                                                .lock()
                                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                                            crate::watch::register_watch_dirs(&mut watcher, &dir)
                                        })
                                        .unless_cancelled(&token)
                                        .await;
                                        match registered {
                                            None => break,
                                            // A new directory's failures add
                                            // to the mode; its success says
                                            // nothing about the rest.
                                            Some(Ok(registration)) => registration.note(&root, false),
                                            Some(Err(e)) => {
                                                eprintln!(
                                                    "[watch-producer] failed to watch new directory {}: {e}",
                                                    path.display()
                                                );
                                                crate::watch::WatchRegistration {
                                                    failed: 1,
                                                    first_error: Some(format!(
                                                        "{}: {e}",
                                                        path.display()
                                                    )),
                                                }
                                                .note(&root, false);
                                            }
                                        }
                                    } else if !crate::graph::store_util::is_lockfile(&rel)
                                        && registry.for_file(&rel).is_some()
                                    {
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
                        match create_watcher_unless_cancelled(&root, debounce_ms, &token).await {
                            None => break,
                            Some(Ok((new_watcher, new_rx))) => {
                                watcher = new_watcher;
                                rx = new_rx;
                                eprintln!("[watch-producer] watcher restarted successfully");
                                // Whatever changed while it was down.
                                queue_true_up(&queue, "the watcher restarted");
                                dropped.queued(std::time::Instant::now());
                                on_event(WatchEvent {
                                    kind: WatchEventKind::WatcherRestarted,
                                    path: root.clone(),
                                    has_cross_file_calls: false,
                                });
                            }
                            Some(Err(e)) => {
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

/// Least time between two true-ups asked for by dropped-event reports: a
/// busy tree can report them in bursts, and one whole-project scan answers
/// them all.
const TRUE_UP_DEBOUNCE: Duration = Duration::from_secs(30);

/// Whole-project passes this process's producers have queued, for any
/// reason: a test measures that a storm of events does not turn into a storm
/// of scans (the log line says why each was queued).
static TRUE_UPS_QUEUED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// See [`TRUE_UPS_QUEUED`].
pub fn true_ups_queued() -> u64 {
    TRUE_UPS_QUEUED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Queues one whole-project pass -- the scan `infigraph index` does, which
/// hash-diffs every file and so costs a read each when nothing changed -- to
/// find what no event reported: an edit made while no watcher ran, or one
/// whose event the backend dropped. Logs when it was asked for and why; the
/// drain logs what the scan found.
fn queue_true_up(queue: &Arc<Mutex<IndexWorkQueue>>, why: &str) {
    TRUE_UPS_QUEUED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    queue.lock().unwrap().mark_whole_project();
    eprintln!(
        "[watch-producer] {} queued a code true-up ({why})",
        crate::watch::log_stamp()
    );
}

/// Whether `event` says events were lost rather than describing one: the
/// backend's own rescan request (FSEvents `MustScanSubDirs`, inotify queue
/// overflow) or a kind this producer does not model. Nothing is known about
/// which paths were lost, so only a whole-project pass can answer it.
fn events_were_dropped(event: &Event) -> bool {
    event.need_rescan() || matches!(event.kind, EventKind::Other)
}

/// Debounces dropped-event reports into at most one true-up per
/// [`TRUE_UP_DEBOUNCE`]; a report inside the window waits for its end.
#[derive(Default)]
struct DroppedEvents {
    pending: bool,
    last_true_up: Option<std::time::Instant>,
    /// Reports since the last true-up, of any kind.
    reports: u64,
    /// When each true-up this answered was queued, newest last, for the
    /// rate in the log line.
    answered: std::collections::VecDeque<std::time::Instant>,
}

/// How far back the logged rate of dropped-event true-ups looks.
const RATE_WINDOW: Duration = Duration::from_secs(600);

impl DroppedEvents {
    fn note(&mut self) {
        self.pending = true;
        self.reports += 1;
    }

    /// What to log with the true-up due at `now`: how many reports it
    /// answers and how many such true-ups the last [`RATE_WINDOW`] held,
    /// this one included -- the number to read off a busy tree.
    fn report(&mut self, now: std::time::Instant) -> String {
        self.answered.push_back(now);
        while self
            .answered
            .front()
            .is_some_and(|at| now.duration_since(*at) > RATE_WINDOW)
        {
            self.answered.pop_front();
        }
        format!(
            "{} report(s) since the last true-up, {} dropped-event true-up(s) in the last {} min",
            self.reports,
            self.answered.len(),
            RATE_WINDOW.as_secs() / 60
        )
    }

    /// A true-up was queued at `now` for any reason: it answers what was
    /// pending too.
    fn queued(&mut self, now: std::time::Instant) {
        self.pending = false;
        self.reports = 0;
        self.last_true_up = Some(now);
    }

    fn due(&self, now: std::time::Instant) -> bool {
        self.pending
            && self
                .last_true_up
                .is_none_or(|at| now.duration_since(at) >= TRUE_UP_DEBOUNCE)
    }
}

/// The file the producer writes to learn its own subscription still delivers
/// (under `.infigraph/`, which the walk never registers on inotify, so
/// `create_watcher` does).
pub(crate) const CANARY_FILE: &str = "watch-canary";

fn canary_stamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
        .to_string()
}

/// A watcher can go permanently deaf with nothing reporting it (sittir's did,
/// after a stretch of directory churn). Every `period` the producer writes
/// [`CANARY_FILE`]; if the event for it has not arrived `wait` later, the
/// subscription is rebuilt and the project is degraded until a probe is seen.
/// Pure state, so the decision is testable without a clock or a filesystem.
struct Canary {
    period: Duration,
    wait: Duration,
    last_probe: Option<std::time::Instant>,
    awaiting: Option<std::time::Instant>,
    misses: u32,
    recovered: bool,
}

#[derive(Debug, PartialEq)]
enum CanaryStep {
    Idle,
    Probe,
    Missed(u32),
}

impl Canary {
    /// `INFIGRAPH_TEST_WATCH_CANARY_MS=<period>,<wait>` shortens both for a
    /// test; nothing else reads it.
    fn new(now: std::time::Instant) -> Self {
        let (period, wait) = std::env::var("INFIGRAPH_TEST_WATCH_CANARY_MS")
            .ok()
            .and_then(|v| {
                let (p, w) = v.split_once(',')?;
                Some((p.parse().ok()?, w.parse().ok()?))
            })
            .map_or(
                (Duration::from_secs(20), Duration::from_secs(10)),
                |(p, w)| (Duration::from_millis(p), Duration::from_millis(w)),
            );
        Canary {
            period,
            wait,
            // The subscription was just made and the true-up covers startup:
            // the first probe is a period away.
            last_probe: Some(now),
            awaiting: None,
            misses: 0,
            recovered: false,
        }
    }

    fn step(&mut self, now: std::time::Instant) -> CanaryStep {
        if let Some(at) = self.awaiting {
            if now.duration_since(at) < self.wait {
                return CanaryStep::Idle;
            }
            self.awaiting = None;
            self.misses += 1;
            self.last_probe = Some(now);
            return CanaryStep::Missed(self.misses);
        }
        if self
            .last_probe
            .is_none_or(|at| now.duration_since(at) >= self.period)
        {
            return CanaryStep::Probe;
        }
        CanaryStep::Idle
    }

    fn probed(&mut self, now: std::time::Instant) {
        self.last_probe = Some(now);
        self.awaiting = Some(now);
    }

    /// The probe could not be written: nothing is awaited, try again later.
    fn skipped(&mut self, now: std::time::Instant) {
        self.last_probe = Some(now);
    }

    fn seen(&mut self) {
        self.awaiting = None;
        if self.misses > 0 {
            self.misses = 0;
            self.recovered = true;
        }
    }

    fn take_recovered(&mut self) -> bool {
        std::mem::take(&mut self.recovered)
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

/// [`create_watcher`] on a thread of its own (see [`Registration`]); `None`
/// means the token was cancelled first.
async fn create_watcher_unless_cancelled(
    root: &Path,
    debounce_ms: u64,
    token: &CancellationToken,
) -> Option<
    anyhow::Result<(
        SharedWatcher,
        tokio_mpsc::UnboundedReceiver<notify::Result<Event>>,
    )>,
> {
    let thread_root = root.to_path_buf();
    Registration::start(root, move || create_watcher(&thread_root, debounce_ms))
        .unless_cancelled(token)
        .await
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
    SharedWatcher,
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
    crate::watch::register_watch_dirs(&mut watcher, root)?.note(root, true);
    // The walk skips hidden directories, so inotify never sees the canary
    // there; one more subscription, and best effort (a project without
    // `.infigraph/` has no canary to see).
    if crate::watch::WATCHES_EACH_DIRECTORY {
        let _ = watcher.watch(
            &root.join(".infigraph"),
            notify::RecursiveMode::NonRecursive,
        );
    }
    Ok((Arc::new(Mutex::new(watcher)), rx))
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

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, Flag};

    #[test]
    fn a_rescan_request_or_an_unmodelled_kind_means_events_were_dropped() {
        let rescan =
            Event::new(EventKind::Modify(notify::event::ModifyKind::Any)).set_flag(Flag::Rescan);
        assert!(events_were_dropped(&rescan));
        assert!(events_were_dropped(&Event::new(EventKind::Other)));
        assert!(!events_were_dropped(&Event::new(EventKind::Create(
            CreateKind::File
        ))));
    }

    #[test]
    fn dropped_event_reports_collapse_into_one_true_up_per_window() {
        let t0 = std::time::Instant::now();
        let mut gate = DroppedEvents::default();
        assert!(!gate.due(t0), "nothing reported, nothing due");

        gate.note();
        assert!(gate.due(t0), "the first report is due at once");
        gate.queued(t0);
        assert!(!gate.due(t0), "queuing answers what was pending");

        gate.note();
        gate.note();
        assert!(!gate.due(t0 + Duration::from_secs(1)), "inside the window");
        assert!(gate.due(t0 + TRUE_UP_DEBOUNCE), "waits only for its end");
    }
    /// 20 directories of 50 subdirectories each, with a source file in every
    /// one: about the directory count of a large TypeScript monorepo.
    #[test]
    fn the_log_line_for_a_dropped_event_true_up_carries_its_rate() {
        let t0 = std::time::Instant::now();
        let mut gate = DroppedEvents::default();
        gate.note();
        gate.note();
        gate.note();
        assert_eq!(
            gate.report(t0),
            "3 report(s) since the last true-up, 1 dropped-event true-up(s) in the last 10 min"
        );
        gate.queued(t0);
        gate.note();
        let line = gate.report(t0 + TRUE_UP_DEBOUNCE);
        assert!(
            line.starts_with("1 report(s)") && line.contains(" 2 dropped-event"),
            "{line}"
        );
        // The first falls out of the window.
        gate.queued(t0 + TRUE_UP_DEBOUNCE);
        gate.note();
        let line = gate.report(t0 + RATE_WINDOW + Duration::from_secs(1));
        assert!(line.contains(" 2 dropped-event"), "{line}");
    }

    #[test]
    fn a_probe_that_is_never_seen_is_a_miss_and_one_that_is_seen_recovers() {
        let t0 = std::time::Instant::now();
        let mut canary = Canary::new(t0);
        let (period, wait) = (canary.period, canary.wait);
        assert_eq!(
            canary.step(t0 + period / 2),
            CanaryStep::Idle,
            "not yet due"
        );
        assert_eq!(canary.step(t0 + period), CanaryStep::Probe);
        canary.probed(t0 + period);
        assert_eq!(canary.step(t0 + period + wait / 2), CanaryStep::Idle);
        assert_eq!(canary.step(t0 + period + wait), CanaryStep::Missed(1));
        assert_eq!(
            canary.step(t0 + period + wait + Duration::from_secs(1)),
            CanaryStep::Idle,
            "the next probe is a period after the miss"
        );
        let t1 = t0 + period + wait + period;
        assert_eq!(canary.step(t1), CanaryStep::Probe);
        canary.probed(t1);
        assert_eq!(canary.step(t1 + wait), CanaryStep::Missed(2));

        let t2 = t1 + wait + period;
        canary.probed(t2);
        canary.seen();
        assert_eq!(canary.step(t2 + wait), CanaryStep::Idle, "seen in time");
        assert!(canary.take_recovered());
        assert!(!canary.take_recovered(), "reported once");
    }

    fn thousand_directories() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for top in 0..20 {
            for sub in 0..50 {
                let d = dir.path().join(format!("pkg{top}/mod{sub}"));
                std::fs::create_dir_all(&d).unwrap();
                std::fs::write(d.join("a.ts"), "export const a = 1;\n").unwrap();
            }
        }
        dir
    }

    /// Registering a watcher is time the watcher is deaf: it was 27s for
    /// these 1,021 directories on macOS, one FSEvents stream rebuild per
    /// directory. One recursive subscription is not a function of the count.
    #[cfg(target_os = "macos")]
    #[test]
    fn registering_a_thousand_directories_takes_one_subscription_not_a_thousand() {
        let dir = thousand_directories();
        let root = dir.path().canonicalize().unwrap();
        let started = std::time::Instant::now();
        let _watcher = create_watcher(&root, 100).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "registration took {:?}",
            started.elapsed()
        );
    }

    /// Registering a watcher is time the watcher is deaf. Run by hand for the
    /// number: `cargo test -p infigraph-core --lib registration_time -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn registration_time_on_a_thousand_directories() {
        let dir = thousand_directories();
        let root = dir.path().canonicalize().unwrap();
        let started = std::time::Instant::now();
        let _watcher = create_watcher(&root, 100).unwrap();
        eprintln!(
            "registered a watcher over 1021 directories in {:.2}s",
            started.elapsed().as_secs_f64()
        );
    }
}
