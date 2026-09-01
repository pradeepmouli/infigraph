use std::sync::Mutex;

// INFIGRAPH_BACKEND is a process-wide env var; serialize tests that set it
// so they don't race each other under cargo's default parallel test runner.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn defaults_to_kuzu_when_unset() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert_eq!(infigraph_core::selected_backend(), "kuzu");
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
