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
///
/// Whether `resolve_cli_binary_sibling_of` actually *finds* the `infigraph`
/// CLI binary next to this test binary is environment-dependent: it lives
/// in `target/debug/deps/` (one level below `target/debug/`, where the real
/// sibling binaries are), and a grandparent-directory fallback was added so
/// it CAN be found there once `infigraph-cli`'s binary has been built into
/// `target/debug/` (e.g. by a full-workspace `cargo test` invocation that
/// also builds `-p infigraph-cli`). Either outcome — spawn succeeds or the
/// binary genuinely isn't there yet — is acceptable; this test only asserts
/// the one invariant that must hold in both cases: daemon mode never
/// populates the in-process WATCHERS map. When a real daemon does spawn, it
/// is cleaned up before returning so it doesn't leak into other tests.
#[test]
fn daemon_mode_on_does_not_populate_in_process_watchers_map() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "1");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let path = root.to_string_lossy().to_string();

    let result = infigraph_mcp::tools::watch::auto_start_watch(&path);
    assert!(!infigraph_mcp::tools::watch::is_watching(
        &path.replace('\\', "/")
    ));

    if let Some(msg) = result {
        // The CLI binary was found and a real detached daemon spawned —
        // stop it via the same path-based stop mechanism a real caller
        // would use, and wait for the lock to be released, so this test
        // doesn't leave an orphaned watch process or a held lock behind.
        assert!(
            msg.contains("Daemon watcher started"),
            "unexpected auto_start_watch outcome: {msg}"
        );
        let args = serde_json::json!({ "path": path });
        let _ = infigraph_mcp::tools::watch::tool_stop_watch(&args);
        wait_for_watch_locks_released(std::slice::from_ref(&path));
    }

    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
}

/// `tool_watch_project` (the explicit MCP tool a client can call directly,
/// not just the opportunistic auto-start wrapper) must ALSO respect the
/// daemon-mode toggle. Before this fix, only `auto_start_watch_inner`
/// checked `watch_daemon_mode_enabled()` -- a direct `watch_project` call
/// bypassed the toggle entirely and always created an in-process watcher,
/// even with daemon mode on. This caused a real incident: a daemon watcher
/// was correctly running for a repo, but a direct tool call on the MCP
/// worker also started a competing in-process watcher that later wedged,
/// holding `.infigraph/index.lock` indefinitely.
#[test]
fn tool_watch_project_respects_daemon_mode_toggle() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "1");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let path = root.to_string_lossy().to_string();

    let args = serde_json::json!({ "path": path });
    let result = infigraph_mcp::tools::watch::tool_watch_project(&args);

    assert!(
        !infigraph_mcp::tools::watch::is_watching(&path.replace('\\', "/")),
        "daemon mode must never populate the in-process WATCHERS map, even on a direct tool_watch_project call"
    );

    if let Ok(msg) = &result {
        assert!(
            msg.contains("Daemon watcher"),
            "unexpected tool_watch_project outcome under daemon mode: {msg}"
        );
        let _ = infigraph_mcp::tools::watch::tool_stop_watch(&args);
        wait_for_watch_locks_released(std::slice::from_ref(&path));
    }

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

/// Writes a `LockInfo` JSON payload directly into `.infigraph/watch.lock`
/// (bypassing `try_acquire`/`LockFile`) without ever holding the flock —
/// simulating the stale payload left behind after an unclean watcher death
/// (SIGKILL/OOM/crash), where the flock releases automatically but the
/// JSON identity payload survives because `Drop` never ran to clear it.
fn write_stale_lock_payload(lock_path: &std::path::Path) {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let stale = infigraph_core::lockfile::LockInfo {
        pid: 999_999,
        role: "test-stale".to_string(),
        build_hash: "test".to_string(),
        acquired_at: 0,
    };
    std::fs::write(lock_path, serde_json::to_string(&stale).unwrap()).unwrap();
}

/// C2 regression test: a stale `LockInfo` payload with no live flock holder
/// must NOT be reported as an active watcher. Fails against the pre-fix
/// implementation, which used `read_holder` alone as the liveness signal
/// and would report "Watcher active ... Held by PID 999999".
#[test]
fn get_watch_status_by_path_ignores_stale_payload_without_live_flock() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let lock_path = root.join(".infigraph").join("watch.lock");
    write_stale_lock_payload(&lock_path);

    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::watch::tool_get_watch_status(&args).unwrap();
    assert!(
        result.starts_with("No watcher running for"),
        "expected stale payload to be treated as no watcher, got: {result}"
    );
}

