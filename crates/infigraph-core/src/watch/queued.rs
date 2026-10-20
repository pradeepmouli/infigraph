//! The one sink: it takes triaged changes off the producer, holds them for a
//! debounce window, and hands the settled set to an indexer's queue. What
//! differs between the code graph and the document store is the indexer, not
//! the sink, so the sink is generic over [`PathQueue`].
//!
//! The producer thread only ever calls [`WatchSink::accept`], which makes the
//! changes durable ([`PathQueue::note_pending`]) and offers them to a bounded
//! channel without waiting. A worker thread owns everything slow: the debounce,
//! the hand-off to the queue (which for documents is a request to the daemon's
//! coordinator, answered when the index run is done) and the wait for its
//! turn. A sink that cannot keep up loses batches, not the producer's time:
//! it counts them and answers the loss with a whole-project pass.

use crate::ignore_rules::Domain;
use crate::pending_paths::PendingPaths;
use crate::watch::sink::{PathChange, PathIndexer, SinkHealth, WatchBatch, WatchSink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// The enqueue side of an indexer: how a settled set of paths gets in line
/// for the execute side ([`PathIndexer`]). `Handle` is whatever the enqueuing
/// needs and the indexer itself, which a daemon opens elsewhere, does not
/// provide: the code graph's work queue, the document store's coordinator port.
pub trait PathQueue: PathIndexer + 'static {
    type Handle: Send + Sync + 'static;

    /// Whether a created or modified `rel` is the indexer's business at all
    /// (a file no language pack claims, a lockfile, a non-document).
    fn wants(_handle: &Self::Handle, _rel: &str) -> bool {
        true
    }

    /// Called on the producer thread as changes are accepted, before any
    /// debounce, so an indexer can make them durable first: a crash between
    /// accepting a path and indexing it must still leave it recoverable (the
    /// code side's dirty marks).
    fn note_pending(_handle: &Self::Handle, _changes: &[(PathBuf, PathChange)]) {}

    /// Hands the settled set to the indexer's queue.
    fn enqueue(handle: &Self::Handle, pending: PendingPaths);
}

/// How a sink paces itself.
#[derive(Debug, Clone, Copy)]
pub struct Pacing {
    /// Quiet time after the last change before the set is handed on.
    pub debounce: Duration,
    /// Least time between two whole-project passes.
    pub whole_gap: Duration,
    /// Batches the channel holds before the sink starts dropping.
    pub channel_cap: usize,
    /// How often the worker looks at the clock when nothing arrives.
    pub tick: Duration,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(1000),
            whole_gap: Duration::from_secs(30),
            channel_cap: 1024,
            tick: Duration::from_millis(200),
        }
    }
}

/// The adapter between a sink and an indexer's queue: the only thing an
/// [`IndexingSink`] is given.
pub struct Queued<I: PathQueue> {
    handle: I::Handle,
}

impl<I: PathQueue> Queued<I> {
    pub fn new(handle: I::Handle) -> Self {
        Self { handle }
    }
}

#[derive(Default)]
struct Counters {
    accepted: AtomicU64,
    dropped: AtomicU64,
}

