//! End-to-end tests for the daemon's read service.

use std::path::Path;
use std::sync::Arc;

use infigraph_core::daemon::read_endpoint::ReadEndpoint;
use infigraph_core::daemon::read_protocol::{collect_rows, write_request, ReadRequest, Store};
use infigraph_core::graph::GraphStore;

/// End-to-end: a client gets rows back over the socket.
#[test]
fn a_client_reads_rows_over_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    {
        let store = GraphStore::open(&graph).unwrap();
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();
    }
    let store = open_shared_store(&graph);
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, store, 4).unwrap();

    let rows = client_query(root, "MATCH (f:File) RETURN f.id").unwrap();
    assert_eq!(rows, vec![vec!["a.rs".to_string()]]);

    svc.shutdown();
}

/// A write sent to the read service is refused, and the refusal comes from
/// the database, not from a keyword check.
#[test]
fn a_write_sent_to_the_read_service_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    drop(GraphStore::open(&graph).unwrap());
    let store = open_shared_store(&graph);
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, store, 2).unwrap();

    let err = client_query(
        root,
        "CREATE (:File {id: 'x', name: 'x', path: 'x', language: 'rust', symbol_count: 0})",
    )
    .expect_err("a write must be refused");
    assert!(err.to_string().contains("not a read"), "unexpected: {err}");

    svc.shutdown();
}

/// A bare `COMMIT` returns an empty result rather than an error, and that
/// is a decision, not an accident.
///
/// lbug's parser judges `COMMIT` read-only, so it passes `ensure_read_only`
/// and reaches `raw_query_on`'s transaction-control no-op -- which is
/// exactly what `DaemonKuzuBackend::raw_query` does today through
/// `open_read` -> `KuzuBackend::raw_query`. Refusing it instead is arguably
/// more correct for a read service, but it would change observable
/// behaviour for `raw_query`'s ~247 callers in a change whose only purpose
/// is transport routing, and this plan's constraint is that the write
/// pipeline and its semantics are untouched.
///
/// This does not weaken the truncation guarantee: the reply is a legitimate
/// empty result *with* an `End` frame. A truncated stream has no `End` and
/// is still an error -- see `read_protocol` and the test below.
#[test]
fn a_bare_commit_returns_an_empty_result_exactly_as_the_local_backend_does() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    let store = open_shared_store(&graph);
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, store, 2).unwrap();

    let got = client_query(root, "COMMIT").expect("a bare COMMIT must not be an error");
    assert!(
        got.is_empty(),
        "a transaction-control statement no-ops to zero rows, as it does locally: {got:?}"
    );

    svc.shutdown();
}

/// Reads must be served in parallel *while* an index operation holds
/// `index.lock`. If reads ever queue behind indexing, the read service has
/// been wired into the write pipeline by mistake and the whole point is
/// lost.
#[test]
fn reads_are_served_concurrently_while_an_index_operation_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    {
        let store = GraphStore::open(&graph).unwrap();
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();
    }
    let store = open_shared_store(&graph);
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, store, 8).unwrap();

    // Hold index.lock for the duration, as a real index operation would.
    let _index_lock = infigraph_core::lockfile::acquire(
        &root.join(".infigraph").join("index.lock"),
        "test-index-op",
        std::time::Duration::from_secs(5),
    )
    .expect("acquire index.lock");

    let start = std::time::Instant::now();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let root = root.to_path_buf();
            std::thread::spawn(move || client_query(&root, "MATCH (f:File) RETURN f.id"))
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap().unwrap(), vec![vec!["a.rs".to_string()]]);
    }
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "8 reads took {:?} with index.lock held -- they are queueing behind it",
        start.elapsed()
    );

    svc.shutdown();
}

/// A write committed by the daemon must be visible to the very next read.
///
/// If the read service ever holds its own `Database` -- even in the same
/// process -- it cannot see the writer's uncommitted WAL, and this fails.
/// That is #149 reproduced inside the daemon, and no test without a
/// concurrent writer would notice.
#[test]
fn a_write_is_visible_to_the_next_read_through_the_service() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();

    let store = open_shared_store(&graph);
    let svc =
        infigraph_core::daemon::read_service::ReadService::start(root, store.clone(), 4).unwrap();

    assert!(client_query(root, "MATCH (f:File) RETURN f.id")
        .unwrap()
        .is_empty());

    // Write through the SAME store the service holds, without checkpointing.
    {
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'fresh.rs', name: 'fresh.rs', path: 'fresh.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();
    }

    let rows = client_query(root, "MATCH (f:File) RETURN f.id").unwrap();
    assert_eq!(
        rows,
        vec![vec!["fresh.rs".to_string()]],
        "the read service must observe the daemon's own uncheckpointed write; \
         if this is empty, the service is holding a second Database handle"
    );

    svc.shutdown();
}