/// C1 regression test: a stale `LockInfo` payload with no live flock holder
/// must NOT cause `tool_stop_watch` to write a stop sentinel — nothing is
/// running, so there is nothing to stop, and writing the sentinel anyway
/// would poison the next legitimately-spawned watcher (its first loop
/// iteration would see the stale sentinel and exit within ~1s). Fails
/// against the pre-fix implementation, which short-circuited on
/// `read_holder(...).is_none()` being false and fell through to writing
/// the sentinel.
#[test]
fn stop_watch_by_path_ignores_stale_payload_without_live_flock() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let lock_path = root.join(".infigraph").join("watch.lock");
    write_stale_lock_payload(&lock_path);

    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::watch::tool_stop_watch(&args).unwrap();
    assert_eq!(result, "No watcher running.");

    let sentinel = root.join(".infigraph").join("watch.stop");
    assert!(
        !sentinel.exists(),
        "stop sentinel must not be written for a dead watcher with a stale payload"
    );
}

/// `auto_start_doc_watch` (the doc-side opportunistic auto-start, mirroring
/// `auto_start_watch`'s existing daemon-mode test above) must ALSO route
/// through the shared daemon primitive when the toggle is on, instead of
/// its current unconditional in-process-thread behavior.
#[test]
fn auto_start_doc_watch_respects_daemon_mode_toggle() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "1");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    infigraph_docs::DocIndex::open(&root)
        .unwrap()
        .init()
        .unwrap();
    let path = root.to_string_lossy().to_string();

    let result = infigraph_mcp::tools::docs::auto_start_doc_watch(&path);

    assert!(
        !infigraph_mcp::tools::docs::is_doc_watching(&path.replace('\\', "/")),
        "daemon mode must never populate the in-process DOC_WATCHERS map"
    );

    if let Some(msg) = result {
        assert!(
            msg.contains("Daemon watcher"),
            "unexpected auto_start_doc_watch outcome under daemon mode: {msg}"
        );
    }

    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
}

/// `tool_watch_docs` (the explicit MCP tool) must also respect the daemon
/// toggle -- mirrors `tool_watch_project_respects_daemon_mode_toggle` above.
#[test]
fn tool_watch_docs_respects_daemon_mode_toggle() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "1");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    infigraph_docs::DocIndex::open(&root)
        .unwrap()
        .init()
        .unwrap();
    let path = root.to_string_lossy().to_string();

    let args = serde_json::json!({ "path": path });
    let result = infigraph_mcp::tools::docs::tool_watch_docs(&args);

    assert!(!infigraph_mcp::tools::docs::is_doc_watching(
        &path.replace('\\', "/")
    ));

    if let Ok(msg) = &result {
        assert!(
            msg.contains("Daemon watcher"),
            "unexpected tool_watch_docs outcome under daemon mode: {msg}"
        );
    }

    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
}

/// `tool_stop_watch_docs` must accept a `path` argument (today it only
/// accepts `watcher_id`, which is meaningless in daemon mode since there is
/// no in-process DOC_WATCHERS entry to look up) and, when the shared daemon
/// is alive, write `.infigraph/watch.stop.docs`.
#[test]
fn stop_watch_docs_by_path_writes_sentinel_when_daemon_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let ig = root.join(".infigraph");
    std::fs::create_dir_all(&ig).unwrap();
    infigraph_docs::DocIndex::open(&root)
        .unwrap()
        .init()
        .unwrap();

    // Simulate a live daemon: hold watch.lock for the duration of this test.
    let lock_path = ig.join("watch.lock");
    let _held = infigraph_core::lockfile::try_acquire(&lock_path, "cli-watch")
        .unwrap()
        .expect("free");

    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::docs::tool_stop_watch_docs(&args).unwrap();

    assert!(
        result.to_lowercase().contains("stop"),
        "expected a stop-signal message, got: {result}"
    );
    assert!(
        ig.join("watch.stop.docs").exists(),
        "must write the docs-specific stop sentinel while the shared daemon is alive"
    );
}

#[test]
fn stop_watch_docs_by_path_reports_no_watcher_when_lock_free() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();

    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::docs::tool_stop_watch_docs(&args).unwrap();

    assert!(
        result.to_lowercase().contains("no"),
        "expected a no-watcher message, got: {result}"
    );
    assert!(!root.join(".infigraph").join("watch.stop.docs").exists());
}
