/// `tool_watch_docs` must also respect the primary/secondary gate --
/// mirrors `tool_watch_project_declines_when_not_primary`.
#[test]
fn tool_watch_docs_declines_when_not_primary() {
    infigraph_mcp::tools::watch::disable_watchers();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    infigraph_docs::DocIndex::open(&root)
        .unwrap()
        .init()
        .unwrap();
    let path = root.to_string_lossy().to_string();

    let args = serde_json::json!({ "path": path });
    let result = infigraph_mcp::tools::docs::tool_watch_docs(&args)
        .expect("must not return an error, only an informative skip message");

    assert!(
        result.to_lowercase().contains("not primary"),
        "expected a not-primary message, got: {result}"
    );
    assert!(!infigraph_mcp::tools::docs::is_doc_watching(
        &path.replace('\\', "/")
    ));
}
