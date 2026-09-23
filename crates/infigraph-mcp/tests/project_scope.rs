//! #196: a tool call is scoped to its project once, at dispatch. A
//! subdirectory `path` resolves to the project that owns it, an omitted one
//! means the server's own project, and nothing creates `.infigraph/` in a
//! directory just because a call named it.

use infigraph_mcp::tools::helpers::{log_activity, scope_to_project, startup_project};
use serde_json::json;
use std::path::PathBuf;

/// A git repo with a project store at its root, and a source subdirectory.
fn project_with_subdir() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::fs::write(root.join(".infigraph").join("graph"), b"").unwrap();
    let sub = root.join("packages").join("codegen").join("src");
    std::fs::create_dir_all(&sub).unwrap();
    (dir, root, sub)
}

fn path_of(args: &serde_json::Value) -> Option<&str> {
    args.get("path").and_then(|p| p.as_str())
}

#[test]
fn a_subdirectory_path_is_scoped_to_the_project_that_owns_it() {
    let (_dir, root, sub) = project_with_subdir();
    let args = scope_to_project("search", json!({"path": sub, "query": "x"}));
    assert_eq!(path_of(&args), Some(root.to_str().unwrap()));
    assert_eq!(args["query"], "x", "other arguments are untouched");
}

#[test]
fn an_omitted_path_means_the_servers_own_project() {
    let args = scope_to_project("search", json!({"query": "x"}));
    assert_eq!(path_of(&args), Some(startup_project().to_str().unwrap()));
    let dot = scope_to_project("search", json!({"path": ".", "query": "x"}));
    assert_eq!(path_of(&dot), path_of(&args), "`.` means the same project");
}

#[test]
fn a_tool_without_a_path_gets_none() {
    let args = scope_to_project("list_languages", json!({}));
    assert!(path_of(&args).is_none(), "{args}");
}

#[test]
fn delete_project_acts_on_exactly_the_store_it_names() {
    let (_dir, _root, sub) = project_with_subdir();
    let args = scope_to_project("delete_project", json!({"path": sub}));
    assert_eq!(path_of(&args), Some(sub.to_str().unwrap()));
}

#[test]
fn activity_is_recorded_in_the_project_store_never_in_a_new_one() {
    let (_dir, root, sub) = project_with_subdir();

    log_activity("search", &json!({"path": sub, "query": "raw"}));
    assert!(
        !sub.join(".infigraph").exists(),
        "a raw subdirectory path must not become a store"
    );

    log_activity(
        "search",
        &scope_to_project("search", json!({"path": sub, "query": "scoped"})),
    );
    assert!(root.join(".infigraph").join("sessions").is_dir());
    assert!(!sub.join(".infigraph").exists());
}

#[test]
fn path_is_optional_in_every_schema_but_delete_projects() {
    for tool in infigraph_mcp::build_tools_list() {
        let name = tool["name"].as_str().unwrap();
        let required: Vec<&str> = tool["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r.as_str())
            .collect();
        assert_eq!(
            required.contains(&"path"),
            name == "delete_project",
            "{name} requires {required:?}"
        );
    }
}
