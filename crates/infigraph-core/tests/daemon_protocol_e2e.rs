// crates/infigraph-core/tests/daemon_protocol_e2e.rs
//
// Full round trip: a "client" thread submits a request via
// submit_write_request while a "server" thread polls the staging
// directory and calls serve_one_request -- proving the two halves built
// in this plan actually interoperate, not just pass their own unit tests
// against a hand-rolled stand-in for the other side.

use infigraph_core::daemon_protocol::{submit_write_request, WriteRequest};
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;
use std::time::Duration;

#[test]
fn client_and_server_interoperate_end_to_end() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();

    // Server thread: polls the staging dir for new .request files and
    // serves the first one it sees, using serve_one_request directly
    // (not the real watcher loop -- that's a later plan).
    let staging_dir_clone = staging_dir.clone();
    let server_handle = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        loop {
            let entries: Vec<_> = std::fs::read_dir(&staging_dir_clone)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "request"))
                .collect();
            if let Some(entry) = entries.first() {
                infigraph_core::daemon_protocol::serve_one_request(&infigraph, &entry.path())
                    .unwrap();
                return;
            }
            if start.elapsed() > Duration::from_secs(5) {
                panic!("server thread never saw a request file appear");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    let result = submit_write_request(
        &staging_dir,
        &WriteRequest::Index { paths: None },
        Duration::from_secs(5),
    )
    .unwrap();

    match result {
        infigraph_core::daemon_protocol::WriteResult::Ok { indexed_files, .. } => {
            assert_eq!(indexed_files, 1)
        }
        infigraph_core::daemon_protocol::WriteResult::Err { message } => {
            panic!("expected Ok, got Err: {message}")
        }
    }

    server_handle.join().unwrap();
}
