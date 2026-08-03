use infigraph_core::config::ConfigBindingWire;
use infigraph_core::daemon_protocol::{
    serve_one_request, write_atomic, write_extractions_json, IngestSource, WriteRequest,
    WriteResult,
};
use infigraph_core::manifest::{DepEntry, ManifestResult};
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn serve_one_request_indexes_and_writes_result_and_removes_request() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-1.request");
    let result_path = staging_dir.join("test-1.result");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::Index { paths: None }).unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    assert!(
        !request_path.exists(),
        "request file should be removed after serving"
    );
    assert!(result_path.exists(), "result file should have been written");
    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    match result {
        WriteResult::Ok { indexed_files, .. } => assert_eq!(indexed_files, 1),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn serve_one_request_writes_err_result_on_failure_without_panicking() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-2.request");
    let result_path = staging_dir.join("test-2.result");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::Index {
            paths: Some(vec!["does/not/exist.py".into()]),
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();
    assert!(result_path.exists());
}

#[test]
fn serve_one_request_writes_err_result_on_corrupt_request_json() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-3.request");
    let result_path = staging_dir.join("test-3.result");
    write_atomic(&request_path, "not valid json {{{").unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    assert!(
        result_path.exists(),
        "corrupt request must still produce a result file"
    );
    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(result, WriteResult::Err { .. }),
        "expected Err for a corrupt request, got {result:?}"
    );
}

#[test]
fn serve_one_request_handles_scip_import() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-scip.request");
    let result_path = staging_dir.join("test-scip.result");
    // A nonexistent scip file is fine for this test -- it exercises the
    // handler routes to import_scip and returns Err cleanly, not that a
    // real SCIP import succeeds (that's covered by existing scip-import
    // integration tests).
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::ScipImport {
            scip_path: "does/not/exist.scip".into(),
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    assert!(result_path.exists());
    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(result, WriteResult::Err { .. }),
        "expected Err for a missing scip file, got {result:?}"
    );
}

#[test]
fn serve_one_request_handles_ingest_structured_file() {
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
    std::fs::write(
        project_dir.path().join("data.json"),
        r#"[{"id": "a"}, {"id": "b"}]"#,
    )
    .unwrap();

    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-ingest.request");
    let result_path = staging_dir.join("test-ingest.result");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::IngestStructured {
            schema_id: "test_schema".to_string(),
            source: IngestSource::File("data.json".into()),
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    match result {
        WriteResult::Ok { indexed_files, .. } => assert_eq!(indexed_files, 2),
        WriteResult::Err { message } => panic!("expected Ok, got Err: {message}"),
        other => panic!("unexpected WriteResult for IngestStructured: {other:?}"),
    }
}

#[test]
fn serve_one_request_handles_ingest_structured_inline() {
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

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-inline.request");
    let result_path = staging_dir.join("test-inline.result");
    let sibling_path = staging_dir.join("test-inline.data.json");

    write_atomic(&sibling_path, r#"[{"id": "a"}, {"id": "b"}, {"id": "c"}]"#).unwrap();
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::IngestStructured {
            schema_id: "test_schema".to_string(),
            source: IngestSource::Inline,
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    assert!(
        !sibling_path.exists(),
        "sibling data file should be cleaned up after serving"
    );
    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    match result {
        WriteResult::Ok { indexed_files, .. } => assert_eq!(indexed_files, 3),
        WriteResult::Err { message } => panic!("expected Ok, got Err: {message}"),
        other => panic!("unexpected WriteResult for IngestStructured Inline: {other:?}"),
    }
}

#[test]
fn serve_one_request_handles_upsert_repo() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-repo.request");
    let result_path = staging_dir.join("test-repo.result");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::UpsertRepo {
            namespace: "org/repo".to_string(),
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(result, WriteResult::Ok { .. }),
        "expected Ok, got {result:?}"
    );
}

#[test]
fn serve_one_request_handles_upsert_similar_edge() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def foo():\n    pass\n\ndef bar():\n    pass\n",
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
    assert!(symbols.len() >= 2, "expected at least 2 symbols to link");

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-similar.request");
    let result_path = staging_dir.join("test-similar.result");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::UpsertSimilarEdge {
            id_a: symbols[0].id.clone(),
            id_b: symbols[1].id.clone(),
            score: 0.9,
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(result, WriteResult::Ok { .. }),
        "expected Ok, got {result:?}"
    );
}

#[test]
fn serve_one_request_handles_write_calls_service_edges() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-cse.request");
    let result_path = staging_dir.join("test-cse.result");
    let edges_path = staging_dir.join("test-cse.edges.arrow");

    let edges = vec![
        infigraph_core::graph::CallsServiceEdge {
            symbol_id: "s1".to_string(),
            target_id: "t1".to_string(),
            method: "GET".to_string(),
            path: "/foo".to_string(),
        },
        infigraph_core::graph::CallsServiceEdge {
            symbol_id: "s2".to_string(),
            target_id: "t2".to_string(),
            method: "POST".to_string(),
            path: "/bar".to_string(),
        },
    ];
    infigraph_core::daemon_protocol::write_calls_service_edges_arrow(&edges_path, &edges).unwrap();

    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::WriteCallsServiceEdges {
            edges_path: edges_path.clone(),
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    assert!(
        !edges_path.exists(),
        "sibling edges file should be cleaned up after serving"
    );
    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(result, WriteResult::Ok { .. }),
        "expected Ok, got {result:?}"
    );
}

