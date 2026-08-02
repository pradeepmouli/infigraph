use infigraph_core::watch::daemon::{is_ci_env, is_remote_backend, watch_daemon_mode_enabled};

/// Serializes tests that mutate process-global env vars — cargo runs this
/// binary's tests on parallel threads, so a lowered override in one test
/// must not leak into another test's window (same lesson as PR6's lockfile
/// tests and PR5's idle-decision tests).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn is_remote_backend_only_true_for_explicit_neo4j() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert!(!is_remote_backend());
    std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
    assert!(!is_remote_backend());
    std::env::set_var("INFIGRAPH_BACKEND", "neo4j");
    assert!(is_remote_backend());
    std::env::remove_var("INFIGRAPH_BACKEND");
}

#[test]
fn is_ci_env_detects_any_known_ci_var() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for v in [
        "CI",
        "GITHUB_ACTIONS",
        "JENKINS_URL",
        "BUILDKITE",
        "GITLAB_CI",
        "INFIGRAPH_NO_WATCH",
    ] {
        std::env::remove_var(v);
    }
    assert!(!is_ci_env());
    std::env::set_var("INFIGRAPH_NO_WATCH", "1");
    assert!(is_ci_env());
    std::env::remove_var("INFIGRAPH_NO_WATCH");
}

#[test]
fn watch_daemon_mode_is_opt_in_off_by_default() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
    assert!(!watch_daemon_mode_enabled());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "1");
    assert!(watch_daemon_mode_enabled());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "0");
    assert!(!watch_daemon_mode_enabled());
    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
}

#[test]
fn ensure_daemon_running_noops_under_ci() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("CI", "1");
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();
    let outcome = infigraph_core::watch::daemon::ensure_daemon_running(
        tmp.path(),
        std::path::Path::new("/nonexistent/infigraph"),
    );
    assert_eq!(
        outcome,
        infigraph_core::watch::daemon::DaemonStartOutcome::AlreadyRunning
    );
    std::env::remove_var("CI");
}

/// A project that hasn't been indexed yet (no `.infigraph` at `root`) is a
/// benign precondition-not-met state, not a failure — e.g. the CLI calls
/// this on every command dispatch, including the very first `infigraph
/// index` on a fresh project, before `.infigraph` is created. Callers
/// (`infigraph-cli::index::ensure_watcher_running`) surface `Failed` as a
/// visible "Failed to start watcher" stderr message, so this case must stay
/// `AlreadyRunning` (silent) rather than `Failed`, or ordinary first-time
/// use prints a spurious failure line. See task-3-review.md Finding 1.
#[test]
fn ensure_daemon_running_noops_when_not_yet_indexed() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for v in [
        "CI",
        "GITHUB_ACTIONS",
        "JENKINS_URL",
        "BUILDKITE",
        "GITLAB_CI",
        "INFIGRAPH_NO_WATCH",
        "INFIGRAPH_BACKEND",
    ] {
        std::env::remove_var(v);
    }
    let tmp = tempfile::tempdir().unwrap();
    assert!(!tmp.path().join(".infigraph").exists());

    let outcome = infigraph_core::watch::daemon::ensure_daemon_running(
        tmp.path(),
        std::path::Path::new("/nonexistent/infigraph"),
    );
    assert_eq!(
        outcome,
        infigraph_core::watch::daemon::DaemonStartOutcome::AlreadyRunning,
        "not-yet-indexed projects must no-op silently, not report Failed"
    );
}

/// Direct, deterministic assertion on the `Command` that `spawn_daemon`
/// builds (via the `pub` `build_daemon_command` helper): its env mutations
/// must include an explicit removal of `INFIGRAPH_BACKEND`, regardless of
/// what leaked into the *test's own* environment. `Command::get_envs()`
/// (stable since Rust 1.57) iterates a command's explicit env mutations,
/// where a removed var appears as `(key, None)` — so this proves
/// `env_remove("INFIGRAPH_BACKEND")` was actually applied to the command
/// that will be exec'd. Delete that call from `build_daemon_command` and
/// this assertion fails; keep it and the test passes — no timing, no OS
/// tool dependency, no reliance on a placeholder backend panicking.
#[test]
fn build_daemon_command_strips_infigraph_backend_env_var() {
    let project_dir = tempfile::tempdir().unwrap();
    let tg_dir = project_dir.path().join(".infigraph");
    std::fs::create_dir_all(&tg_dir).unwrap();

    let cmd = infigraph_core::watch::daemon::build_daemon_command(
        project_dir.path(),
        &tg_dir,
        std::path::Path::new("/nonexistent/infigraph"),
    );

    let removed = cmd
        .get_envs()
        .any(|(key, value)| key == "INFIGRAPH_BACKEND" && value.is_none());
    assert!(
        removed,
        "expected build_daemon_command's Command to explicitly remove INFIGRAPH_BACKEND from its env"
    );
}

/// Sanity check that a daemon spawned via `ensure_daemon_running` still
/// starts up and acquires `watch.lock` end-to-end, even with
/// `INFIGRAPH_BACKEND=daemon` leaked into the *test's own* environment.
/// This supplements (does not replace) the deterministic
/// `build_daemon_command_strips_infigraph_backend_env_var` assertion above:
/// lock acquisition in `cmd_daemon` happens before backend selection is
/// even reached, so this alone can't prove the env-stripping fix works —
/// it only proves the daemon still functions normally.
#[test]
#[cfg(unix)]
fn spawn_daemon_child_still_starts_with_infigraph_backend_leaked_into_test_env() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project_dir.path().join(".infigraph")).unwrap();

    // infigraph-core has no dev-dependency on infigraph-cli, so
    // `env!("CARGO_BIN_EXE_infigraph")` isn't available here (cargo only
    // sets that var for a test binary's own crate-graph binaries). Fall
    // back to the same sibling-binary resolution MCP uses, matching the
    // pattern in crates/infigraph-cli/tests/watch_daemon_docs.rs's
    // `cli_binary` helper.
    let cli_binary = infigraph_core::watch::daemon::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    )
    .expect("infigraph CLI binary must already be built (shared target dir)");

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let outcome =
        infigraph_core::watch::daemon::ensure_daemon_running(project_dir.path(), &cli_binary);
    std::env::remove_var("INFIGRAPH_BACKEND");

    assert_eq!(
        outcome,
        infigraph_core::watch::daemon::DaemonStartOutcome::Spawned,
        "expected the daemon to spawn successfully despite INFIGRAPH_BACKEND=daemon in this test's own env"
    );

    // Give the child a moment to acquire watch.lock.
    let lock_path = project_dir.path().join(".infigraph").join("watch.lock");
    let start = std::time::Instant::now();
    loop {
        if lock_path.exists() && std::fs::metadata(&lock_path).unwrap().len() > 0 {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("daemon never acquired watch.lock");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Clean up: signal the spawned daemon to stop.
    std::fs::write(project_dir.path().join(".infigraph").join("watch.stop"), "").unwrap();
}
