use std::sync::Mutex;
use std::time::{Duration, Instant};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Stop every in-process watcher and block until each one's
/// `.infigraph/watch.lock` is confirmed released. Mirrors the helper of the
/// same shape in `watcher_reindex.rs` (not reusable across integration test
/// binaries, since each `tests/*.rs` file compiles to its own crate) — kept
/// in sync deliberately rather than factored into a shared `tests/common`
/// module, matching this test suite's existing convention.
fn stop_all_watchers() {
    let mut guard = infigraph_mcp::tools::watch::get_watchers();
    let stopped_paths: Vec<String> = if let Some(map) = guard.as_mut() {
        let ids: Vec<String> = map.keys().cloned().collect();
        let mut paths = Vec::new();
        for id in ids {
            if let Some(entry) = map.remove(&id) {
                paths.push(entry.path.clone());
                let _ = entry.stop_tx.send(());
            }
        }
        paths
    } else {
        Vec::new()
    };
    drop(guard);
    wait_for_watch_locks_released(&stopped_paths);
}

fn wait_for_watch_locks_released(paths: &[String]) {
    use fs2::FileExt;
    for path in paths {
        let lock_path = std::path::Path::new(path)
            .join(".infigraph")
            .join("watch.lock");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let file = match std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
            {
                Ok(f) => f,
                Err(_) => break,
            };
            match file.try_lock_exclusive() {
                Ok(()) => {
                    let _ = file.unlock();
                    break;
                }
                Err(_) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

/// With the toggle OFF (default), auto_start_watch must behave exactly as
/// before: an in-process thread, tracked in the WATCHERS map, with no
/// external process spawned. This is the regression guard for "toggle
/// defaults to unchanged behavior".
#[test]
fn daemon_mode_off_by_default_uses_in_process_thread() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();

    let path = root.to_string_lossy().to_string();
    let result = infigraph_mcp::tools::watch::auto_start_watch(&path);
    assert!(result.is_some(), "expected in-process watcher to start");
    assert!(infigraph_mcp::tools::watch::is_watching(
        &path.replace('\\', "/")
    ));

    stop_all_watchers();
}

/// With the toggle ON, auto_start_watch must NOT create an in-process
/// thread — is_watching() (which only reflects the in-process WATCHERS
/// map) must stay false, since the watcher is meant to live in a separate
/// daemon process instead.
#[test]
fn daemon_mode_on_does_not_populate_in_process_watchers_map() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "1");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let path = root.to_string_lossy().to_string();

    // No infigraph CLI binary sibling exists next to the test binary, so
    // this exercises the "could not locate infigraph CLI binary" failure
    // path — proving daemon mode was taken (not the in-process path) even
    // though the actual spawn can't succeed in this test environment.
    let result = infigraph_mcp::tools::watch::auto_start_watch(&path);
    assert!(result.is_none());
    assert!(!infigraph_mcp::tools::watch::is_watching(
        &path.replace('\\', "/")
    ));

    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
}

#[test]
fn stop_watch_by_path_reports_no_watcher_when_none_running() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::watch::tool_stop_watch(&args).unwrap();
    assert_eq!(result, "No watcher running.");
}

#[test]
fn get_watch_status_by_path_reports_no_watcher_when_none_running() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::watch::tool_get_watch_status(&args).unwrap();
    assert!(result.starts_with("No watcher running for"));
}

#[test]
fn get_watch_status_by_path_reports_holder_identity_when_lock_held() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let lock_path = root.join(".infigraph").join("watch.lock");
    let _held = infigraph_core::lockfile::try_acquire(&lock_path, "test-daemon")
        .unwrap()
        .unwrap();

    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::watch::tool_get_watch_status(&args).unwrap();
    assert!(result.contains("role: test-daemon"));
}
