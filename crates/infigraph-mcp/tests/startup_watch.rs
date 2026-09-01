//! Coverage for proactive startup watching
//! (`infigraph_mcp::recovery::start_daemon_watcher_for_startup_dir`):
//! when the MCP server becomes primary under daemon mode
//! (`INFIGRAPH_BACKEND=daemon`), the directory it was launched in must
//! start being watched immediately, without waiting for some future write
//! to trigger `auto_start_watch` reactively — and the new
//! `[watch].auto_start_on_boot` / `INFIGRAPH_WATCH_AUTO_START` toggle must
//! actually suppress that effect when turned off, not just exist as an
//! unused config knob. Deliberately scoped to just the startup directory,
//! not the whole project registry -- an earlier version swept the registry
//! plus the groups dir and started a daemon for every registered project
//! regardless of whether this server instance was serving it.
//!
//! `start_daemon_watcher_for_startup_dir` lives in the library crate
//! (`infigraph-mcp/src/recovery.rs`), not in `main.rs`, specifically so it's
//! reachable from here — `main.rs` compiles to a separate `[[bin]]` target
//! with no unit-test history of its own (see the doc comment on that
//! function for the full rationale). It is synchronous by design (unlike
//! `main.rs`'s thin wrapper, which spawns a thread around it), so this test
//! can observe its effect deterministically.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Serializes tests that mutate process-global env vars, mirroring
/// `watcher_daemon_mode.rs`'s `ENV_LOCK` (not reusable across integration
/// test binaries, since each `tests/*.rs` file compiles to its own crate).
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn wait_for_watch_lock_state(
    lock_path: &std::path::Path,
    want_held: bool,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let held = infigraph_core::lockfile::try_acquire(lock_path, "test-probe")
            .map(|g| g.is_none())
            .unwrap_or(false);
        if held == want_held {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Given `INFIGRAPH_BACKEND=daemon` and a startup directory that has never
/// had a write, `start_daemon_watcher_for_startup_dir` starts watching
/// it on its own — proving proactive startup watching actually works, not
/// just that some helper exists. Folds in the negative case in the same
/// test (same setup, same lock-file observable): with
/// `INFIGRAPH_WATCH_AUTO_START=0`, the identical call must NOT spawn a
/// daemon — the config toggle must really suppress the effect.
#[test]
fn start_daemon_watcher_for_startup_dir_respects_boot_toggle() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // This test needs a real `infigraph` CLI binary to spawn (daemon-mode
    // watching re-execs it) — skip rather than fail if this test binary was
    // built without it, matching `watch_daemon.rs`'s
    // `init_daemon_backend_starts_a_daemon` precedent (infigraph-mcp has no
    // dev-dependency on infigraph-cli, so cargo won't build it for us).
    let Ok(_cli) = infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    ) else {
        eprintln!("skipping: infigraph CLI binary not built in this target dir");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let lock_path = root.join(".infigraph").join("watch.lock");

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");

    // --- Negative case: boot toggle explicitly off. ---
    std::env::set_var("INFIGRAPH_WATCH_AUTO_START", "0");
    infigraph_mcp::recovery::start_daemon_watcher_for_startup_dir(Some(&root));
    assert!(
        !wait_for_watch_lock_state(&lock_path, true, Duration::from_millis(800)),
        "INFIGRAPH_WATCH_AUTO_START=0 must suppress proactive startup watching, \
         but a daemon acquired watch.lock anyway"
    );

    // --- Positive case: boot toggle on (explicit, not relying on default \
    // resolution against whatever config.toml might be discoverable from \
    // this test binary's cwd). ---
    std::env::set_var("INFIGRAPH_WATCH_AUTO_START", "1");
    infigraph_mcp::recovery::start_daemon_watcher_for_startup_dir(Some(&root));
    let started = wait_for_watch_lock_state(&lock_path, true, Duration::from_secs(15));

    // Clean up unconditionally before asserting, so a failed assertion can't
    // leak a daemon process or a held lock.
    std::fs::write(root.join(".infigraph").join("watch.stop"), "").unwrap();
    wait_for_watch_lock_state(&lock_path, false, Duration::from_secs(15));

    std::env::remove_var("INFIGRAPH_WATCH_AUTO_START");
    std::env::remove_var("INFIGRAPH_BACKEND");

    assert!(
        started,
        "expected start_daemon_watcher_for_startup_dir to start a daemon \
         watching the startup directory when daemon mode + the boot toggle are both on"
    );
}

/// `start_daemon_watcher_for_startup_dir` must also respect the persisted
/// `[watch].enabled` policy (`INFIGRAPH_WATCH_ENABLED`), distinct from the
/// `[watch].auto_start_on_boot` toggle covered above -- closes Task 12's
/// stubbed `write_watch_policy_to_config` so `infigraph watch disable`
/// actually suppresses proactive startup watching, not just explicit
/// `infigraph watch` invocations.
#[test]
fn start_daemon_watcher_for_startup_dir_respects_watch_enabled_policy() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let Ok(_cli) = infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    ) else {
        eprintln!("skipping: infigraph CLI binary not built in this target dir");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let lock_path = root.join(".infigraph").join("watch.lock");

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    std::env::set_var("INFIGRAPH_WATCH_AUTO_START", "1");
    std::env::set_var("INFIGRAPH_WATCH_ENABLED", "0");

    infigraph_mcp::recovery::start_daemon_watcher_for_startup_dir(Some(&root));
    let suppressed = !wait_for_watch_lock_state(&lock_path, true, Duration::from_millis(800));

    std::env::remove_var("INFIGRAPH_WATCH_ENABLED");
    std::env::remove_var("INFIGRAPH_WATCH_AUTO_START");
    std::env::remove_var("INFIGRAPH_BACKEND");

    assert!(
        suppressed,
        "INFIGRAPH_WATCH_ENABLED=0 must suppress proactive startup watching, \
         but a daemon acquired watch.lock anyway"
    );
}