pub struct IndexingSink<I: PathQueue> {
    name: &'static str,
    domain: Domain,
    queued: Arc<Queued<I>>,
    tx: Option<mpsc::SyncSender<WatchBatch>>,
    /// Set when a batch was dropped; the worker answers it with a
    /// whole-project pass.
    lost: Arc<AtomicBool>,
    counters: Arc<Counters>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl<I: PathQueue> IndexingSink<I> {
    pub fn new(name: &'static str, domain: Domain, queued: Queued<I>, pacing: Pacing) -> Self {
        let queued = Arc::new(queued);
        let (tx, rx) = mpsc::sync_channel(pacing.channel_cap.max(1));
        let lost = Arc::new(AtomicBool::new(false));
        let worker = {
            let queued = Arc::clone(&queued);
            let lost = Arc::clone(&lost);
            std::thread::Builder::new()
                .name(format!("watch-sink-{name}"))
                .spawn(move || work::<I>(&queued, &rx, &lost, pacing))
                .expect("spawn a watch sink worker")
        };
        Self {
            name,
            domain,
            queued,
            tx: Some(tx),
            lost,
            counters: Arc::new(Counters::default()),
            worker: Some(worker),
        }
    }
}

fn work<I: PathQueue>(
    queued: &Queued<I>,
    rx: &mpsc::Receiver<WatchBatch>,
    lost: &AtomicBool,
    pacing: Pacing,
) {
    let mut window: PendingPaths = PendingPaths::new();
    let mut last_change: Option<Instant> = None;
    let mut last_whole: Option<Instant> = None;
    loop {
        match rx.recv_timeout(pacing.tick) {
            Ok(batch) => {
                absorb::<I>(&queued.handle, &mut window, batch);
                last_change = Some(Instant::now());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if lost.swap(false, Ordering::SeqCst) {
            window.mark_whole();
            last_change = Some(Instant::now());
        }
        let settled = last_change.is_some_and(|at| at.elapsed() >= pacing.debounce);
        let whole_allowed =
            last_whole.is_none_or(|at| at.elapsed() >= pacing.whole_gap) || !window.wants_whole();
        if settled && !window.is_empty() && whole_allowed {
            if window.wants_whole() {
                last_whole = Some(Instant::now());
            }
            I::enqueue(&queued.handle, std::mem::take(&mut window));
            last_change = None;
        }
    }
    // A sink that is dropped hands on what it still holds.
    if !window.is_empty() {
        I::enqueue(&queued.handle, window);
    }
}

fn absorb<I: PathQueue>(handle: &I::Handle, window: &mut PendingPaths, batch: WatchBatch) {
    for (path, change) in batch.changes {
        let rel = path.to_string_lossy().replace('\\', "/");
        match change {
            PathChange::Removed => window.remove_tree(rel),
            PathChange::Created | PathChange::Modified => {
                if I::wants(handle, &rel) {
                    window.upsert(rel, ());
                }
            }
        }
    }
    if batch.rescan {
        window.mark_whole();
    }
}

impl<I: PathQueue> WatchSink for IndexingSink<I> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn domain(&self) -> Domain {
        self.domain
    }

    fn accept(&mut self, batch: WatchBatch) {
        self.counters.accepted.fetch_add(1, Ordering::Relaxed);
        let wanted: Vec<(PathBuf, PathChange)> = batch
            .changes
            .iter()
            .filter(|(path, change)| {
                *change == PathChange::Removed
                    || I::wants(&self.queued.handle, &path.to_string_lossy())
            })
            .cloned()
            .collect();
        if !wanted.is_empty() {
            I::note_pending(&self.queued.handle, &wanted);
        }
        let Some(tx) = &self.tx else { return };
        if tx.try_send(batch).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            self.lost.store(true, Ordering::SeqCst);
        }
    }

    fn health(&self) -> SinkHealth {
        SinkHealth {
            accepted: self.counters.accepted.load(Ordering::Relaxed),
            dropped: self.counters.dropped.load(Ordering::Relaxed),
        }
    }
}

impl<I: PathQueue> Drop for IndexingSink<I> {
    fn drop(&mut self) {
        // Closing the channel is what ends the worker.
        self.tx = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
pub(crate) mod fakes {
    //! The fake indexer every test of the sink machinery drives: it records
    //! what was made durable and what was enqueued, and does nothing else.
    use super::*;
    use crate::watch::sink::PathOutcome;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct Record {
        pub noted: Vec<(PathBuf, PathChange)>,
        pub enqueued: Vec<crate::pending_paths::Taken>,
        /// Paths `wants` turns away.
        pub unwanted: Vec<String>,
        /// While set, `enqueue` blocks, as an indexer whose queue is busy does.
        pub stall: Option<Arc<AtomicBool>>,
    }

    pub struct Fake;

    pub type FakeHandle = Arc<Mutex<Record>>;

    impl PathIndexer for Fake {
        fn index_paths(&self, _: &[PathBuf]) -> anyhow::Result<PathOutcome> {
            Ok(PathOutcome::default())
        }
        fn delete_paths(&self, _: &[PathBuf]) -> anyhow::Result<PathOutcome> {
            Ok(PathOutcome::default())
        }
        fn index_all(&self) -> anyhow::Result<PathOutcome> {
            Ok(PathOutcome::default())
        }
    }

    impl PathQueue for Fake {
        type Handle = FakeHandle;

        fn wants(handle: &Self::Handle, rel: &str) -> bool {
            !handle.lock().unwrap().unwanted.iter().any(|u| u == rel)
        }

        fn note_pending(handle: &Self::Handle, changes: &[(PathBuf, PathChange)]) {
            handle.lock().unwrap().noted.extend_from_slice(changes);
        }

        fn enqueue(handle: &Self::Handle, pending: PendingPaths) {
            let stall = handle.lock().unwrap().stall.clone();
            while stall.as_ref().is_some_and(|s| s.load(Ordering::SeqCst)) {
                std::thread::sleep(Duration::from_millis(5));
            }
            handle.lock().unwrap().enqueued.push(pending.take());
        }
    }

    pub fn quick() -> Pacing {
        Pacing {
            debounce: Duration::from_millis(30),
            whole_gap: Duration::from_millis(200),
            channel_cap: 4,
            tick: Duration::from_millis(5),
        }
    }

    pub fn sink(pacing: Pacing) -> (IndexingSink<Fake>, FakeHandle) {
        let handle: FakeHandle = Arc::new(Mutex::new(Record::default()));
        let sink = IndexingSink::new(
            "fake",
            Domain::Docs,
            Queued::<Fake>::new(Arc::clone(&handle)),
            pacing,
        );
        (sink, handle)
    }

    pub fn batch(changes: &[(&str, PathChange)]) -> WatchBatch {
        WatchBatch {
            changes: changes
                .iter()
                .map(|(p, c)| (PathBuf::from(p), *c))
                .collect(),
            rescan: false,
        }
    }

    /// Waits up to two seconds for `done` to hold of the record.
    pub fn wait_for(handle: &FakeHandle, done: impl Fn(&Record) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if done(&handle.lock().unwrap()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;
    use crate::watch::sink::PathChange::{Created, Modified, Removed};

    #[test]
    fn changes_are_made_durable_on_accept_and_handed_on_once_settled() {
        let (mut sink, handle) = sink(quick());

        sink.accept(batch(&[("a.md", Created), ("b.md", Modified)]));

        // Durable before the debounce, on the caller's thread.
        assert_eq!(handle.lock().unwrap().noted.len(), 2);
        assert!(wait_for(&handle, |r| r.enqueued.len() == 1));
        let taken = handle.lock().unwrap().enqueued.remove(0);
        let mut upserts: Vec<_> = taken.upserts.into_keys().collect();
        upserts.sort();
        assert_eq!(upserts, ["a.md", "b.md"]);
    }

    #[test]
    fn a_burst_inside_the_window_is_one_hand_off_with_the_removal_winning() {
        let (mut sink, handle) = sink(quick());

        sink.accept(batch(&[("a.md", Created)]));
        sink.accept(batch(&[("a.md", Removed)]));
        sink.accept(batch(&[("dir", Removed)]));

        assert!(wait_for(&handle, |r| !r.enqueued.is_empty()));
        std::thread::sleep(Duration::from_millis(100));
        let record = handle.lock().unwrap();
        assert_eq!(record.enqueued.len(), 1, "one window, one hand-off");
        let taken = &record.enqueued[0];
        assert!(taken.upserts.is_empty());
        assert!(taken.removals.contains("a.md") && taken.removals.contains("dir"));
        assert!(taken.removal_prefixes.contains("dir"));
    }

    #[test]
    fn a_path_the_indexer_does_not_want_never_enters_the_window() {
        let (mut sink, handle) = sink(quick());
        handle.lock().unwrap().unwanted.push("skip.bin".into());

        sink.accept(batch(&[("skip.bin", Created), ("keep.md", Created)]));

        assert!(wait_for(&handle, |r| r.enqueued.len() == 1));
        let record = handle.lock().unwrap();
        assert_eq!(
            record.noted.len(),
            1,
            "nothing durable for an unwanted path"
        );
        assert!(record.enqueued[0].upserts.contains_key("keep.md"));
        assert!(!record.enqueued[0].upserts.contains_key("skip.bin"));
    }

    #[test]
    fn a_sink_that_never_drains_delays_nobody_and_its_overflow_becomes_one_rescan() {
        // The stalled sink: its worker is blocked inside the hand-off.
        let (mut stalled, stalled_handle) = sink(quick());
        let stall = Arc::new(AtomicBool::new(true));
        stalled_handle.lock().unwrap().stall = Some(Arc::clone(&stall));
        stalled.accept(batch(&[("first.md", Created)]));
        assert!(wait_for(&stalled_handle, |r| !r.noted.is_empty()));
        std::thread::sleep(Duration::from_millis(100)); // worker is now stuck in enqueue

        // A healthy sink beside it is not slowed down.
        let (mut healthy, healthy_handle) = sink(quick());
        let started = Instant::now();
        // Far more batches than the stalled sink's channel holds.
        for i in 0..50 {
            stalled.accept(batch(&[(&format!("n{i}.md"), Created)]));
        }
        healthy.accept(batch(&[("ok.md", Created)]));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "accept never waits for a sink"
        );
        assert!(wait_for(&healthy_handle, |r| r.enqueued.len() == 1));
        assert!(stalled.health().dropped > 0, "{:?}", stalled.health());

        // Let it go: the loss is answered by exactly one whole-project pass.
        stall.store(false, Ordering::SeqCst);
        assert!(wait_for(&stalled_handle, |r| r
            .enqueued
            .iter()
            .any(|t| t.whole)));
        std::thread::sleep(Duration::from_millis(300));
        let wholes = stalled_handle
            .lock()
            .unwrap()
            .enqueued
            .iter()
            .filter(|t| t.whole)
            .count();
        assert_eq!(wholes, 1);
    }

    #[test]
    fn a_rescan_batch_asks_for_a_whole_project_pass_and_whole_passes_are_spaced() {
        let (mut sink, handle) = sink(quick());

        sink.accept(WatchBatch::rescan());
        assert!(wait_for(&handle, |r| r.enqueued.len() == 1));
        let first = Instant::now();
        sink.accept(WatchBatch::rescan());
        assert!(wait_for(&handle, |r| r.enqueued.len() == 2));

        assert!(handle.lock().unwrap().enqueued.iter().all(|t| t.whole));
        assert!(
            first.elapsed() >= Duration::from_millis(150),
            "the second pass waited out the gap ({:?})",
            first.elapsed()
        );
    }

    #[test]
    fn dropping_a_sink_hands_on_what_it_still_holds_and_stops_its_worker() {
        let (mut sink, handle) = sink(Pacing {
            debounce: Duration::from_secs(60),
            ..quick()
        });
        sink.accept(batch(&[("late.md", Created)]));

        drop(sink);

        assert_eq!(handle.lock().unwrap().enqueued.len(), 1);
    }
}
