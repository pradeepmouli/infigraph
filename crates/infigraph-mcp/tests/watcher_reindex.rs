use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::json;

use infigraph_mcp::tools::docs::{
    init_doc_watchers, tool_index_docs, tool_search_docs, tool_watch_docs, DOC_WATCHERS,
};
use infigraph_mcp::tools::index::tool_index_project;
use infigraph_mcp::tools::search::tool_search;
use infigraph_mcp::tools::watch::*;

static WATCHER_LOCK: Mutex<()> = Mutex::new(());

fn make_project(files: &[(&str, &str)]) -> (tempfile::TempDir, String) {
    let dir = tempfile::TempDir::new().expect("tmpdir");
    for (name, content) in files {
        let p = dir.path().join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
    }
    let path = dir.path().to_string_lossy().to_string();
    (dir, path)
}

fn stop_all_watchers() {
    let mut guard = get_watchers();
    if let Some(map) = guard.as_mut() {
        let ids: Vec<String> = map.keys().cloned().collect();
        for id in ids {
            if let Some(entry) = map.remove(&id) {
                let _ = entry.stop_tx.send(());
            }
        }
    }
}

fn poll_until<F: Fn() -> bool>(check: F, timeout: Duration, desc: &str) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!("poll_until timed out: {desc}");
    false
}

/// Modify an existing file in an existing directory — watcher should detect and reindex.
#[test]
fn test_code_watcher_reindexes_modified_file() {
    let _guard = WATCHER_LOCK.lock().unwrap();
    stop_all_watchers();
    init_watchers();

    let (_dir, path) = make_project(&[("lib.py", "def original(): return 1")]);

    tool_index_project(&json!({"path": &path})).expect("initial index");

    // Verify original function is searchable
    let result = tool_search(&json!({"path": &path, "query": "original"})).unwrap();
    assert!(
        result.contains("original"),
        "original should be searchable: {result}"
    );

    // Stop auto-watcher from index, start explicit one with short debounce
    stop_all_watchers();
    std::thread::sleep(Duration::from_millis(200));

    let result = tool_watch_project(&json!({
        "path": &path,
        "auto_resolve": true,
        "debounce_ms": 200
    }))
    .unwrap();
    assert!(
        result.contains("Watcher started"),
        "watcher should start: {result}"
    );

    // Modify file — add a new function
    std::thread::sleep(Duration::from_millis(500));
    let lib_path = std::path::PathBuf::from(&path).join("lib.py");
    std::fs::write(
        &lib_path,
        "def original(): return 1\n\ndef brand_new_function(): return 42\n",
    )
    .unwrap();

    // Poll until the new function is searchable
    let found = poll_until(
        || {
            tool_search(&json!({"path": &path, "query": "brand_new_function"}))
                .map(|r| r.contains("brand_new_function"))
                .unwrap_or(false)
        },
        Duration::from_secs(15),
        "brand_new_function should be searchable after watcher reindex",
    );

    stop_all_watchers();
    assert!(
        found,
        "watcher should have reindexed modified file — brand_new_function not found"
    );
}

/// Create a new file in an existing directory — watcher should detect and reindex.
#[test]
fn test_code_watcher_reindexes_new_file_existing_dir() {
    let _guard = WATCHER_LOCK.lock().unwrap();
    stop_all_watchers();
    init_watchers();

    let (_dir, path) = make_project(&[("src/main.py", "def main(): pass")]);

    tool_index_project(&json!({"path": &path})).expect("initial index");
    stop_all_watchers();
    std::thread::sleep(Duration::from_millis(200));

    tool_watch_project(&json!({
        "path": &path,
        "auto_resolve": true,
        "debounce_ms": 200
    }))
    .unwrap();

    // Create new file in existing src/ dir
    std::thread::sleep(Duration::from_millis(500));
    let new_file = std::path::PathBuf::from(&path).join("src/utils.py");
    std::fs::write(&new_file, "def helper_util(): return 'help'\n").unwrap();

    let found = poll_until(
        || {
            tool_search(&json!({"path": &path, "query": "helper_util"}))
                .map(|r| r.contains("helper_util"))
                .unwrap_or(false)
        },
        Duration::from_secs(15),
        "helper_util should be searchable after watcher reindex",
    );

    stop_all_watchers();
    assert!(
        found,
        "watcher should have reindexed new file in existing dir"
    );
}

