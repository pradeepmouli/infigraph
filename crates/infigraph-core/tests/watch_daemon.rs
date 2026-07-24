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
