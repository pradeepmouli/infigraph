use infigraph_core::graph::{DaemonKuzuBackend, GraphBackend};
use infigraph_core::structured::SchemaMeta;
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;
use std::path::Path;

/// Spawns a background thread that watches `staging_dir` for the next
/// `.request` file to appear and serves exactly one request against a
/// fresh write-mode `Infigraph` opened on `project_dir`, then returns.
/// Shared by every wrapper test below -- each test's `DaemonKuzuBackend`
/// call submits one request; this is the "daemon side" that answers it.
fn spawn_one_request_server(project_dir: &Path) -> std::thread::JoinHandle<()> {
    let project_dir = project_dir.to_path_buf();
    std::thread::spawn(move || {
        let registry = bundled_registry().unwrap();
        let mut server_infigraph = Infigraph::open(&project_dir, registry).unwrap();
        server_infigraph.init().unwrap();
        let staging_dir = project_dir.join(".infigraph").join("requests");
        let start = std::time::Instant::now();
        loop {
            if let Ok(entries) = std::fs::read_dir(&staging_dir) {
                for entry in entries.flatten() {
                    if entry.path().extension().is_some_and(|e| e == "request") {
                        infigraph_core::daemon_protocol::serve_one_request(
                            &server_infigraph,
                            &entry.path(),
                        )
                        .unwrap();
                        return;
                    }
                }
            }
            if start.elapsed() > std::time::Duration::from_secs(5) {
                panic!("test daemon never saw a request");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    })
}

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

#[test]
fn wrapper_upsert_repo_routes_through_daemon_protocol() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    drop(infigraph);

    let handle = spawn_one_request_server(project_dir.path());

    let dk = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    dk.upsert_repo("org/repo").unwrap();

    handle.join().unwrap();
}

#[test]
fn wrapper_derive_tested_by_edges_routes_through_daemon_protocol() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    drop(infigraph);

    let handle = spawn_one_request_server(project_dir.path());

    let dk = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    let count = dk.derive_tested_by_edges(None).unwrap();
    assert_eq!(count, 0, "empty graph has no TESTED_BY edges to derive");

    handle.join().unwrap();
}

#[test]
fn wrapper_upsert_similar_edge_routes_through_daemon_protocol() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    drop(infigraph);

    let handle = spawn_one_request_server(project_dir.path());

    let dk = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    dk.upsert_similar_edge("a::foo", "b::bar", 0.9).unwrap();

    handle.join().unwrap();
}

#[test]
fn wrapper_write_calls_service_edges_cleans_up_arrow_sibling() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    drop(infigraph);

    let handle = spawn_one_request_server(project_dir.path());

    let dk = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    let edge = infigraph_core::graph::CallsServiceEdge {
        symbol_id: "a::foo".to_string(),
        target_id: "svc::bar".to_string(),
        method: "GET".to_string(),
        path: "/bar".to_string(),
    };
    dk.write_calls_service_edges(&[edge]).unwrap();

    handle.join().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    let leftover: Vec<_> = std::fs::read_dir(&staging_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "arrow"))
        .collect();
    assert!(
        leftover.is_empty(),
        "expected the Arrow sibling file to be cleaned up after serving, found: {leftover:?}"
    );
}

#[test]
fn wrapper_ingest_structured_data_inline_cleans_up_sibling() {
    let project_dir = tempfile::tempdir().unwrap();
    let schema_dir = project_dir
        .path()
        .join(".infigraph")
        .join("structured-schemas");
    std::fs::create_dir_all(&schema_dir).unwrap();
    std::fs::write(
        schema_dir.join("test.toml"),
        r#"
[schema]
schema_id = "test_schema"
name = "Test Schema"
node_table = "TestNode"
"#,
    )
    .unwrap();

    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    drop(infigraph);

    let handle = spawn_one_request_server(project_dir.path());

    let dk = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    let schema = SchemaMeta {
        schema_id: "test_schema".to_string(),
        name: "Test Schema".to_string(),
        node_table: "TestNode".to_string(),
        columns: Vec::new(),
        edges: Vec::new(),
        searchable_fields: Vec::new(),
        id_template: None,
    };
    let data = vec![
        serde_json::json!({"id": "a"}),
        serde_json::json!({"id": "b"}),
        serde_json::json!({"id": "c"}),
    ];
    let result = dk.ingest_structured_data(&schema, &data).unwrap();
    assert_eq!(result.nodes_created, 3);

    handle.join().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    let leftover: Vec<_> = std::fs::read_dir(&staging_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert!(
        leftover.is_empty(),
        "expected the inline data sibling file to be cleaned up after serving, found: {leftover:?}"
    );
}
