//! A watch handler whose write to the graph failed has the coordinator drop and reopen its store,
//! as a failed drain does, and raise no further event of that batch (the handle is suspect).

use infigraph_core::daemon::writes::{submit, WriteOpts};
use infigraph_core::daemon_protocol::{WriteRequest, WriteResult};
use infigraph_languages::bundled_registry;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn a_handler_that_reports_a_failed_write_gets_the_store_reopened_and_no_more_events() {
    let project_dir = tempfile::tempdir().unwrap();
    // Two files, so the first drain is ONE batch of two extractions: the handler fails on the
    // first event, and the second must not be raised.
    for (name, body) in [
        ("a.py", "def alpha():\n    pass\n"),
        ("b.py", "def beta():\n    pass\n"),
    ] {
        std::fs::write(project_dir.path().join(name), body).unwrap();
    }

    let calls: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let calls_in_handler = Arc::clone(&calls);
    let (stop_tx, stop_rx) = mpsc::channel();
    let root = project_dir.path().to_path_buf();
    let daemon_token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = daemon_token.clone();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            || Ok(bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            move |evt| {
                // Only the events that carry the coordinator's store: say the write failed.
                if let Some(held) = evt.held.as_ref() {
                    calls_in_handler.lock().unwrap().push(evt.path.clone());
                    held.report_failed();
                }
            },
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
            &token_for_thread,
            None,
            None,
        )
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    while calls.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the handler was never called for the first drain"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The loop reopens a dropped store when something asks for it: an index request.
    let result = submit(
        project_dir.path(),
        &WriteRequest::Index { paths: None },
        WriteOpts {
            timeout: Duration::from_secs(60),
            cancel: None,
            on_slot_wait: None,
        },
    )
    .unwrap();
    assert!(
        matches!(result, WriteResult::Ok { .. }),
        "the index request after the failure: {result:?}"
    );
    std::thread::sleep(Duration::from_millis(500));

    let graph = project_dir
        .path()
        .canonicalize()
        .unwrap()
        .join(".infigraph")
        .join(infigraph_core::graph::names::STEM);
    let stats = infigraph_core::graph::rw_database_stats(&graph);
    assert!(
        stats.opened >= 2,
        "the failed handler's store was never dropped and reopened: {stats:?}"
    );
    assert_eq!(
        stats.most_at_once, 1,
        "the reopen must wait for the old store to close: {stats:?}"
    );
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "the batch went on after its handler failed: {:?}",
        calls.lock().unwrap()
    );

    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
}
