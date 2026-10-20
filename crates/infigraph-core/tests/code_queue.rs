//! The code graph's sink (`IndexingSink<Infigraph>`) on its own: what a batch of triaged changes does to the dirty set,
//! the debounce window and the shared queue. The language registry comes from
//! `infigraph_languages`, which depends on this crate, so these are integration
//! tests.

use infigraph_core::daemon::queue::IndexWorkQueue;
use infigraph_core::watch::code_queue::{CodeQueue, EventSink};
use infigraph_core::watch::queued::{IndexingSink, Pacing, Queued};
use infigraph_core::watch::sink::PathChange::{self, Created, Modified, Removed};
use infigraph_core::watch::sink::{WatchBatch, WatchSink};
use infigraph_core::watch::WatchEvent;
use infigraph_core::Infigraph;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type SinkHarness<I> = (
    IndexingSink<I>,
    Arc<Mutex<IndexWorkQueue>>,
    Arc<Mutex<Vec<String>>>,
);

fn sink(root: &Path) -> SinkHarness<Infigraph> {
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&events);
    let on_event: EventSink = Arc::new(Mutex::new(Box::new(move |e: WatchEvent| {
        seen.lock().unwrap().push(e.to_string());
    })));
    let sink = IndexingSink::<Infigraph>::new(
        "code",
        infigraph_core::ignore_rules::Domain::Code,
        Queued::new(CodeQueue::new(
            root,
            Arc::new(infigraph_languages::bundled_registry().unwrap()),
            Arc::clone(&queue),
            200,
            on_event,
        )),
        Pacing {
            debounce: std::time::Duration::from_millis(300),
            ..Pacing::default()
        },
    );
    (sink, queue, events)
}

fn batch(changes: &[(&str, PathChange)]) -> WatchBatch {
    WatchBatch {
        changes: changes
            .iter()
            .map(|(p, c)| (PathBuf::from(p), *c))
            .collect(),
        rescan: false,
    }
}

/// Waits up to five seconds for the sink's worker to hand something to the queue.
fn wait_for_queue(queue: &Arc<Mutex<IndexWorkQueue>>) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !queue.lock().unwrap().is_empty() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

#[test]
fn a_source_file_is_marked_dirty_before_it_enters_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sink, queue, _) = sink(dir.path());

    sink.accept(batch(&[("src/a.py", Modified)]));

    // Persisted at accept, not at the flush a second later: a crash in
    // between must still leave the path recoverable (R3.3.5).
    assert_eq!(
        infigraph_core::dirty::pending_dirty(&dir.path().join(".infigraph")).unwrap(),
        std::collections::HashSet::from(["src/a.py".to_string()])
    );
    assert!(
        queue.lock().unwrap().is_empty(),
        "the window has not closed"
    );
}

#[test]
fn a_closed_window_flushes_into_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sink, queue, _) = sink(dir.path());
    sink.accept(batch(&[("src/a.py", Created)]));

    assert!(
        wait_for_queue(&queue),
        "the settled window is handed to the queue"
    );
}

#[test]
fn only_code_the_registry_claims_and_never_a_lockfile_reaches_the_sink() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sink, _queue, _) = sink(dir.path());

    sink.accept(batch(&[
        ("notes.unknownext", Created),
        ("pnpm-lock.yaml", Modified),
        ("image.png", Created),
    ]));

    assert!(
        infigraph_core::dirty::pending_dirty(&dir.path().join(".infigraph"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_removal_is_marked_queued_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sink, queue, events) = sink(dir.path());

    sink.accept(batch(&[("src/gone", Removed)]));

    assert_eq!(
        infigraph_core::dirty::pending_dirty(&dir.path().join(".infigraph")).unwrap(),
        std::collections::HashSet::from(["src/gone".to_string()])
    );
    assert!(wait_for_queue(&queue));
    assert_eq!(events.lock().unwrap().len(), 1);
}

#[test]
fn a_rescan_queues_a_whole_project_pass() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sink, queue, _) = sink(dir.path());

    sink.accept(WatchBatch::rescan());

    assert!(wait_for_queue(&queue));
    assert!(queue.lock().unwrap().wants_whole_project());
}
