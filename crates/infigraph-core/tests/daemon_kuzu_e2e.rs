use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use infigraph_core::graph::GraphBackend;
use infigraph_core::Infigraph;

/// `INFIGRAPH_BACKEND` is process-wide, so the set/init/remove window in
/// `open_daemon_client` must not overlap between the tests in this binary --
/// otherwise one test's `remove_var` lands inside another's window and that
/// client silently opens a direct Kuzu backend instead of DaemonKuzu.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that kills and reaps a spawned child process on drop, so a
/// panic anywhere in this test (not just the happy path) can't leave a real
/// `infigraph daemon` process running forever against an abandoned tempdir.
/// `std::process::Child` does not do this itself -- Drop just closes the
/// parent's handle, the child keeps running independently. Mirrors the
/// `KillOnDrop` guard in `crates/infigraph-cli/tests/watch_daemon_docs.rs`
/// (kept as its own small copy here since that's a different crate).
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// infigraph-core has no dev-dependency on infigraph-cli, so
/// `env!("CARGO_BIN_EXE_infigraph")` isn't available here (cargo only sets
/// that var for a test binary's own crate-graph binaries). Resolve the CLI
/// binary the same way Task 2's watch_daemon.rs test does.
fn cli_binary() -> PathBuf {
    infigraph_core::watch::daemon::resolve_cli_binary_sibling_of(&std::env::current_exe().unwrap())
        .expect("infigraph CLI binary must already be built (shared target dir)")
}

/// Bootstrap-index `project_dir` directly (BackendKind::Kuzu, no daemon
/// involved, so `.infigraph/` exists), then start a real detached
/// `infigraph daemon` against it and wait until it holds `watch.lock`.
fn start_real_daemon(project_dir: &Path) -> KillOnDrop {
    let cli = cli_binary();

    let status = Command::new(&cli)
        .arg("index")
        .current_dir(project_dir)
        .env_remove("INFIGRAPH_BACKEND")
        .status()
        .unwrap();
    assert!(status.success(), "bootstrap index failed");

    let daemon = KillOnDrop(
        Command::new(&cli)
            .arg("daemon")
            .current_dir(project_dir)
            .env_remove("INFIGRAPH_BACKEND")
            .spawn()
            .unwrap(),
    );

    let lock_path = project_dir.join(".infigraph").join("watch.lock");
    let start = std::time::Instant::now();
    loop {
        if lock_path.exists() && std::fs::metadata(&lock_path).unwrap().len() > 0 {
            break;
        }
        if start.elapsed() > Duration::from_secs(10) {
            panic!("daemon never acquired watch.lock");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon
}

/// Open a `DaemonKuzu`-backed client against `project_dir`. `init()`'s own
/// `ensure_daemon_running` call resolves to `AlreadyRunning` here, since
/// `start_real_daemon` already holds the lock.
fn open_daemon_client(project_dir: &Path) -> Infigraph {
    let registry = infigraph_languages::bundled_registry().unwrap();
    let mut client = Infigraph::open(project_dir, registry).unwrap();
    let init_result = {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_BACKEND", "daemon");
        let r = client.init();
        std::env::remove_var("INFIGRAPH_BACKEND");
        r
    };
    init_result.unwrap();
    client
}

/// Stop the daemon via its sentinel file, falling back to a kill.
fn stop_daemon(project_dir: &Path, daemon: &mut KillOnDrop) {
    std::fs::write(project_dir.join(".infigraph").join("watch.stop"), "").unwrap();
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(_)) = daemon.0.try_wait() {
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = daemon.0.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Open a fresh, independent read-only connection to verify what actually
/// landed on disk -- deliberately NOT the client's own wrapper, so a passing
/// assertion can't be explained by client-side caching. Uses
/// `KuzuBackend::open_read_only` (the same primitive `DaemonKuzuBackend`
/// uses internally) rather than a second writable open, which would collide
/// with the daemon's -- exactly what this design exists to prevent.
fn verify_conn(project_dir: &Path) -> infigraph_core::graph::KuzuBackend {
    infigraph_core::graph::KuzuBackend::open_read_only(
        &project_dir.join(".infigraph").join("graph"),
    )
    .unwrap()
}

/// End-to-end proof that a real spawned `infigraph daemon` process and a
/// real `DaemonKuzu`-backed client `Infigraph` genuinely interoperate --
/// not just the in-process simulated-server pattern (a background thread
/// calling `serve_one_request` directly) that earlier tasks' tests used.
#[test]
fn real_daemon_process_serves_a_daemon_kuzu_client() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    let mut daemon = start_real_daemon(project_dir.path());
    let client = open_daemon_client(project_dir.path());

    let backend = client.backend().unwrap();
    // `upsert_repo` (the brief's original choice) turned out to be a bad
    // fit: `GraphBackend::upsert_repo`'s trait default is a *documented,
    // intentional* no-op for Kuzu ("only meaningful for Neo4j multi-repo
    // graphs" -- see backend.rs) and `KuzuBackend` never overrides it. So
    // routing it through the daemon proves nothing about Kuzu writes --
    // the daemon's own local `KuzuBackend::upsert_repo` call is a genuine
    // no-op by design, not a wiring gap. `store_config_bindings` has no
    // such trap: the trait requires every backend to implement it (see
    // its "No default impl" doc comment, added specifically so a
    // raw_query-based default couldn't be silently inherited by this
    // wrapper), and `KuzuBackend`'s implementation unconditionally
    // `CREATE`s a `ConfigBinding` node -- a real, observable Kuzu write.
    let bindings = vec![infigraph_core::config::ConfigBindingWire {
        symbol_id: "e2e-test::hello".to_string(),
        kind: "env".to_string(),
        key: "E2E_TEST_KEY".to_string(),
        value: "e2e-test-value".to_string(),
        profile: "default".to_string(),
        source_file: "main.py".to_string(),
    }];
    backend.store_config_bindings(&bindings).unwrap();

    let rows = verify_conn(project_dir.path())
        .raw_query("MATCH (c:ConfigBinding {key: 'E2E_TEST_KEY'}) RETURN c.key")
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "expected the daemon to have created the ConfigBinding node"
    );

    stop_daemon(project_dir.path(), &mut daemon);
}

/// `Infigraph::index()`/`index_files()` must work under `BackendKind::
/// DaemonKuzu`. They can't use the client-side path -- `upsert_files_bulk`,
/// `remove_file` and `resolve_calls` are all Tier-3 stubs there -- so they
/// route the whole operation to the daemon as a `WriteRequest::Index`.
/// Before that routing existed, `index()` with changed files failed with
/// "use Infigraph::index()/index_files() instead", i.e. it told the caller
/// to call the function they were already inside.
#[test]
fn index_and_index_files_route_through_the_daemon() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    let mut daemon = start_real_daemon(project_dir.path());
    let client = open_daemon_client(project_dir.path());

    // A file the bootstrap index never saw, so there is real work to do.
    std::fs::write(
        project_dir.path().join("second.py"),
        "def another():\n    pass\n",
    )
    .unwrap();

    let full = client.index().unwrap();
    assert!(
        full.total_files >= 2,
        "index() must report the daemon's real file count, got total_files={}",
        full.total_files
    );

    // `indexed_files` is deliberately not asserted on: the daemon is also a
    // watcher, so it may already have picked `second.py` up on its own,
    // making the count legitimately 0. `total_files` for a scoped call is
    // exactly `paths.len()` on the daemon side, so that stays deterministic.
    let scoped = client.index_files(&[PathBuf::from("second.py")]).unwrap();
    assert_eq!(
        scoped.total_files, 1,
        "index_files() must report the daemon's real result, got total_files={}",
        scoped.total_files
    );

    let rows = verify_conn(project_dir.path())
        .raw_query("MATCH (f:File) WHERE f.id = 'second.py' RETURN f.id")
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the daemon must actually have indexed second.py into the graph"
    );

    stop_daemon(project_dir.path(), &mut daemon);
}

