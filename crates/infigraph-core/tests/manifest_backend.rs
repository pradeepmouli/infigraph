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

/// #166: the Dependency node was a real upsert but its DEPENDS_ON edge was an
/// unconditional CREATE, so every re-run of manifest indexing added another
/// edge per dependency. A repeat must converge to one edge, and a dependency
/// that moves between dev and regular must update that edge, not add a
/// second.
#[test]
fn upsert_dependencies_twice_keeps_one_depends_on_edge_and_updates_it() {
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
    let backend = infigraph.backend().unwrap();
    backend
        .raw_query(
            "CREATE (:Module {id: 'requirements.txt', name: 'requirements.txt', \
             file: 'requirements.txt', language: 'text', content_hash: 'h', summary: ''})",
        )
        .unwrap();
    let manifest = |is_dev: bool| ManifestResult {
        ecosystem: "pypi".to_string(),
        manifest_file: "requirements.txt".to_string(),
        deps: vec![DepEntry {
            name: "requests".to_string(),
            version: "2.31.0".to_string(),
            ecosystem: "pypi".to_string(),
            is_dev,
        }],
        doc_urls: vec![],
    };

    backend.upsert_dependencies(&manifest(false)).unwrap();
    backend.upsert_dependencies(&manifest(false)).unwrap();
    backend.upsert_dependencies(&manifest(true)).unwrap();

    let rows = backend
        .raw_query("MATCH (:Module)-[r:DEPENDS_ON]->(d:Dependency) RETURN d.id, r.is_dev")
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "one edge per module and dependency: {rows:?}"
    );
    assert_eq!(
        rows[0][1], "True",
        "the edge must carry the latest is_dev: {rows:?}"
    );
}
