use infigraph_core::daemon_protocol::{
    serve_one_request, write_atomic, IngestSource, WriteRequest, WriteResult,
};
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
