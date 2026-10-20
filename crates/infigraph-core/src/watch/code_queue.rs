//! The code graph as a [`PathQueue`]: how the producer's settled changes get
//! in line for the daemon's drain, which is the execute side
//! ([`PathIndexer for Infigraph`](crate::daemon::drain)).
//!
//! The producer never opens the graph. It makes a change durable (a dirty mark
//! that survives a crash), then hands the settled set to the shared
//! [`IndexWorkQueue`], and the daemon's drain indexes exactly those files.

use crate::daemon::queue::IndexWorkQueue;
use crate::pending_paths::PendingPaths;
use crate::watch::queued::PathQueue;
use crate::watch::sink::PathChange;
use crate::watch::{WatchEvent, WatchEventKind};
use crate::Infigraph;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The producer's event callback, shared with what reports through it (a
/// removal is an event of its own).
pub type EventSink = Arc<Mutex<Box<dyn Fn(WatchEvent) + Send>>>;

pub fn emit(on_event: &EventSink, event: WatchEvent) {
    (on_event
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))(event);
}

/// What enqueuing a code change needs.
pub struct CodeQueue {
    root: PathBuf,
    infigraph_dir: PathBuf,
    /// Gates which Created/Modified paths get marked dirty and queued. A path
    /// with no language pack behind it never produces a `FileExtraction`, so
    /// marking it dirty leaves it dirty forever: `clear_dirty` only clears
    /// what a drain's `outcome.extractions` reported. Without this filter
    /// every README/image touched under `root` leaks into a permanently
    /// growing dirty set.
    ///
    /// It does **not** cover dependency lockfiles, and used to claim it did:
    /// `pnpm-lock.yaml` and `package-lock.json` are claimed by the bundled
    /// YAML and JSON packs, so they pass this gate and were indexed in full
    /// (3,498 symbols from one lockfile on sittir). `store_util::is_lockfile`,
    /// checked beside this, is what stops those.
    registry: Arc<crate::lang::LanguageRegistry>,
    queue: Arc<Mutex<IndexWorkQueue>>,
    /// R7.4 (#84): above this many files in one debounce window, the batch
    /// coalesces into a single whole-project pass instead of per-file
    /// updates.
    storm_threshold: usize,
    on_event: EventSink,
}

impl CodeQueue {
    pub fn new(
        root: &Path,
        registry: Arc<crate::lang::LanguageRegistry>,
        queue: Arc<Mutex<IndexWorkQueue>>,
        storm_threshold: usize,
        on_event: EventSink,
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            infigraph_dir: root.join(".infigraph"),
            registry,
            queue,
            storm_threshold,
            on_event,
        }
    }
}

impl PathQueue for Infigraph {
    type Handle = CodeQueue;

    fn wants(handle: &CodeQueue, rel: &str) -> bool {
        !crate::graph::store_util::is_lockfile(rel) && handle.registry.for_file(rel).is_some()
    }

    fn note_pending(handle: &CodeQueue, changes: &[(PathBuf, PathChange)]) {
        // R3.3.5: persisted before the change enters the debounce window --
        // the in-memory set a crash could still lose -- so a crash between
        // here and the drain leaves every path recoverable at next startup.
        let rels: Vec<String> = changes
            .iter()
            .map(|(path, _)| path.to_string_lossy().replace('\\', "/"))
            .collect();
        if let Err(e) = crate::dirty::mark_dirty(&handle.infigraph_dir, &rels) {
            eprintln!("[watch-producer] failed to persist dirty marks for {rels:?}: {e}");
        }
        for (path, change) in changes {
            if *change == PathChange::Removed {
                emit(
                    &handle.on_event,
                    WatchEvent {
                        kind: WatchEventKind::Removed,
                        path: handle.root.join(path),
                        has_cross_file_calls: false,
                    },
                );
            }
        }
    }

    fn enqueue(handle: &CodeQueue, pending: PendingPaths) {
        let crate::pending_paths::Taken {
            upserts,
            removals,
            whole,
            ..
        } = pending.take();
        let mut q = handle.queue.lock().unwrap();
        // `add_watch_removal`, not `add_removal`, because these are real
        // filesystem removals: the path is already gone from disk, so there
        // is no way to tell here whether it named a file or a directory -- the
        // drain scans for and removes anything nested under it too.
        for rel in removals {
            q.add_watch_removal(rel);
        }
        let paths = upserts
            .into_keys()
            .map(|rel| handle.root.join(rel))
            .collect();
        crate::watch::flush_batch_into_queue(&mut q, paths, &handle.root, handle.storm_threshold);
        if whole {
            q.mark_whole_project();
            eprintln!(
                "[watch-producer] {} queued a code true-up (events were lost or a directory \
                 arrived holding more files than a list can carry)",
                crate::watch::log_stamp()
            );
        }
    }
}
