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

/// The source must not hold `docs.kuzu` open between reads.
///
/// `DocStore::open` takes the process-wide `DB_LOCK` and holds the guard for
/// the store's lifetime, so a source that kept one open for the daemon's
/// lifetime would block the doc watcher's next `DocIndex::init()` forever --
/// which is exactly what the first version of this did.
#[test]
fn the_source_does_not_hold_the_store_open_between_reads() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    seed_one_document(root);

    let docs = infigraph_docs::daemon_source::daemon_row_source(root).unwrap();
    let svc = ReadService::start_with_sources(root, graph_source(root), Some(docs), 2).unwrap();

    // Checked BEFORE any read: if the source holds DB_LOCK, a read blocks
    // inside the service and the client waits on the socket forever, so
    // probing the lock first is what turns this into a clean failure
    // instead of a hung test.
    //
    // With the service live, another `DocStore` must be openable -- as the
    // doc watcher does on every reindex.
    let path = root.join(".infigraph").join("docs.kuzu");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(DocStore::open(&path).is_ok());
    });
    let opened = rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("opening a second DocStore blocked -- the source is holding DB_LOCK");
    assert!(
        opened,
        "the doc watcher must still be able to open the store"
    );

    // And reads work with the service live.
    let exec = RemoteExec::for_docs(root);
    let rows = exec.query_rows("MATCH (d:Document) RETURN d.id").unwrap();
    assert_eq!(rows, vec![vec!["a.md".to_string()]]);

    svc.shutdown();
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
