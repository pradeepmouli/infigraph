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
    /// Ask for a whole-project pass this often even when no event was lost,
    /// because events are never taken as complete. `None` leaves it to the
    /// loss signals.
    pub true_up_every: Option<Duration>,
    /// After a hand-off that took `t` to be answered, wait `t * run_gap_factor`
    /// (at most `run_gap_cap`) before the next. A hand-off the indexer answers
    /// when its run is done counts as work in flight for the daemon's idle
    /// exit; a trigger that never stops (a formatter rewriting a document with
    /// the same bytes) would otherwise keep a daemon busy nearly all the time.
    /// Zero turns it off.
    pub run_gap_factor: u32,
    pub run_gap_cap: Duration,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(1000),
            whole_gap: Duration::from_secs(30),
            channel_cap: 1024,
            tick: Duration::from_millis(200),
            true_up_every: None,
            run_gap_factor: 0,
            run_gap_cap: Duration::ZERO,
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
    let started = Instant::now();
    let mut window: PendingPaths = PendingPaths::new();
    let mut last_change: Option<Instant> = None;
    let mut last_whole: Option<Instant> = None;
    let mut next_run_at = Instant::now();
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
        if let Some(every) = pacing.true_up_every {
            if last_whole.unwrap_or(started).elapsed() >= every && !window.wants_whole() {
                window.mark_whole();
                last_change = Some(Instant::now());
            }
        }
        let settled = last_change.is_some_and(|at| at.elapsed() >= pacing.debounce);
        let whole_allowed =
            last_whole.is_none_or(|at| at.elapsed() >= pacing.whole_gap) || !window.wants_whole();
        if settled && !window.is_empty() && whole_allowed && Instant::now() >= next_run_at {
            if window.wants_whole() {
                last_whole = Some(Instant::now());
            }
            let handed_on = Instant::now();
            I::enqueue(&queued.handle, std::mem::take(&mut window));
            last_change = None;
            next_run_at = Instant::now()
                + (handed_on.elapsed() * pacing.run_gap_factor).min(pacing.run_gap_cap);
        }
    }
    // A sink that is dropped (detached, or its producer stopped) discards what
    // it still holds: it is no longer listening, so the changes it never
    // handed on are for the next sink to find, which it does with the rescan
    // it is attached with.
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
    /// Closing the channel is what ends the worker, which discards what it
    /// still holds and exits on its own. It is deliberately not joined: its
    /// hand-off may be waiting on the very thread that is dropping the sink
    /// (the daemon's coordinator stops the producer, and the producer's sinks
    /// go with it, while a document run is waiting for that coordinator).
    fn drop(&mut self) {
        self.tx = None;
        self.worker = None;
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
        /// How long every `enqueue` takes to be answered.
        pub delay: Option<Duration>,
        /// When each `enqueue` began.
        pub began: Vec<Instant>,
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
            let (stall, delay) = {
                let mut record = handle.lock().unwrap();
                record.began.push(Instant::now());
                (record.stall.clone(), record.delay)
            };
            if let Some(delay) = delay {
                std::thread::sleep(delay);
            }
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
            true_up_every: None,
            run_gap_factor: 0,
            run_gap_cap: Duration::ZERO,
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
    fn dropping_a_sink_discards_what_it_still_holds_and_does_not_wait_for_its_worker() {
        let (mut sink, handle) = sink(Pacing {
            debounce: Duration::from_secs(60),
            ..quick()
        });
        sink.accept(batch(&[("late.md", Created)]));

        let started = Instant::now();
        drop(sink);
        std::thread::sleep(Duration::from_millis(100));

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            handle.lock().unwrap().enqueued.is_empty(),
            "a detached sink indexes nothing more"
        );
    }

    #[test]
    fn a_sink_asks_for_a_whole_project_pass_on_its_own_when_told_to() {
        let (_sink, handle) = sink(Pacing {
            true_up_every: Some(Duration::from_millis(80)),
            ..quick()
        });

        assert!(wait_for(&handle, |r| r.enqueued.iter().any(|t| t.whole)));
    }

    #[test]
    fn a_hand_off_that_took_long_to_answer_is_followed_by_a_proportionate_wait() {
        let (mut sink, handle) = sink(Pacing {
            run_gap_factor: 4,
            run_gap_cap: Duration::from_secs(5),
            ..quick()
        });
        handle.lock().unwrap().delay = Some(Duration::from_millis(100));

        sink.accept(batch(&[("a.md", Created)]));
        assert!(wait_for(&handle, |r| r.began.len() == 1));
        sink.accept(batch(&[("b.md", Created)]));
        assert!(wait_for(&handle, |r| r.began.len() == 2));

        let began = handle.lock().unwrap().began.clone();
        assert!(
            began[1] - began[0] >= Duration::from_millis(480),
            "100ms to answer, four times that to wait, then 100ms more: {:?}",
            began[1] - began[0]
        );
    }

    #[test]
    fn the_wait_is_capped() {
        let (mut sink, handle) = sink(Pacing {
            run_gap_factor: 1000,
            run_gap_cap: Duration::from_millis(50),
            ..quick()
        });
        handle.lock().unwrap().delay = Some(Duration::from_millis(100));

        sink.accept(batch(&[("a.md", Created)]));
        assert!(wait_for(&handle, |r| r.began.len() == 1));
        sink.accept(batch(&[("b.md", Created)]));
        assert!(wait_for(&handle, |r| r.began.len() == 2));

        let began = handle.lock().unwrap().began.clone();
        assert!(began[1] - began[0] < Duration::from_secs(1));
    }
}