/// Create a new file in a NEW directory — watcher should detect and reindex.
/// This is the branch-switch scenario where new dirs appear.
#[test]
fn test_code_watcher_reindexes_new_file_new_dir() {
    let _guard = WATCHER_LOCK.lock().unwrap();
    stop_all_watchers();
    init_watchers();

    let (_dir, path) = make_project(&[("src/main.py", "def main(): pass")]);

    tool_index_project(&json!({"path": &path})).expect("initial index");
    stop_all_watchers();
    std::thread::sleep(Duration::from_millis(200));

    tool_watch_project(&json!({
        "path": &path,
        "auto_resolve": true,
        "debounce_ms": 200
    }))
    .unwrap();

    // Create new directory + file (simulates branch switch adding new module)
    std::thread::sleep(Duration::from_millis(500));
    let new_dir = std::path::PathBuf::from(&path).join("newmodule");
    std::fs::create_dir_all(&new_dir).unwrap();
    std::fs::write(
        new_dir.join("feature.py"),
        "def new_feature(): return 'branch-b'\n",
    )
    .unwrap();

    let found = poll_until(
        || {
            tool_search(&json!({"path": &path, "query": "new_feature"}))
                .map(|r| r.contains("new_feature"))
                .unwrap_or(false)
        },
        Duration::from_secs(15),
        "new_feature should be searchable after watcher reindex",
    );

    stop_all_watchers();
    assert!(
        found,
        "watcher should have reindexed new file in new dir — branch switch scenario"
    );
}

fn stop_all_doc_watchers() {
    let mut guard = DOC_WATCHERS.lock().unwrap();
    if let Some(map) = guard.as_mut() {
        let ids: Vec<String> = map.keys().cloned().collect();
        for id in ids {
            if let Some(entry) = map.remove(&id) {
                let _ = entry.stop_tx.send(());
            }
        }
    }
}

/// Doc watcher should detect new .md files and reindex them.
#[test]
fn test_doc_watcher_reindexes_new_doc() {
    let _guard = WATCHER_LOCK.lock().unwrap();
    stop_all_watchers();
    stop_all_doc_watchers();
    init_doc_watchers();

    let (_dir, path) = make_project(&[("docs/readme.md", "# Hello\n\nThis is the readme.")]);

    // Initial doc index
    let result = tool_index_docs(&json!({"path": &path})).expect("initial doc index");
    eprintln!("initial index: {result}");

    // Verify initial doc is searchable
    let result = tool_search_docs(&json!({"path": &path, "query": "readme hello"})).unwrap();
    assert!(
        result.contains("readme") || result.contains("Hello"),
        "initial doc should be searchable: {result}"
    );

    // Start doc watcher with short debounce
    let result = tool_watch_docs(&json!({"path": &path, "debounce_ms": 500})).unwrap();
    eprintln!("watch_docs: {result}");
    assert!(
        result.contains("Document watcher started"),
        "watcher should start: {result}"
    );

    // Add a new doc file
    std::thread::sleep(Duration::from_millis(500));
    let new_doc = std::path::PathBuf::from(&path).join("docs/guide.md");
    std::fs::write(
        &new_doc,
        "# Unique Guide\n\nThis document contains xylophone_zebra_unicorn content.\n",
    )
    .unwrap();
    eprintln!("wrote new doc: {}", new_doc.display());

    // Poll until the new doc is searchable
    let found = poll_until(
        || {
            tool_search_docs(&json!({"path": &path, "query": "xylophone_zebra_unicorn"}))
                .map(|r| {
                    let has = r.contains("xylophone_zebra_unicorn") || r.contains("Unique Guide");
                    if !has {
                        eprintln!("search_docs result: {r}");
                    }
                    has
                })
                .unwrap_or(false)
        },
        Duration::from_secs(15),
        "xylophone_zebra_unicorn should be searchable after doc watcher reindex",
    );

    stop_all_doc_watchers();
    assert!(found, "doc watcher should have reindexed new document");
}

/// Doc watcher without concurrent readers — isolates whether WAL error is from concurrency.
#[test]
fn test_doc_watcher_reindexes_no_concurrent_read() {
    let _guard = WATCHER_LOCK.lock().unwrap();
    stop_all_watchers();
    stop_all_doc_watchers();
    init_doc_watchers();

    let (_dir, path) = make_project(&[("docs/readme.md", "# Hello\n\nOriginal readme.")]);

    tool_index_docs(&json!({"path": &path})).expect("initial doc index");

    let result = tool_watch_docs(&json!({"path": &path, "debounce_ms": 500})).unwrap();
    eprintln!("watch_docs: {result}");

    // Add new doc
    std::thread::sleep(Duration::from_millis(500));
    let new_doc = std::path::PathBuf::from(&path).join("docs/noconcurrent.md");
    std::fs::write(
        &new_doc,
        "# No Concurrent\n\nContent: alpha_beta_gamma_unique.\n",
    )
    .unwrap();
    eprintln!("wrote new doc (no concurrent read)");

    // Wait for watcher to reindex WITHOUT polling search
    std::thread::sleep(Duration::from_secs(5));

    // Now do ONE search
    stop_all_doc_watchers();
    std::thread::sleep(Duration::from_millis(200));

    let result =
        tool_search_docs(&json!({"path": &path, "query": "alpha_beta_gamma_unique"})).unwrap();
    eprintln!("search result: {result}");
    assert!(
        result.contains("alpha_beta_gamma") || result.contains("No Concurrent"),
        "doc watcher should have reindexed without concurrent readers: {result}"
    );
}

