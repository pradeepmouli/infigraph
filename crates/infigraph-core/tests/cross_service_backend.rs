use infigraph_core::graph::CrossServiceEdgeCandidate;
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn write_cross_service_edges_creates_target_node_and_edge_once() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def caller():\n    pass\n",
    )
    .unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    infigraph.index().unwrap();

    let symbols = infigraph
        .backend()
        .unwrap()
        .symbols_with_docstring(None)
        .unwrap();
    let caller_id = symbols.first().map(|s| s.id.clone()).unwrap();

    let backend = infigraph.backend().unwrap();
    let candidate = CrossServiceEdgeCandidate {
        target_id: "xsvc::payments::POST::/charge".to_string(),
        target_name: "payments POST /charge".to_string(),
        docstring: "External service: payments POST /charge".to_string(),
        caller_symbol_id: caller_id.clone(),
        method: "POST".to_string(),
        path: "/charge".to_string(),
        target_service: "payments".to_string(),
        protocol: "http".to_string(),
    };

    let created_first = backend
        .write_cross_service_edges(std::slice::from_ref(&candidate))
        .unwrap();
    assert_eq!(created_first, 1);

    // Idempotent: running the same candidate again creates no new edge.
    let created_second = backend.write_cross_service_edges(&[candidate]).unwrap();
    assert_eq!(created_second, 0);

    let rows = backend
        .raw_query("MATCH (:Symbol)-[e:CALLS_SERVICE]->(:Symbol) RETURN e.method")
        .unwrap();
    assert_eq!(rows.len(), 1, "expected exactly one edge, not a duplicate");
}
