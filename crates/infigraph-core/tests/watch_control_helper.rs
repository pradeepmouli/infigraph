//! Integration test for the `Infigraph::submit_watch_control_and_await`
//! client-side helper (Task 11). Tests that the helper correctly submits
//! a WatchControl request against a real running daemon and returns success.
//!
//! Harness pattern (real detached child process, stderr/stdout drained,
//! `KillOnDrop` cleanup) mirrors `crates/infigraph-core/tests/watch_control.rs`.

use infigraph_core::daemon_protocol::{WatchAction, WatchRole};
use infigraph_core::lang::LanguageRegistry;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;
use std::time::Instant;

fn cli_binary() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    let deps_dir = exe.parent().unwrap();
    let candidate = deps_dir.join("infigraph");
    if candidate.exists() {
        return candidate;
    }
    deps_dir.parent().unwrap().join("infigraph")
}

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

fn still_running(child: &mut std::process::Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

const REPLY_TIMEOUT: Duration = Duration::from_secs(180);

#[test]
fn submit_watch_control_and_await_reply_round_trips_against_a_real_daemon() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "skipping: infigraph CLI binary not found at {} (needs a full `cargo build`/`cargo test --workspace` first)",
            bin.display()
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let watched = root.join("main.py");
    std::fs::write(&watched, "def test_fn():\n    pass\n").unwrap();

    let mut child = KillOnDrop(
        Command::new(&bin)
            .arg("daemon")
            .arg("--debounce")
            .arg("50")
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn infigraph daemon"),
    );

    // Drain stdout/stderr to prevent the daemon from blocking on full pipes.
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            eprintln!("[daemon stdout] {line}");
        }
    });
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[daemon stderr] {line}");
        }
    });

    // Wait for the daemon to become ready.
    let lock_path = root.join(".infigraph").join("watch.lock");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !infigraph_core::daemon::lifecycle::daemon_is_alive(&lock_path) {
        assert!(
            Instant::now() < deadline,
            "daemon never became alive (watch.lock never showed a live holder)"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // Create an Infigraph instance pointing to the daemon's root.
    let registry = LanguageRegistry::new();
    let ig = infigraph_core::Infigraph::open(&root, registry)
        .expect("failed to create Infigraph instance");

    // Call the new helper to stop code-watching.
    let result =
        ig.submit_watch_control_and_await(WatchRole::Code, WatchAction::Stop, REPLY_TIMEOUT);
    assert!(
        result.is_ok(),
        "submit_watch_control_and_await must return success for WatchControl {{ Code, Stop }}: {result:?}"
    );

    // Verify the daemon is still running (the whole point of the split).
    assert!(
        still_running(&mut child),
        "the daemon process exited after WatchControl stop -- watch lifecycle and process lifecycle are conflated"
    );

    // Verify cleanup: the request and result files should be gone.
    let requests_dir = root.join(".infigraph").join("requests");
    let entries: Vec<_> = std::fs::read_dir(&requests_dir)
        .map(|rd| rd.collect())
        .unwrap_or_default();
    assert!(
        entries.is_empty(),
        "request/result files were not cleaned up by submit_write_request: {:?}",
        entries
            .into_iter()
            .map(|e| e.map(|d| d.path()))
            .collect::<Vec<_>>()
    );

    // Clean shutdown.
    let _ = child.kill();
    let _ = child.wait();
}
