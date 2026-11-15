//! The root notice through the real dispatcher: a call with no `path`, served by
//! a worker started in a project that has a linked worktree, says which project
//! answered once per period; a call that names `path` says nothing; `get_stats`
//! names its project on its first line. One test, because the server's start
//! project is fixed for the process by its first use.

use std::process::Command;

use serde_json::{json, Value};

mod support;

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn text(reply: &Value) -> String {
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text in {reply}"))
        .to_string()
}

fn call(id: i64, tool: &str, args: Value) -> String {
    let request = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": tool, "arguments": args}});
    text(&infigraph_mcp::handle_tools_call(&json!(id), &request))
}

#[test]
fn a_call_with_no_path_says_which_project_answered_once() {
    support::disable_background_watchers();
    std::env::set_var(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND);
    let project =
        support::TestProject::with_files(&[("closure.ts", "export function f() { return 1; }\n")]);
    let root = project.path().canonicalize().unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    let linked = root.with_file_name(format!(
        "{}-wt",
        root.file_name().unwrap().to_string_lossy()
    ));
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "notice-wt",
            linked.to_str().unwrap(),
        ],
    );

    let cli = infigraph_mcp::tools::helpers::find_infigraph_cli().unwrap();
    let index = Command::new(cli)
        .args(["index", "--no-embed"])
        .current_dir(&root)
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .output()
        .unwrap();
    assert!(index.status.success(), "{index:?}");

    // The server's start project is whatever the process starts in.
    std::env::set_current_dir(&root).unwrap();

    let marker = "answered for";
    let first = call(1, "search", json!({"query": "f"}));
    assert!(first.contains(marker), "first unnamed call: {first}");
    assert!(first.contains("1 linked worktree,"), "{first}");
    assert!(first.contains(root.to_str().unwrap()), "{first}");

    let second = call(2, "search", json!({"query": "f"}));
    assert!(!second.contains(marker), "within the period: {second}");

    // A call that names the project says nothing.
    let named = call(3, "search", json!({"query": "f", "path": root}));
    assert!(!named.contains(marker), "{named}");

    // get_stats names its project on its first line, every time.
    for id in [4, 5] {
        let stats = call(id, "get_stats", json!({}));
        assert_eq!(
            stats.lines().next().unwrap(),
            format!("project: {}", root.display()),
            "{stats}"
        );
        assert!(stats.contains("linked worktrees: 1"), "{stats}");
    }
}