#[test]
fn serve_one_request_handles_write_cross_service_edges() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-xse.request");
    let result_path = staging_dir.join("test-xse.result");
    let edges_path = staging_dir.join("test-xse.edges.arrow");

    let candidates = vec![
        infigraph_core::graph::CrossServiceEdgeCandidate {
            target_id: "xsvc::payments::GET::/foo".to_string(),
            target_name: "payments GET /foo".to_string(),
            docstring: "External service: payments GET /foo".to_string(),
            caller_symbol_id: "s1".to_string(),
            method: "GET".to_string(),
            path: "/foo".to_string(),
            target_service: "payments".to_string(),
        },
        infigraph_core::graph::CrossServiceEdgeCandidate {
            target_id: "xsvc::billing::POST::/bar".to_string(),
            target_name: "billing POST /bar".to_string(),
            docstring: "External service: billing POST /bar".to_string(),
            caller_symbol_id: "s2".to_string(),
            method: "POST".to_string(),
            path: "/bar".to_string(),
            target_service: "billing".to_string(),
        },
    ];
    infigraph_core::daemon_protocol::write_cross_service_edges_arrow(&edges_path, &candidates)
        .unwrap();

    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::WriteCrossServiceEdges {
            edges_path: edges_path.clone(),
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    assert!(
        !edges_path.exists(),
        "sibling edges file should be cleaned up after serving"
    );
    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(result, WriteResult::Ok { .. }),
        "expected Ok, got {result:?}"
    );
}

#[test]
fn serve_one_request_handles_upsert_dependencies() {
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

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-deps.request");
    let result_path = staging_dir.join("test-deps.result");

    let manifest_result = ManifestResult {
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
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::UpsertDependencies {
            result: manifest_result,
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(result, WriteResult::Ok { .. }),
        "expected Ok, got {result:?}"
    );

    let rows = infigraph
        .backend()
        .unwrap()
        .raw_query("MATCH (d:Dependency) WHERE d.id = 'pypi::requests' RETURN d.id")
        .unwrap();
    assert_eq!(rows.len(), 1, "expected the Dependency node to exist");
}

#[test]
fn serve_one_request_handles_store_clusters() {
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
    let idx_to_id: Vec<String> = symbols.iter().map(|s| s.id.clone()).collect();
    let community: Vec<usize> = vec![0; idx_to_id.len()];

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-clusters.request");
    let result_path = staging_dir.join("test-clusters.result");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::StoreClusters {
            idx_to_id,
            community,
            modularity: 0.5,
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(result, WriteResult::ClustersOk(ref stats) if stats.num_clusters == 1),
        "expected ClustersOk with num_clusters == 1, got {result:?}"
    );

    let rows = infigraph
        .backend()
        .unwrap()
        .raw_query("MATCH (c:Cluster) RETURN c.id")
        .unwrap();
    assert_eq!(rows.len(), 1, "expected the Cluster node to exist");
}

#[test]
fn serve_one_request_handles_store_config_bindings() {
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

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-config-bindings.request");
    let result_path = staging_dir.join("test-config-bindings.result");

    let bindings = vec![ConfigBindingWire {
        symbol_id,
        kind: "EnvVar".to_string(),
        key: "DATABASE_URL".to_string(),
        value: "postgres://...".to_string(),
        profile: "default".to_string(),
        source_file: "main.py".to_string(),
    }];
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::StoreConfigBindings { bindings }).unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(result, WriteResult::Ok { .. }),
        "expected Ok, got {result:?}"
    );

    let rows = infigraph
        .backend()
        .unwrap()
        .raw_query("MATCH (c:ConfigBinding) RETURN c.id")
        .unwrap();
    assert_eq!(rows.len(), 1, "expected the ConfigBinding node to exist");
}

