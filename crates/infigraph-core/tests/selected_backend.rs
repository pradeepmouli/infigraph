use std::sync::Mutex;

// INFIGRAPH_BACKEND is a process-wide env var; serialize tests that set it
// so they don't race each other under cargo's default parallel test runner.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The one assertion that defines the default. #159: reads and writes route
/// through a daemon unless a caller says otherwise, so that a project cannot
/// end up with some processes routing and others opening the graph file
/// directly -- a split that is invisible without `lsof` and that produced a
/// 16h-old MCP worker reading one repo's graph directly while its daemon
/// held the same file.
///
/// Everything that needs the local backend now says so explicitly (see
/// `LOCAL_BACKEND`); this is deliberately the only place left where the
/// absence of the variable is what is under test.
#[test]
fn defaults_to_daemon_when_unset() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert_eq!(
        infigraph_core::selected_backend(),
        infigraph_core::DAEMON_BACKEND
    );
}

#[test]
fn reads_real_env_var_name_unchanged() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "neo4j");
    assert_eq!(infigraph_core::selected_backend(), "neo4j");
    std::env::remove_var("INFIGRAPH_BACKEND");
}

#[test]
fn daemon_backend_selected_matches_selected_backend() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    assert!(infigraph_core::daemon_backend_selected());
    assert_eq!(infigraph_core::selected_backend(), "daemon");
    std::env::remove_var("INFIGRAPH_BACKEND");
}

#[test]
fn is_remote_backend_matches_selected_backend() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "neo4j");
    assert!(infigraph_core::daemon::lifecycle::is_remote_backend());
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert!(!infigraph_core::daemon::lifecycle::is_remote_backend());
}
