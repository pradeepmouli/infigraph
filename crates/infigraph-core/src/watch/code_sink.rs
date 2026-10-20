//! The code graph's sink: the producer's per-file arms, moved here as they
//! were. A change becomes a persisted dirty mark and an entry in the debounce
//! window; the window flushes into the shared [`IndexWorkQueue`], which the
//! daemon's drain turns into an index of exactly those files.

use crate::daemon::queue::IndexWorkQueue;
use crate::ignore_rules::Domain;
use crate::watch::batch::ChangeBatch;
use crate::watch::sink::{PathChange, SinkHealth, WatchBatch, WatchSink};
use crate::watch::{WatchEvent, WatchEventKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The producer's event callback, shared with the sinks that report through
/// it (a removal is an event of its own).
pub type EventSink = Arc<Mutex<Box<dyn Fn(WatchEvent) + Send>>>;

pub fn emit(on_event: &EventSink, event: WatchEvent) {
    (on_event
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))(event);
}

pub struct CodeSink {
    root: PathBuf,
    infigraph_dir: PathBuf,
    registry: Arc<crate::lang::LanguageRegistry>,
    queue: Arc<Mutex<IndexWorkQueue>>,
    /// Collects file changes over a 1-second window so the consumer indexes
    /// them all at once via the bulk write path.
    batch: ChangeBatch,
    /// R7.4 (#84): above this many files in one debounce window, the batch
    /// coalesces into a single whole-project pass instead of per-file
    /// updates.
    storm_threshold: usize,
    on_event: EventSink,
    health: SinkHealth,
}

impl CodeSink {
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
            batch: ChangeBatch::new(1000),
            storm_threshold,
            on_event,
            health: SinkHealth::default(),
        }
    }
}

impl WatchSink for CodeSink {
    fn name(&self) -> &'static str {
        "code"
    }

    fn domain(&self) -> Domain {
        Domain::Code
    }

    fn accept(&mut self, batch: WatchBatch) {
        self.health.accepted += 1;
        if batch.rescan {
            crate::watch::producer::queue_true_up(
                &self.queue,
                "a directory arrived holding more files than a list can carry",
            );
        }
        for (rel_path, change) in batch.changes {
            let rel = rel_path.to_string_lossy().replace('\\', "/");
            let path = self.root.join(&rel);
            match change {
                PathChange::Removed => {
                    // R3.3.5: persisted before the in-memory
                    // queue add below, so a crash between the
                    // two still leaves this path recoverable
                    // at next startup.
                    if let Err(e) =
                        crate::dirty::mark_dirty(&self.infigraph_dir, std::slice::from_ref(&rel))
                    {
                        eprintln!("[watch-producer] failed to persist dirty mark for {rel}: {e}");
                    }
                    // `add_watch_removal` (not `add_removal`)
                    // because this is a real filesystem
                    // removal: `path` is already gone from
                    // disk, so there is no way to tell here
                    // whether it named a file or a directory
                    // -- the drain scans for and removes
                    // anything nested under it too.
                    self.queue.lock().unwrap().add_watch_removal(rel);
                    emit(
                        &self.on_event,
                        WatchEvent {
                            kind: WatchEventKind::Removed,
                            path,
                            has_cross_file_calls: false,
                        },
                    );
                }
                PathChange::Created | PathChange::Modified => {
                    if !crate::graph::store_util::is_lockfile(&rel)
                        && self.registry.for_file(&rel).is_some()
                    {
                        // R3.3.5: persisted before this event
                        // enters `batch` -- the in-memory
                        // accumulator a crash could still lose
                        // between now and the flush below.
                        if let Err(e) = crate::dirty::mark_dirty(
                            &self.infigraph_dir,
                            std::slice::from_ref(&rel),
                        ) {
                            eprintln!(
                                "[watch-producer] failed to persist dirty mark for {rel}: {e}"
                            );
                        }
                        self.batch.add(path);
                    }
                }
            }
        }
    }

    fn health(&self) -> SinkHealth {
        self.health
    }

    /// Runs on a timer rather than off the back of an event: the window only
    /// elapses *after* the last event, so an event-triggered check would
    /// leave a batch stranded whenever changes stop arriving -- which is the
    /// common case (someone saves a file and pauses).
    fn tick(&mut self) {
        if self.batch.is_empty() || !self.batch.is_ready() {
            return;
        }
        let paths = self.batch.drain();
        let mut q = self.queue.lock().unwrap();
        crate::watch::flush_batch_into_queue(&mut q, paths, &self.root, self.storm_threshold);
    }
}
