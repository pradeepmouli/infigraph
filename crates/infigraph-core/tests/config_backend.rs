use infigraph_core::config::ConfigBindingWire;
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn store_config_bindings_creates_node_and_edge() {
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
    let symbol_id = symbols
        .first()
        .map(|s| s.id.clone())
        .unwrap_or_else(|| "nonexistent".to_string());

    let backend = infigraph.backend().unwrap();
    let bindings = vec![ConfigBindingWire {
        symbol_id: symbol_id.clone(),
        kind: "EnvVar".to_string(),
        key: "DATABASE_URL".to_string(),
        value: "postgres://...".to_string(),
        profile: "default".to_string(),
        source_file: "main.py".to_string(),
    }];
    backend.store_config_bindings(&bindings).unwrap();

    let rows = backend
        .raw_query("MATCH (c:ConfigBinding) RETURN c.id")
        .unwrap();
    assert_eq!(rows.len(), 1);
}
