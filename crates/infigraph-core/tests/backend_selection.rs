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

    // DaemonKuzuBackend::open() opens a real read-only Kuzu connection
    // (Task 12), and a read-only connection cannot create a database --
    // so the graph must already exist on disk before daemon-mode init()
    // can succeed. In production this precondition is met by a real
    // `infigraph daemon` process having already run a normal (writable)
    // init() first; here we simulate that by initializing with the
    // default Kuzu backend and dropping it before switching to daemon
    // mode.
    std::env::remove_var("INFIGRAPH_BACKEND");
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    drop(infigraph);

    // This test is about backend *selection* only. init()'s daemon arm also
    // calls ensure_daemon_running, which would spawn a real detached daemon
    // against this tempdir (and outlive it); INFIGRAPH_NO_WATCH makes that
    // a no-op. The spawn itself is covered by
    // init_daemon_backend_starts_a_daemon in watch_daemon.rs.
    std::env::set_var("INFIGRAPH_NO_WATCH", "1");
    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    let result = infigraph.init();
    std::env::remove_var("INFIGRAPH_BACKEND");
    std::env::remove_var("INFIGRAPH_NO_WATCH");

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
