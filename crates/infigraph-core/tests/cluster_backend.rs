use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn store_clusters_creates_cluster_node_and_membership() {
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

    let symbols = infigraph
        .backend()
        .unwrap()
        .symbols_with_docstring(None)
        .unwrap();
    assert!(!symbols.is_empty());

    let backend = infigraph.backend().unwrap();
    let idx_to_id: Vec<String> = symbols.iter().map(|s| s.id.clone()).collect();
    let community: Vec<usize> = vec![0; idx_to_id.len()];
    let stats = backend.store_clusters(&idx_to_id, &community, 0.5).unwrap();

    assert_eq!(stats.num_clusters, 1);

    let rows = backend.raw_query("MATCH (c:Cluster) RETURN c.id").unwrap();
    assert_eq!(rows.len(), 1);
}
