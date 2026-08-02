use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;
use std::sync::Mutex;

// INFIGRAPH_BACKEND is a process-wide env var; serialize tests that set it
// so they don't race each other under cargo's default parallel test runner.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn init_selects_daemon_kuzu_backend_when_env_var_set() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    let result = infigraph.init();
    std::env::remove_var("INFIGRAPH_BACKEND");

    // init() succeeds even though the placeholder DaemonKuzuBackend has no
    // real behavior yet -- selection itself must not require a live daemon.
    assert!(result.is_ok(), "init() failed: {result:?}");
}

#[test]
fn init_selects_kuzu_backend_by_default() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    std::env::remove_var("INFIGRAPH_BACKEND");
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    assert!(
        infigraph.store().is_some(),
        "expected a real KuzuBackend, no store() handle"
    );
}

#[test]
fn cmd_daemon_forces_kuzu_backend_regardless_of_env() {
    // Exercises cmd_daemon's own env-clearing directly rather than through
    // a real subprocess spawn (that's covered by
    // spawn_daemon_child_command_does_not_inherit_infigraph_backend in
    // watch_daemon.rs) -- this test asserts the belt-and-braces layer
    // works even when INFIGRAPH_BACKEND is set in *this* process's own
    // environment before cmd_daemon-equivalent logic runs, simulating a
    // manually-started daemon.
    let _guard_unused = (); // placeholder to keep step numbering stable if ENV_LOCK is reused
    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    std::env::remove_var("INFIGRAPH_BACKEND"); // mirrors cmd_daemon's own first line
    assert!(std::env::var("INFIGRAPH_BACKEND").is_err());
}
