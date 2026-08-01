use infigraph_core::daemon_protocol::{serve_one_request, write_atomic, WriteRequest, WriteResult};
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
        WriteResult::Err { message } => panic!("expected Ok, got Err: {message}"),
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
