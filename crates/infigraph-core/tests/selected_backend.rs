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
        infigraph_core::BackendChoice::Daemon
    );
    assert_eq!(infigraph_core::DAEMON_BACKEND, "daemon");
}

#[test]
fn reads_real_env_var_name_unchanged() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "neo4j");
    assert_eq!(
        infigraph_core::selected_backend(),
        infigraph_core::BackendChoice::Neo4j
    );
    std::env::remove_var("INFIGRAPH_BACKEND");
}

#[test]
fn daemon_backend_selected_matches_selected_backend() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    assert!(infigraph_core::daemon_backend_selected());
    assert_eq!(
        infigraph_core::selected_backend(),
        infigraph_core::BackendChoice::Daemon
    );
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

#[test]
fn a_typo_is_rejected_by_validation_naming_the_variable_and_choices() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "kuzuu");
    let err = infigraph_core::validated_backend().unwrap_err().to_string();
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert!(
        err.contains("INFIGRAPH_BACKEND") && err.contains("\"kuzuu\""),
        "{err}"
    );
    assert!(err.contains("kuzu, daemon, neo4j"), "{err}");
}

#[test]
fn empty_backend_is_an_error_not_the_default() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "");
    let result = infigraph_core::validated_backend();
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert!(
        result.is_err(),
        "empty INFIGRAPH_BACKEND must not mean the default"
    );
}

/// #74's I-6: an unknown value used to reach `init`'s catch-all arm and
/// open local Kuzu. The match is exhaustive now, and `init` validates first.
#[test]
fn init_refuses_an_unknown_backend_instead_of_opening_kuzu() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("INFIGRAPH_BACKEND", "kuzuu");
    let registry = infigraph_languages::bundled_registry().unwrap();
    let mut ig = infigraph_core::Infigraph::open(tmp.path(), registry).unwrap();
    let result = ig.init();
    std::env::set_var(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND);
    assert!(
        result.is_err(),
        "unknown backend must not fall through to Kuzu"
    );
    assert!(
        !tmp.path().join(".infigraph/graph").exists(),
        "no graph file may be created"
    );
}