/// Regression coverage for the scope-narrowing fix: a *different* directory
/// that also has a real `.infigraph` (simulating some other registered
/// project) must NOT get a daemon started for it just because
/// `start_daemon_watcher_for_startup_dir` was called for an unrelated
/// `startup_dir`. An earlier version swept the whole registry and started a
/// daemon for every entry it found, which was a real bug -- this locks in
/// that only the passed-in `startup_dir` is ever touched.
#[test]
fn start_daemon_watcher_for_startup_dir_never_touches_other_projects() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let Ok(_cli) = infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    ) else {
        eprintln!("skipping: infigraph CLI binary not built in this target dir");
        return;
    };

    let startup_tmp = tempfile::tempdir().unwrap();
    let startup_root = startup_tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(startup_root.join(".infigraph")).unwrap();
    let startup_lock = startup_root.join(".infigraph").join("watch.lock");

    let other_tmp = tempfile::tempdir().unwrap();
    let other_root = other_tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(other_root.join(".infigraph")).unwrap();
    let other_lock = other_root.join(".infigraph").join("watch.lock");

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    std::env::set_var("INFIGRAPH_WATCH_AUTO_START", "1");

    infigraph_mcp::recovery::start_daemon_watcher_for_startup_dir(Some(&startup_root));
    let started = wait_for_watch_lock_state(&startup_lock, true, Duration::from_secs(15));

    let other_untouched = !wait_for_watch_lock_state(&other_lock, true, Duration::from_millis(800));

    std::fs::write(startup_root.join(".infigraph").join("watch.stop"), "").unwrap();
    wait_for_watch_lock_state(&startup_lock, false, Duration::from_secs(15));

    std::env::remove_var("INFIGRAPH_WATCH_AUTO_START");
    std::env::remove_var("INFIGRAPH_BACKEND");

    assert!(
        started,
        "expected a daemon to start for the actual startup_dir"
    );
    assert!(
        other_untouched,
        "a daemon was started for `other_root`, which was never passed as startup_dir -- \
         proactive startup watching must not sweep unrelated projects"
    );
}

