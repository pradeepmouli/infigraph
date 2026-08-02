use std::process::Command;
use std::time::Duration;

use infigraph_core::graph::GraphBackend;

/// RAII guard that kills and reaps a spawned child process on drop, so a
/// panic anywhere in this test (not just the happy path) can't leave a real
/// `infigraph daemon` process running forever against an abandoned tempdir.
/// `std::process::Child` does not do this itself -- Drop just closes the
/// parent's handle, the child keeps running independently. Mirrors the
/// `KillOnDrop` guard in `crates/infigraph-cli/tests/watch_daemon_docs.rs`
/// (kept as its own small copy here since that's a different crate).
struct KillOnDrop(std::process::Child);

impl std::ops::Deref for KillOnDrop {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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

    // infigraph-core has no dev-dependency on infigraph-cli, so
    // `env!("CARGO_BIN_EXE_infigraph")` isn't available here (cargo only
    // sets that var for a test binary's own crate-graph binaries). Resolve
    // the CLI binary the same way Task 2's watch_daemon.rs test does.
    let cli_binary = infigraph_core::watch::daemon::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    )
    .expect("infigraph CLI binary must already be built (shared target dir)");

    // Bootstrap: index once directly (BackendKind::Kuzu, no daemon
    // involved) so .infigraph/ exists before starting the daemon.
    let status = Command::new(&cli_binary)
        .arg("index")
        .current_dir(project_dir.path())
        .env_remove("INFIGRAPH_BACKEND")
        .status()
        .unwrap();
    assert!(status.success(), "bootstrap index failed");

    // Start the real daemon as a detached child, guarded so a panic
    // anywhere below still kills it.
    let mut daemon = KillOnDrop(
        Command::new(&cli_binary)
            .arg("daemon")
            .current_dir(project_dir.path())
            .env_remove("INFIGRAPH_BACKEND")
            .spawn()
            .unwrap(),
    );

    // Wait for it to acquire watch.lock (proves it started successfully).
    let lock_path = project_dir.path().join(".infigraph").join("watch.lock");
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

    // A DaemonKuzu-backed client submits a write request against the same
    // project the real daemon process is watching.
    let registry = infigraph_languages::bundled_registry().unwrap();
    let mut client = infigraph_core::Infigraph::open(project_dir.path(), registry).unwrap();
    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let init_result = client.init();
    std::env::remove_var("INFIGRAPH_BACKEND");
    init_result.unwrap();

    std::fs::write(
        project_dir.path().join("second.py"),
        "def another():\n    pass\n",
    )
    .unwrap();

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

    // Verify via a fresh, independent read-only connection (not the
    // client's own wrapper) that the real daemon process actually
    // performed the write. Using `KuzuBackend::open_read_only` directly
    // (the same primitive `DaemonKuzuBackend` uses internally) instead of
    // a second direct writable `Infigraph`/`Kuzu` open, which would
    // collide with the daemon's own open -- exactly the problem this
    // whole design exists to prevent.
    let verify = infigraph_core::graph::KuzuBackend::open_read_only(
        &project_dir.path().join(".infigraph").join("graph"),
    )
    .unwrap();
    let rows = verify
        .raw_query("MATCH (c:ConfigBinding {key: 'E2E_TEST_KEY'}) RETURN c.key")
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "expected the daemon to have created the ConfigBinding node"
    );

    // Clean up: stop the daemon via its sentinel file.
    std::fs::write(project_dir.path().join(".infigraph").join("watch.stop"), "").unwrap();
    let stop_start = std::time::Instant::now();
    loop {
        if let Ok(Some(_)) = daemon.try_wait() {
            break;
        }
        if stop_start.elapsed() > Duration::from_secs(5) {
            let _ = daemon.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
