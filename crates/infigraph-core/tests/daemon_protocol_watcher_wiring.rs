use infigraph_core::daemon_protocol::{submit_write_request, WriteRequest, WriteResult};
use infigraph_languages::bundled_registry;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn watch_loop_serves_write_requests_when_serve_requests_is_true() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    let (stop_tx, stop_rx) = mpsc::channel();
    let root = project_dir.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::watch::watch_project_with_periodic(
            &root,
            || Ok(bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
        )
    });

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    let request = WriteRequest::Index { paths: None };
    // 30s, not 5s: this asserts the WIRING serves requests, not first-reply
    // latency. Debug-build registry construction alone approached the old
    // 5s budget on loaded machines (#58) -- a genuine wiring hang still
    // fails, just without the false negatives.
    let result = submit_write_request(&staging_dir, &request, Duration::from_secs(30)).unwrap();

    #[allow(unreachable_patterns)]
    match result {
        WriteResult::Ok { indexed_files, .. } => assert_eq!(indexed_files, 1),
        WriteResult::Err { message } => panic!("expected Ok, got Err: {message}"),
        other => panic!("unexpected WriteResult for Index: {other:?}"),
    }

    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
}

#[test]
fn watch_loop_does_not_serve_requests_when_serve_requests_is_false() {
    let project_dir = tempfile::tempdir().unwrap();
    let (stop_tx, stop_rx) = mpsc::channel();
    let root = project_dir.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::watch::watch_project_with_periodic(
            &root,
            || Ok(bundled_registry().unwrap()),
            50,
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            false, // serve_requests
            None,
        )
    });

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    let request = WriteRequest::Index { paths: None };
    let result = submit_write_request(&staging_dir, &request, Duration::from_millis(500));
    assert!(
        result.is_err(),
        "expected a timeout -- serve_requests=false must never serve"
    );

    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
}