/// True-up: a project that drifted while nothing was watching it (e.g. a
/// file was added between MCP server restarts) must be caught by
/// `start_daemon_watcher_for_startup_dir` before it starts watching, not
/// silently missed until some future fsevent happens to touch that exact
/// file again. Before this fix, the function only ever started the watcher
/// and relied entirely on future filesystem events -- drift accumulated
/// while unwatched was invisible until something else touched it.
#[test]
fn start_daemon_watcher_for_startup_dir_catches_drift_from_before_it_was_running() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let Ok(_cli) = infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    ) else {
        eprintln!("skipping: infigraph CLI binary not built in this target dir");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("main.py"), "def main(): pass\n").unwrap();
    let path_str = root.to_string_lossy().to_string();

    let lock_path = root.join(".infigraph").join("watch.lock");

    // Bootstrap: index once so `.infigraph/` is a real, already-indexed
    // project (not just an empty directory), matching a project that was
    // indexed in a prior session. Without `INFIGRAPH_BACKEND=daemon` set
    // yet, this also auto-starts an ordinary in-process watcher -- stop it
    // and wait for `watch.lock` to be released, so it doesn't sit there
    // holding the lock and starving the real daemon spawn below (the true-up
    // step's write would otherwise be dropped into `.infigraph/requests/`
    // with nothing -- no real `infigraph daemon` process -- ever polling
    // that directory to serve it, hanging until its timeout).
    infigraph_mcp::tools::index::tool_index_project(&serde_json::json!({ "path": &path_str }))
        .expect("initial index");
    std::fs::write(root.join(".infigraph").join("watch.stop"), "").unwrap();
    wait_for_watch_lock_state(&lock_path, false, Duration::from_secs(15));

    // Nothing is watching yet -- simulate drift accumulated between MCP
    // server runs by adding a file with no watcher alive to see it.
    std::fs::write(
        root.join("drifted.py"),
        "def drifted_during_downtime(): return 'late'\n",
    )
    .unwrap();

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    std::env::set_var("INFIGRAPH_WATCH_AUTO_START", "1");

    infigraph_mcp::recovery::start_daemon_watcher_for_startup_dir(Some(&root));
    let started = wait_for_watch_lock_state(&lock_path, true, Duration::from_secs(15));

    let hashes = infigraph_mcp::tools::helpers::open_prism_read_only(&serde_json::json!({
        "path": &path_str
    }))
    .ok()
    .and_then(|prism| prism.backend().and_then(|b| b.get_file_hashes().ok()));
    let drifted_file_indexed = hashes
        .map(|h| h.keys().any(|k| k.ends_with("drifted.py")))
        .unwrap_or(false);

    // Clean up unconditionally before asserting, so a failed assertion
    // can't leak a daemon process or a held lock.
    std::fs::write(root.join(".infigraph").join("watch.stop"), "").unwrap();
    wait_for_watch_lock_state(&lock_path, false, Duration::from_secs(15));

    std::env::remove_var("INFIGRAPH_WATCH_AUTO_START");
    std::env::remove_var("INFIGRAPH_BACKEND");

    assert!(started, "expected a daemon to start for the startup dir");
    assert!(
        drifted_file_indexed,
        "a file added while nothing was watching must be caught by the true-up \
         index before the watcher starts, not left invisible until some future \
         fsevent happens to touch it"
    );
}

/// `auto_start_watch_on_boot_enabled` itself: env var overrides config file
/// overrides the hardcoded default (on), matching the priority convention
/// documented on `get_ml_compression_mode` in the same file.
#[test]
fn auto_start_watch_on_boot_enabled_env_override_priority() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    std::env::set_var("INFIGRAPH_WATCH_AUTO_START", "0");
    assert!(!infigraph_mcp::session_context::auto_start_watch_on_boot_enabled());

    std::env::set_var("INFIGRAPH_WATCH_AUTO_START", "false");
    assert!(!infigraph_mcp::session_context::auto_start_watch_on_boot_enabled());

    std::env::set_var("INFIGRAPH_WATCH_AUTO_START", "1");
    assert!(infigraph_mcp::session_context::auto_start_watch_on_boot_enabled());

    std::env::remove_var("INFIGRAPH_WATCH_AUTO_START");
}
