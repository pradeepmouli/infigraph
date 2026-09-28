// crates/infigraph-core/tests/daemon_protocol_e2e.rs
//
// Full round trip over the socket (#204): a client submits a write with
// `daemon::writes::submit` while a stub coordinator behind a real read
// service answers it with `serve_write` -- proving the two halves
// interoperate, not just pass their own unit tests against a hand-rolled
// stand-in for the other side.

use infigraph_core::daemon::coordinator_port::{CoordinatorPort, PortMsg};
use infigraph_core::daemon::liveness::Liveness;
use infigraph_core::daemon::read_service::ReadService;
use infigraph_core::daemon::writes::{submit, WriteOpts};
use infigraph_core::daemon_protocol::{serve_write, WriteRequest, WriteResult};
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;
use std::sync::Arc;
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

    let (port, rx) = CoordinatorPort::new(1800, 60);
    let svc = ReadService::start_serving(
        project_dir.path(),
        Arc::new(|| None),
        None,
        2,
        Arc::new(Liveness::new()),
        Some(port),
    )
    .unwrap();

    // Server thread: the stub coordinator serves the first write it gets,
    // with `serve_write` directly (not the real coordinator loop).
    let server_handle = std::thread::spawn(move || {
        let PortMsg::Write { request, reply } = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the server never saw a write")
        else {
            panic!("expected a write")
        };
        reply.send(serve_write(&infigraph, &request));
    });

    let result = submit(
        project_dir.path(),
        &WriteRequest::Index { paths: None },
        WriteOpts {
            timeout: Duration::from_secs(30),
            cancel: None,
        },
    )
    .unwrap();

    match result {
        WriteResult::Ok { indexed_files, .. } => assert_eq!(indexed_files, 1),
        other => panic!("expected Ok, got {other:?}"),
    }

    server_handle.join().unwrap();
    svc.shutdown();
}