/// Read-after-write through the SAME long-lived wrapper instance.
///
/// `DaemonKuzuBackend` opens one read-only `KuzuBackend` at construction and
/// reuses it for every read. Every other test here verifies a daemon-side
/// write through a *fresh* connection (deliberately, to rule out client-side
/// caching), which leaves the question this test answers untested: does a
/// commit made by the daemon process become visible to an already-open
/// read-only connection in another process, without reopening it?
///
/// This matters for any code path that reads via the wrapper and writes via
/// the daemon within one process -- e.g. `detect_config_bindings`,
/// `detect_clusters`, `link_cross_service_calls`.
///
/// KNOWN FAILING -- documents an unresolved architectural limitation, which
/// is why it is `#[ignore]`d rather than deleted or weakened. The answer is
/// NO: a Kuzu embedded read-only `Database` serves the snapshot it loaded at
/// open time and does not observe another process's later commits. The
/// assertion below is written as the behavior we *want*; running
/// `cargo test -p infigraph-core --test daemon_kuzu_e2e -- --ignored` shows
/// it failing with the independent-connection check above it passing, i.e.
/// the write commits but the long-lived connection can't see it.
///
/// Deliberately NOT fixed here by reopening `read_conn` inside the wrapper's
/// read methods -- that is a real design change (when to reopen, what it
/// costs per read, whether reads should be `&mut self`) that needs its own
/// consideration, not a silent patch smuggled into a review-fix round.
#[test]
#[ignore = "known limitation: daemon commits are invisible to an already-open read-only connection"]
fn daemon_write_is_visible_through_the_same_wrapper_instance() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    let mut daemon = start_real_daemon(project_dir.path());
    let client = open_daemon_client(project_dir.path());
    let backend = client.backend().unwrap();

    // Prove the key isn't already present, so the post-write read can't
    // pass for the wrong reason.
    let before = backend
        .raw_query("MATCH (c:ConfigBinding {key: 'SAME_INSTANCE_KEY'}) RETURN c.key")
        .unwrap();
    assert_eq!(before.len(), 0, "key must not exist before the write");

    backend
        .store_config_bindings(&[infigraph_core::config::ConfigBindingWire {
            symbol_id: "e2e-test::hello".to_string(),
            kind: "env".to_string(),
            key: "SAME_INSTANCE_KEY".to_string(),
            value: "same-instance-value".to_string(),
            profile: "default".to_string(),
            source_file: "main.py".to_string(),
        }])
        .unwrap();

    // An independent connection confirms the write really committed, so a
    // failure below is unambiguously a visibility problem on the long-lived
    // connection and not a write that never happened.
    let independent = verify_conn(project_dir.path())
        .raw_query("MATCH (c:ConfigBinding {key: 'SAME_INSTANCE_KEY'}) RETURN c.key")
        .unwrap();
    assert_eq!(
        independent.len(),
        1,
        "the daemon must have committed the write"
    );

    // The actual question: same wrapper instance, same connection, no reopen.
    let same_instance = backend
        .raw_query("MATCH (c:ConfigBinding {key: 'SAME_INSTANCE_KEY'}) RETURN c.key")
        .unwrap();

    stop_daemon(project_dir.path(), &mut daemon);

    assert_eq!(
        same_instance.len(),
        1,
        "a daemon-committed write must be visible through the same long-lived \
         DaemonKuzuBackend read connection without reopening it"
    );
}