/// Simulate branch switch: modify existing files, add new files in existing dirs.
#[test]
fn test_code_watcher_branch_switch_existing_dirs() {
    let _guard = WATCHER_LOCK.lock().unwrap();
    stop_all_watchers();
    init_watchers();

    // "Branch A" state
    let (_dir, path) = make_project(&[
        ("src/main.py", "def main_a(): pass"),
        ("src/lib.py", "def lib_a(): pass"),
    ]);

    tool_index_project(&json!({"path": &path})).expect("initial index");
    stop_all_watchers();
    std::thread::sleep(Duration::from_millis(200));

    tool_watch_project(&json!({
        "path": &path,
        "auto_resolve": true,
        "debounce_ms": 200
    }))
    .unwrap();

    // Simulate "git checkout branch-b" — modify existing + add new in same dir
    std::thread::sleep(Duration::from_millis(500));
    let src = std::path::PathBuf::from(&path).join("src");
    std::fs::write(src.join("main.py"), "def main_b(): return 'branch-b'\n").unwrap();
    std::fs::write(src.join("lib.py"), "def lib_b(): return 'branch-b'\n").unwrap();
    std::fs::write(src.join("extra.py"), "def extra_branch_b(): return 99\n").unwrap();

    // All three should be searchable
    let found_main = poll_until(
        || {
            tool_search(&json!({"path": &path, "query": "main_b"}))
                .map(|r| r.contains("main_b"))
                .unwrap_or(false)
        },
        Duration::from_secs(15),
        "main_b searchable",
    );
    let found_extra = poll_until(
        || {
            tool_search(&json!({"path": &path, "query": "extra_branch_b"}))
                .map(|r| r.contains("extra_branch_b"))
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "extra_branch_b searchable",
    );

    stop_all_watchers();
    assert!(
        found_main,
        "modified file main_b should be searchable after branch switch"
    );
    assert!(
        found_extra,
        "new file extra_branch_b should be searchable after branch switch"
    );
}

/// Simulate branch switch with NEW directories (new module added on branch B).
#[test]
fn test_code_watcher_branch_switch_new_dirs() {
    let _guard = WATCHER_LOCK.lock().unwrap();
    stop_all_watchers();
    init_watchers();

    // "Branch A" — only has src/
    let (_dir, path) = make_project(&[("src/main.py", "def main_a(): pass")]);

    tool_index_project(&json!({"path": &path})).expect("initial index");
    stop_all_watchers();
    std::thread::sleep(Duration::from_millis(200));

    tool_watch_project(&json!({
        "path": &path,
        "auto_resolve": true,
        "debounce_ms": 200
    }))
    .unwrap();

    // Simulate "git checkout branch-b" — adds new top-level module dir
    std::thread::sleep(Duration::from_millis(500));
    let new_module = std::path::PathBuf::from(&path).join("new_module");
    std::fs::create_dir_all(&new_module).unwrap();
    std::fs::write(
        new_module.join("feature.py"),
        "def branch_b_feature(): return 'new'\n",
    )
    .unwrap();

    // Also add nested new dir
    let nested = new_module.join("sub");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("deep.py"),
        "def deeply_nested_func(): return 'deep'\n",
    )
    .unwrap();

    let found_feature = poll_until(
        || {
            tool_search(&json!({"path": &path, "query": "branch_b_feature"}))
                .map(|r| r.contains("branch_b_feature"))
                .unwrap_or(false)
        },
        Duration::from_secs(15),
        "branch_b_feature in new dir",
    );

    let found_deep = poll_until(
        || {
            tool_search(&json!({"path": &path, "query": "deeply_nested_func"}))
                .map(|r| r.contains("deeply_nested_func"))
                .unwrap_or(false)
        },
        Duration::from_secs(10),
        "deeply_nested_func in nested new dir",
    );

    stop_all_watchers();
    assert!(
        found_feature,
        "file in new dir should be searchable after branch switch"
    );
    assert!(
        found_deep,
        "file in nested new dir should be searchable after branch switch"
    );
}