/// The client-side executor speaks the same protocol the service serves,
/// and satisfies the same `QueryExec` trait `GraphQuery` runs on -- which is
/// what lets all 1045 lines of Cypher serve both paths unchanged.
#[test]
fn remote_exec_satisfies_query_exec_against_a_live_service() {
    use infigraph_core::graph::query_exec::QueryExec;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    {
        let store = GraphStore::open(&graph).unwrap();
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();
    }
    let store = open_shared_store(&graph);
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, store, 2).unwrap();

    // `RemoteExec` satisfies `QueryExec`, so it can be handed to
    // `GraphQuery::new_with(exec)` by value exactly like `LocalExec`.
    let exec = infigraph_core::graph::remote_exec::RemoteExec::new(root);
    let rows = exec.query_rows("MATCH (f:File) RETURN f.id").unwrap();
    assert_eq!(rows, vec![vec!["a.rs".to_string()]]);

    // And the point of the trait: a real `GraphQuery` runs over it unchanged.
    let q = infigraph_core::graph::GraphQuery::new_with(exec);
    let via_graph_query = q.raw_query("MATCH (f:File) RETURN f.id").unwrap();
    assert_eq!(via_graph_query, rows);

    svc.shutdown();
}

/// Killing the service mid-response must surface an error, never a
/// successful empty result. This is the one failure mode with a precedent
/// in this codebase: `Infigraph::init` once served a 0-symbol graph as
/// healthy after a "successful" rebuild.
///
/// Note this is the frame-boundary case, not the mid-frame one: the bytes
/// written here are a complete, well-formed `Rows` frame and nothing more,
/// so the stream is byte-identical to a valid response that simply has not
/// finished. Only the absent `End` distinguishes it from an empty result.
#[test]
fn a_service_that_dies_mid_response_produces_an_error_not_an_empty_result() {
    use infigraph_core::daemon::read_protocol::{collect_rows, write_frame, ReadFrame};

    // Simulate the wire directly: rows, then the connection dies.
    let mut buf = Vec::new();
    write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["a.rs".to_string()]])).unwrap();
    // No End frame -- the peer went away.

    let err =
        collect_rows(&mut buf.as_slice()).expect_err("a stream with no End frame must be an error");
    assert!(
        err.to_string().contains("not an empty result set"),
        "the error must say plainly that this is not an empty result: {err}"
    );
}

/// A real write coordinator must answer reads on its endpoint. Without this
/// the service exists but nothing starts it.
///
/// Asserts only that the read *succeeds*, not that it returns rows. Whether
/// the daemon has indexed anything yet is a separate concern, and on macOS
/// a `TempDir` root is symlinked, where watch-driven indexing currently
/// delivers no events at all (#151). The property this task introduces is
/// that the daemon binds the endpoint and serves from its own store, and
/// only the daemon can answer here -- nothing else binds this name.
#[test]
#[ignore = "drives a real write coordinator; run explicitly"]
fn a_running_daemon_answers_reads_on_its_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("a.rs"), "pub fn hello() {}\n").unwrap();

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = token.clone();
    let root_for_thread = root.clone();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root_for_thread,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50,
            stop_rx,
            |_| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
            &token_for_thread,
            None,
            None,
        )
    });

    // Generous: the coordinator builds the whole bundled language registry
    // before it opens anything, which its own comment notes costs seconds in
    // a debug build.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut served = false;
    let mut last_err = None;
    while std::time::Instant::now() < deadline {
        match client_query(&root, "MATCH (f:File) RETURN f.id") {
            Ok(_) => {
                served = true;
                break;
            }
            Err(e) => last_err = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    token.cancel();
    let _ = stop_tx.send(());
    let _ = handle.join();

    assert!(
        served,
        "a running daemon must serve reads on its endpoint; last error: {last_err:?}"
    );
}

// ── helpers ──────────────────────────────────────────────────────────

/// The one `GraphStore` the service serves from.
///
/// Deliberately a plain `GraphStore::open` behind an `Arc`, not a bespoke
/// `Database`: opening through the store is what carries `validate_db_file`'s
/// truncation preflight, `refuse_newer_schema`, the bounded write buffer
/// pool and the `write_phase` breadcrumbs.
fn open_shared_store(graph: &Path) -> Arc<GraphStore> {
    Arc::new(GraphStore::open(graph).unwrap())
}

/// Ask the read service at `root` for `cypher`, exactly as `RemoteExec` will.
fn client_query(root: &Path, cypher: &str) -> anyhow::Result<Vec<Vec<String>>> {
    let mut stream = ReadEndpoint::for_root(root).connect()?;
    write_request(
        &mut stream,
        &ReadRequest {
            store: Store::Graph,
            query: cypher.to_string(),
            params: vec![],
            chunk_size: 1024,
        },
    )?;
    collect_rows(&mut stream)
}
