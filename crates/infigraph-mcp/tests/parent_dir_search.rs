use serde_json::json;

use infigraph_mcp::tools::index::tool_index_project;
use infigraph_mcp::tools::search::tool_search;

/// Issue #8: Infigraph tools fail when CWD is parent of indexed project directory.
/// Tools should work using the `path` parameter, regardless of CWD.
#[test]
fn test_search_works_from_parent_cwd() {
    // Create a project inside a parent directory
    let parent = tempfile::TempDir::new().expect("parent tmpdir");
    let project_dir = parent.path().join("myproject");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();
    std::fs::write(
        project_dir.join("src/main.py"),
        "def unique_search_target_func():\n    return 42\n",
    )
    .unwrap();

    let project_path = project_dir.to_string_lossy().to_string();

    // Index the project using its absolute path
    tool_index_project(&json!({"path": &project_path})).expect("index should succeed");

    // Verify search works when CWD is the project itself
    let result = tool_search(&json!({"path": &project_path, "query": "unique_search_target_func"}))
        .expect("search from project dir should work");
    assert!(
        result.contains("unique_search_target_func"),
        "should find function when path points to project: {result}"
    );

    // Change CWD to parent directory (the bug scenario)
    let original_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(parent.path()).unwrap();

    // Search using absolute path — should still work
    let result = tool_search(&json!({
        "path": &project_path,
        "query": "unique_search_target_func"
    }));

    // Restore CWD before asserting (so other tests aren't affected)
    std::env::set_current_dir(&original_cwd).unwrap();

    let result = result.expect("search with absolute path should work regardless of CWD");
    assert!(
        result.contains("unique_search_target_func"),
        "should find function from parent CWD with absolute path: {result}"
    );
}

/// Issue #8: Search with relative path "." should use CWD, which may not be the project.
#[test]
fn test_search_with_dot_path_uses_cwd() {
    let parent = tempfile::TempDir::new().expect("parent tmpdir");
    let project_dir = parent.path().join("subproject");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();
    std::fs::write(
        project_dir.join("src/lib.py"),
        "def dot_path_test_func():\n    return 99\n",
    )
    .unwrap();

    let project_path = project_dir.to_string_lossy().to_string();
    tool_index_project(&json!({"path": &project_path})).expect("index");

    // Set CWD to project dir — "." path should work
    let original_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&project_dir).unwrap();

    let result = tool_search(&json!({"path": ".", "query": "dot_path_test_func"}));

    std::env::set_current_dir(&original_cwd).unwrap();

    let result = result.expect("search with '.' should work when CWD is project");
    assert!(
        result.contains("dot_path_test_func"),
        "should find function with '.' path when CWD is project: {result}"
    );
}

/// Issue #8: CWD is parent of indexed project, path is "." — should resolve via registry.
#[test]
fn test_search_dot_path_from_parent_finds_via_registry() {
    let parent = tempfile::TempDir::new().expect("parent tmpdir");
    let project_dir = parent.path().join("child");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();
    std::fs::write(
        project_dir.join("src/app.py"),
        "def registry_resolve_func(): pass\n",
    )
    .unwrap();

    let project_path = project_dir.to_string_lossy().to_string();
    // index_project now registers in global registry
    tool_index_project(&json!({"path": &project_path})).expect("index");

    // Set CWD to parent — "." resolves to parent, no .infigraph there
    let original_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(parent.path()).unwrap();

    // Should find the function via registry fallback
    let result = tool_search(&json!({"path": ".", "query": "registry_resolve_func"}));

    std::env::set_current_dir(&original_cwd).unwrap();

    let result = result.expect("search should resolve child project via registry");
    assert!(
        result.contains("registry_resolve_func"),
        "should find function from parent CWD via registry resolution: {result}"
    );
}