#[test]
fn serve_one_request_handles_derive_tested_by() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n\ndef test_hello():\n    hello()\n",
    )
    .unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    infigraph.index().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-tested-by.request");
    let result_path = staging_dir.join("test-tested-by.result");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::DeriveTestedBy { files: None }).unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    match result {
        WriteResult::Ok { .. } => {}
        WriteResult::Err { message } => panic!("expected Ok, got Err: {message}"),
        other => panic!("unexpected WriteResult for DeriveTestedBy: {other:?}"),
    }
}

fn file_in_graph(infigraph: &Infigraph, file: &str) -> bool {
    !infigraph
        .backend()
        .unwrap()
        .raw_query(&format!("MATCH (f:File) WHERE f.id = '{file}' RETURN f.id"))
        .unwrap()
        .is_empty()
}

/// The three write paths that carry already-parsed `FileExtraction`s.
///
/// Driven with real extractions from a real index rather than hand-built
/// structs, so this also covers the round-trip fidelity the JSON-sibling
/// choice rests on: `FileExtraction` is three nested `Vec`s of structs that
/// themselves carry enums and `Option`s, which is exactly why these do not
/// use the Arrow IPC sibling format the flat edge-writing paths use.
#[test]
fn serve_one_request_handles_the_extraction_carrying_writes() {
    let project_dir = tempfile::tempdir().unwrap();
    // Deliberately a CROSS-file call: `ResolveStats::total_calls` counts
    // calls still dangling after extraction, and a same-file call is already
    // resolved by then (see tests/resolve_calls.rs), so a single-file fixture
    // would report zero and assert nothing.
    std::fs::write(
        project_dir.path().join("helpers.py"),
        "def helper():\n    pass\n",
    )
    .unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "from helpers import helper\n\n\ndef caller():\n    helper()\n",
    )
    .unwrap();

    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    let indexed = infigraph.index().unwrap();
    assert_eq!(
        indexed.extractions.len(),
        2,
        "expected both files to be parsed"
    );

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();

    // RemoveFiles takes the file back out...
    let request_path = staging_dir.join("remove.request");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::RemoveFiles {
            files: vec!["main.py".to_string()],
        })
        .unwrap(),
    )
    .unwrap();
    serve_one_request(&infigraph, &request_path).unwrap();
    assert!(
        !file_in_graph(&infigraph, "main.py"),
        "RemoveFiles must have deleted the File node"
    );

    // ...and UpsertFilesBulk puts it back, from the JSON sibling alone.
    let request_path = staging_dir.join("bulk.request");
    let extractions_path = staging_dir.join("bulk.extractions.json");
    write_extractions_json(&extractions_path, &indexed.extractions).unwrap();
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::UpsertFilesBulk {
            extractions_path: extractions_path.clone(),
            existing_hashes_empty: false,
        })
        .unwrap(),
    )
    .unwrap();
    serve_one_request(&infigraph, &request_path).unwrap();
    assert!(
        file_in_graph(&infigraph, "main.py"),
        "UpsertFilesBulk must have restored the File node from the sibling file"
    );
    assert!(
        !extractions_path.exists(),
        "the handler must clean up the sibling file it consumed"
    );

    // ResolveCalls must report real stats, not `Ok`'s two lossy counters.
    let request_path = staging_dir.join("resolve.request");
    let extractions_path = staging_dir.join("resolve.extractions.json");
    write_extractions_json(&extractions_path, &indexed.extractions).unwrap();
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::ResolveCalls {
            extractions_path,
            use_learned: false,
        })
        .unwrap(),
    )
    .unwrap();
    serve_one_request(&infigraph, &request_path).unwrap();
    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(staging_dir.join("resolve.result")).unwrap())
            .unwrap();
    match result {
        WriteResult::ResolveOk(stats) => assert!(
            stats.total_calls > 0,
            "caller() calls helper(), so resolution must see at least one call: {stats:?}"
        ),
        other => panic!("expected ResolveOk, got {other:?}"),
    }
}
