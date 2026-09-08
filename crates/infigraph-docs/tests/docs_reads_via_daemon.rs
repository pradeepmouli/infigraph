//! `search` with `scope='all'` hits the code graph AND the document store in
//! one call, so routing only the graph would leave the flagship read tool
//! still opening `docs.kuzu` directly. `docs.kuzu` has its own lock file and
//! its own wipe-on-any-open-failure history (#143).

use std::path::Path;
use std::sync::Arc;

use infigraph_core::daemon::read_service::ReadService;
use infigraph_core::graph::query_exec::QueryExec;
use infigraph_core::graph::remote_exec::RemoteExec;
use infigraph_core::graph::GraphStore;
use infigraph_docs::store::DocStore;

#[test]
fn document_reads_are_served_by_the_daemon_read_service() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    seed_one_document(root);

    let docs = infigraph_docs::daemon_source::daemon_row_source(root).unwrap();
    let svc = ReadService::start_with_sources(root, graph_source(root), Some(docs), 2).unwrap();

    let exec = RemoteExec::for_docs(root);
    let rows = exec.query_rows("MATCH (d:Document) RETURN d.id").unwrap();
    assert_eq!(
        rows,
        vec![vec!["a.md".to_string()]],
        "the docs store must answer over the read service"
    );

    svc.shutdown();
}

/// A write sent to the document side is refused by the database's own
/// parser, exactly as on the graph side -- the read service must not become
/// a write backdoor for either store.
#[test]
fn a_write_sent_to_the_document_side_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    seed_one_document(root);

    let docs = infigraph_docs::daemon_source::daemon_row_source(root).unwrap();
    let svc = ReadService::start_with_sources(root, graph_source(root), Some(docs), 2).unwrap();

    let exec = RemoteExec::for_docs(root);
    let err = exec
        .query_rows("CREATE (:Document {id: 'x', title: 'x', file: 'x', format: 'md', content_hash: 'h', page_count: 0, chunk_count: 0})")
        .expect_err("a write must be refused");
    assert!(err.to_string().contains("not a read"), "unexpected: {err}");

    svc.shutdown();
}

/// With no daemon listening, a document read must fail rather than silently
/// opening `docs.kuzu` in this process.
#[test]
fn document_reads_fail_without_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    seed_one_document(root);

    let exec = RemoteExec::for_docs(root);
    assert!(
        exec.query_rows("MATCH (d:Document) RETURN d.id").is_err(),
        "no daemon must be an error, not a silent direct open"
    );
}

// ── helpers ──────────────────────────────────────────────────────────

/// Seed one document through the docs store's own open path, then drop it:
/// `DocStore` holds the process-wide DB_LOCK, so it must be released before
/// `daemon_row_source` opens its own.
fn seed_one_document(root: &Path) {
    let path = root.join(".infigraph").join("docs.kuzu");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store = DocStore::open(&path).unwrap();
    let conn = store.connection().unwrap();
    conn.query(
        "CREATE (:Document {id: 'a.md', title: 'A', file: 'a.md', format: 'md', \
         content_hash: 'h', page_count: 0, chunk_count: 1})",
    )
    .unwrap();
}

/// The graph half of the service. These tests are about the docs half, but
/// the service requires a graph source.
fn graph_source(root: &Path) -> infigraph_core::daemon::read_service::StoreSource {
    let graph = root.join(".infigraph").join("graph");
    let store = Arc::new(GraphStore::open(&graph).unwrap());
    Arc::new(move || Some(store.clone()))
}
