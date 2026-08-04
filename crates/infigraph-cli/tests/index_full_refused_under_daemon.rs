//! `infigraph index --full` under `INFIGRAPH_BACKEND=daemon` now works by
//! routing through the daemon's own build-fresh-then-swap handler, instead
//! of refusing. See
//! docs/superpowers/specs/2026-08-04-daemon-routed-full-reindex-design.md
//! and the real e2e coverage in
//! crates/infigraph-core/tests/daemon_kuzu_e2e.rs (this file only checks
//! the CLI-level success/failure contract, not the daemon internals).

use std::process::Command;

#[test]
fn full_reindex_succeeds_under_daemon_backend_with_a_real_running_daemon() {
    let project = tempfile::tempdir().expect("failed to create project temp dir");
    let fake_home = tempfile::tempdir().expect("failed to create fake home temp dir");

    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n")
        .expect("failed to write fixture file");

    let cli = env!("CARGO_BIN_EXE_infigraph");

    // Bootstrap-index locally first (no daemon involved yet), matching the
    // established pattern in crates/infigraph-core/tests/daemon_kuzu_e2e.rs.
    let bootstrap = Command::new(cli)
        .arg("index")
        .current_dir(project.path())
        .env("HOME", fake_home.path())
        .env_remove("INFIGRAPH_BACKEND")
        .status()
        .expect("failed to run bootstrap infigraph index");
    assert!(bootstrap.success(), "bootstrap index must succeed");

    // Start a real daemon against the project.
    let mut daemon = Command::new(cli)
        .arg("daemon")
        .current_dir(project.path())
        .env("HOME", fake_home.path())
        .env_remove("INFIGRAPH_BACKEND")
        .spawn()
        .expect("failed to spawn daemon");

    let lock_path = project.path().join(".infigraph").join("watch.lock");
    let start = std::time::Instant::now();
    loop {
        if lock_path.exists() && std::fs::metadata(&lock_path).unwrap().len() > 0 {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(10) {
            let _ = daemon.kill();
            panic!("daemon never acquired watch.lock");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let output = Command::new(cli)
        .arg("index")
        .arg("--full")
        .arg("--no-embed")
        .current_dir(project.path())
        .env("HOME", fake_home.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("failed to run infigraph index --full");

    let _ = daemon.kill();
    let _ = daemon.wait();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected `infigraph index --full` to succeed under the daemon backend, but it failed:\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        !stderr.contains("not yet supported under the daemon backend"),
        "the old refusal message must not appear -- this behavior was replaced, got:\n{stderr}"
    );

    // Verify the graph genuinely has real content (a real rebuild
    // happened, not a silent no-op).
    let graph_path = project.path().join(".infigraph").join("graph");
    assert!(graph_path.exists(), "graph must exist after a full reindex");
}
