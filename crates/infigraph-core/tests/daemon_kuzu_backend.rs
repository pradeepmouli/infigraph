use infigraph_core::graph::{DaemonKuzuBackend, GraphBackend};
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn read_only_connection_rejects_write_statements() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap(); // opens direct Kuzu, creates the graph on disk
    drop(infigraph); // release the write connection so the read-only open below can succeed

    let dk = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    let result = dk.raw_query("CREATE (n:Symbol {id: 'should-not-be-written'})");

    assert!(
        result.is_err(),
        "a CREATE through the read-only connection must fail at the DB level"
    );

    // Confirm nothing was actually written -- reopen a fresh read-only
    // connection (not reusing dk, to rule out any client-side caching)
    // and check the node genuinely doesn't exist.
    let verify = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    let rows = verify
        .raw_query("MATCH (n:Symbol {id: 'should-not-be-written'}) RETURN n.id")
        .unwrap();
    assert!(
        rows.is_empty(),
        "the rejected CREATE must not have partially applied"
    );
}

#[test]
fn read_methods_pass_through_to_a_real_connection() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    infigraph.index().unwrap();
    drop(infigraph);

    let dk = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    let stats = dk.stats().unwrap();
    assert!(
        stats.symbols > 0,
        "expected real read access to the already-indexed graph"
    );
}
