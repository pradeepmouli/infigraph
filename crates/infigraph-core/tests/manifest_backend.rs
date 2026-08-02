use infigraph_core::manifest::{DepEntry, ManifestResult};
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn upsert_dependencies_creates_dependency_node_and_edge() {
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

    let result = ManifestResult {
        ecosystem: "pypi".to_string(),
        manifest_file: "requirements.txt".to_string(),
        deps: vec![DepEntry {
            name: "requests".to_string(),
            version: "2.31.0".to_string(),
            ecosystem: "pypi".to_string(),
            is_dev: false,
        }],
        doc_urls: vec![],
    };

    let backend = infigraph.backend().unwrap();
    backend.upsert_dependencies(&result).unwrap();

    let rows = backend
        .raw_query("MATCH (d:Dependency) WHERE d.id = 'pypi::requests' RETURN d.id")
        .unwrap();
    assert_eq!(rows.len(), 1, "expected the Dependency node to exist");
}
