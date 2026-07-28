/// `tool_watch_project` (the explicit MCP tool) must also respect the
/// primary/secondary gate. Before this fix, a secondary instance (one that
/// called `disable_watchers()` at startup because another instance holds
/// `mcp.lock`) would still spin up its own in-process watcher on a direct
/// tool call -- exactly the coordination gap that let two watchers race for
/// the same repo's `index.lock` in a real incident.
///
/// `disable_watchers()` is a one-way, process-lifetime flag with no reset
/// function -- this test lives in its own file (a separate test binary) so
/// its mutation can never leak into another test file's expectations.
#[test]
fn tool_watch_project_declines_when_not_primary() {
    infigraph_mcp::tools::watch::disable_watchers();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let path = root.to_string_lossy().to_string();

    let args = serde_json::json!({ "path": path });
    let result = infigraph_mcp::tools::watch::tool_watch_project(&args)
        .expect("must not return an error, only an informative skip message");

    assert!(
        result.to_lowercase().contains("not primary"),
        "expected a not-primary message, got: {result}"
    );
    assert!(!infigraph_mcp::tools::watch::is_watching(
        &path.replace('\\', "/")
    ));
}
