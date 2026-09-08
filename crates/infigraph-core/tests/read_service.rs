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
