pub mod batch;
pub mod config;
pub mod producer;

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio_util::sync::CancellationToken;

use crate::daemon::queue::IndexWorkQueue;
use crate::daemon::run_write_coordinator;
use crate::daemon::task::Task;
use crate::Infigraph;

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
/// The coordinator's handle on the code-watch producer `Task<()>`: enough
/// state to stop the live one, and enough to spawn a replacement when a
/// `WatchControl { role: Code, action: Start|Restart }` request asks for
/// one. Stopping cancels the *task's* own child token, leaving `token`
/// (the `code_token` of the spec's hierarchy) intact and reusable -- which
/// is what makes stop-then-start work without rebuilding the hierarchy.
///
/// Constructed by the coordinator rather than its caller because the
/// producer needs the same `LanguageRegistry` the coordinator already built
/// once for the whole session (#58): handing that construction to the
/// caller would mean building the 62-pack registry twice per daemon start.
pub(crate) struct CodeWatch {
    /// Dedicated small runtime for producer tasks, deliberately separate
    /// from `drain_rt` (blocking indexing work only) so a stalled producer
    /// can't stall drain dispatch/reaping, or vice versa.
    rt: tokio::runtime::Runtime,
    token: CancellationToken,
    task: Option<Task<()>>,
    config: producer::ProducerConfig,
    queue: Arc<Mutex<IndexWorkQueue>>,
    on_event: Arc<dyn Fn(WatchEvent) + Send + Sync>,
}

impl CodeWatch {
    pub(crate) fn new(
        daemon_token: &CancellationToken,
        config: producer::ProducerConfig,
        queue: Arc<Mutex<IndexWorkQueue>>,
        on_event: Arc<dyn Fn(WatchEvent) + Send + Sync>,
    ) -> Result<Self> {
        Ok(CodeWatch {
            rt: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("infigraph-watch")
                .enable_all()
                .build()?,
            token: daemon_token.child_token(),
            task: None,
            config,
            queue,
            on_event,
        })
    }

    pub(crate) fn start(&mut self) {
        if self.task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        // A producer can self-terminate without cancellation (initial
        // watcher-creation failure, restart budget exhausted, restart
        // failure) -- drop a finished task before respawning, or Start
        // would silently no-op forever after any of those.
        self.task.take();
        let config = self.config.clone();
        let queue = Arc::clone(&self.queue);
        let on_event = Arc::clone(&self.on_event);
        // `Task::spawn` dispatches via the ambient `tokio::task::spawn`,
        // which needs a runtime context on this (plain OS) thread.
        let _guard = self.rt.enter();
        self.task = Some(Task::spawn(&self.token, "code-watch", move |token| {
            producer::run_producer(config, queue, move |evt| on_event(evt), token)
        }));
    }

    pub(crate) fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            // Blocking here is bounded: a producer's `select!` loop has no
            // long synchronous work to finish, so it observes the
            // cancellation on its next poll.
            self.rt.block_on(task.stop());
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
    on_event: impl Fn(WatchEvent) + Send + Sync + 'static,
) -> Result<()>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry> + Send + 'static,
{
    // Phase 3 replaces this with a real token hierarchy passed down from the
    // daemon's own lifetime -- for now, a fresh, unparented token keeps this
    // thin wrapper's existing callers unaffected by the signature change.
    let token = CancellationToken::new();
    run_write_coordinator(
        root,
        make_registry,
        debounce_ms,
        stop_rx,
        on_event,
        0,
        None::<fn(&crate::IndexResult)>,
        false,
        None,
        &token,
        None,
    )
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
/// Feeds one drained debounce batch into the shared queue (R7.4, #84).
/// A reindex storm (mass git checkout, branch switch) floods the batch
/// with hundreds of per-file events; above `storm_threshold`, one
/// whole-project pass beats N per-file extractions -- the drain's
/// scan_changed_files hash-diffs the tree, so unchanged files cost a hash
/// each while every actually-changed file is still picked up, including
/// any the storm's event flood dropped or that arrived after the window
/// closed, and the scan's stale-file sweep prunes removals too.
pub(crate) fn flush_batch_into_queue(
    q: &mut crate::daemon::queue::IndexWorkQueue,
    paths: Vec<PathBuf>,
    root: &Path,
    storm_threshold: usize,
) {
    if paths.len() > storm_threshold {
        eprintln!(
            "[watch] {} files changed in one debounce window (> {}) -- \
             coalescing into a single whole-project pass",
            paths.len(),
            storm_threshold
        );
        q.mark_whole_project();
        return;
    }
    for path in paths {
        let rel = path
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"));
        q.add_raw(rel);
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
pub(crate) fn register_watch_dirs(watcher: &mut RecommendedWatcher, root: &Path) -> Result<()> {
    for result in crate::ignore_rules::walk_builder(root).build() {
        let Ok(entry) = result else { continue };
        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            let _ = watcher.watch(entry.path(), RecursiveMode::NonRecursive);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Regression test for a macOS-specific bug: `run_write_coordinator`
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

        // Give the watcher time to register before triggering a change. Under
        // heavy machine load the notify backend can take noticeably longer to
        // arm; a too-short wait means the remove fires before the watch is live
        // and the event is missed entirely, so keep this generous.
        std::thread::sleep(Duration::from_millis(1000));
        std::fs::remove_file(&file_path).unwrap();

        // Poll rather than a single fixed sleep: fast on a quiet machine,
        // robust on a loaded one. ~10s ceiling absorbs fs-notify debounce + load.
        let mut seen = false;
        for _ in 0..100 {
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
             run_write_coordinator may have regressed"
        );
    }
}

#[cfg(test)]
mod storm_coalescing {
    use crate::daemon::queue::IndexWorkQueue;
    use crate::watch::flush_batch_into_queue;
    use std::path::{Path, PathBuf};

    fn paths(root: &Path, n: usize) -> Vec<PathBuf> {
        (0..n).map(|i| root.join(format!("f{i}.py"))).collect()
    }

    #[test]
    fn under_threshold_batches_enqueue_per_file_raw_items() {
        let root = Path::new("/proj");
        let mut q = IndexWorkQueue::new();
        flush_batch_into_queue(&mut q, paths(root, 3), root, 200);
        let drained = q.drain();
        assert!(!drained.whole_project);
        assert_eq!(drained.items.len(), 3);
        assert!(drained.items.contains_key("f0.py"), "{:?}", drained.items);
    }

    #[test]
    fn storm_sized_batches_coalesce_into_one_whole_project_pass() {
        let root = Path::new("/proj");
        let mut q = IndexWorkQueue::new();
        flush_batch_into_queue(&mut q, paths(root, 201), root, 200);
        let drained = q.drain();
        assert!(drained.whole_project, "must fall back to one scan pass");
        assert!(
            drained.items.is_empty(),
            "no per-file items alongside the whole-project pass: {:?}",
            drained.items.len()
        );
    }

    #[test]
    fn threshold_is_exclusive_a_batch_exactly_at_it_stays_per_file() {
        let root = Path::new("/proj");
        let mut q = IndexWorkQueue::new();
        flush_batch_into_queue(&mut q, paths(root, 200), root, 200);
        let drained = q.drain();
        assert!(!drained.whole_project);
        assert_eq!(drained.items.len(), 200);
    }
}
