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
    std::env::set_var(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND);
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
    std::env::set_var(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND);
    std::env::remove_var("INFIGRAPH_NO_WATCH");

    assert!(result.is_ok(), "init() failed: {result:?}");
}

/// The counterpart to `init_selects_daemon_kuzu_backend_when_env_var_set`:
/// both selections are now asserted explicitly, and neither depends on
/// which one happens to be the default.
///
/// This used to be `init_selects_kuzu_backend_by_default` and leant on the
/// variable being absent. #159 flipped that default, and the assertion that
/// defines it now lives in exactly one place --
/// `selected_backend.rs::defaults_to_daemon_when_unset`. Restoring an
/// unset-means-local assumption here would also make this test spawn a real
/// daemon against a tempdir, since `init`'s daemon arm auto-starts one.
#[test]
fn init_selects_kuzu_backend_when_pinned_local() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    std::env::set_var(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND);
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    assert!(
        infigraph.store().is_some(),
        "expected a real KuzuBackend, no store() handle"
    );
}

/// #159: with routing as the default, a project that has never been indexed
/// has to be able to bootstrap itself.
///
/// It could not. `DaemonKuzuBackend::open` takes a *read-only* Kùzu
/// connection and a read-only connection cannot create a database, so
/// daemon-mode `init()` structurally requires the graph to already exist --
/// which a fresh project can never reach through the routed path.
/// `ensure_daemon_running_required` compounded it by treating "no
/// `.infigraph/` yet" as `AlreadyRunning`, so nothing was spawned and the
/// 10s readiness wait then failed. The observed result was `infigraph index`
/// on a new project dying after 10s with a message claiming
/// `INFIGRAPH_BACKEND=daemon is set` when the user had set nothing at all.
#[test]
fn init_bootstraps_a_never_indexed_project_under_the_default_backend() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    // The default, stated rather than assumed, so this test keeps testing
    // bootstrap rather than silently becoming a local-mode test.
    std::env::set_var(infigraph_core::BACKEND_ENV, infigraph_core::DAEMON_BACKEND);
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    let result = infigraph.init();
    std::env::set_var(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND);

    assert!(
        result.is_ok(),
        "a never-indexed project must be able to bootstrap under the routed default, \
         got: {result:?}"
    );
    assert!(
        project_dir.path().join(".infigraph").join("graph").exists(),
        "bootstrap must leave a real graph on disk for the daemon to serve"
    );
}
