//! Integration coverage for R2.4.4-R2.4.6 (docs/DESIGN-hardening.md): the
//! daemon must survive a `WatchControl { role: Code, action: Stop }` request
//! without exiting or losing its ability to serve `DaemonKuzu` writes.
//!
//! Harness shape (real detached child process, stderr/stdout drained on their
//! own threads, `KillOnDrop` cleanup, binary resolved via the deps-dir
//! grandparent fallback) is copied from
//! `crates/infigraph-cli/tests/watch_daemon_docs.rs::cmd_watch_daemon_also_indexes_docs_without_restart`
//! rather than reinvented -- that test already proved this shape works
//! against a genuine `infigraph daemon`.

use infigraph_core::daemon_protocol::{
    submit_write_request, WatchAction, WatchRole, WriteRequest, WriteResult,
};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn cli_binary() -> std::path::PathBuf {
    // Mirrors infigraph_core::watch::daemon::resolve_cli_binary_sibling_of's
    // grandparent fallback: integration-test binaries live one level below
    // the real build output directory.
    let exe = std::env::current_exe().unwrap();
    let deps_dir = exe.parent().unwrap();
    let candidate = deps_dir.join("infigraph");
    if candidate.exists() {
        return candidate;
    }
    deps_dir.parent().unwrap().join("infigraph")
}

/// RAII guard that kills and reaps a spawned child process on drop, so a
/// panic anywhere in the test (not just the happy path) can't leave a real
/// `infigraph daemon` process running forever against an abandoned tempdir.
/// `std::process::Child` does not do this itself -- Drop just closes the
/// parent's handle, the child keeps running independently.
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

/// True while the child is still running. `try_wait` is the only
/// non-destructive liveness check available for a process we own -- a
/// `kill(pid, 0)` probe would report the zombie an unreaped exited child
/// leaves behind as still alive.
fn still_running(child: &mut std::process::Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

/// Drains everything already queued on `rx` without blocking.
fn drain_pending(rx: &mpsc::Receiver<String>) {
    while rx.try_recv().is_ok() {}
}

/// Debug builds of the daemon spend seconds building the 62-language
/// registry before its first graph open, so every wait here is generous:
/// the point of the test is behavior, not latency.
const REPLY_TIMEOUT: Duration = Duration::from_secs(180);

#[test]
fn daemon_survives_watch_control_stop_and_keeps_serving_writes() {
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
    std::fs::write(&watched, "def v1():\n    pass\n").unwrap();

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

    // Every reindex the daemon performs surfaces as an on_event line on its
    // stdout ("[watch] modified: <path>"), emitted from the coordinator's
    // drain-reap step. That is the observable that distinguishes "the
    // producer really stopped" from "it only logged that it did".
    let (reindex_tx, reindex_rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            eprintln!("[daemon stdout] {line}");
            if line.contains("[watch] modified:") || line.contains("[watch] created:") {
                let _ = reindex_tx.send(line);
            }
        }
    });
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[daemon stderr] {line}");
        }
    });

    let lock_path = root.join(".infigraph").join("watch.lock");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !infigraph_core::watch::daemon::daemon_is_alive(&lock_path) {
        assert!(
            Instant::now() < deadline,
            "daemon never became alive (watch.lock never showed a live holder)"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let requests_dir = root.join(".infigraph").join("requests");

    // 1. Stop code-watching. Before this task, WatchControl reached
    //    serve_one_request's placeholder and came back as an error.
    let stop_reply = submit_write_request(
        &requests_dir,
        &WriteRequest::WatchControl {
            role: WatchRole::Code,
            action: WatchAction::Stop,
        },
        REPLY_TIMEOUT,
    )
    .expect("daemon never replied to WatchControl { Code, Stop }");
    assert!(
        matches!(stop_reply, WriteResult::Ok { .. }),
        "WatchControl {{ Code, Stop }} must succeed, got {stop_reply:?}"
    );

    // 2. The whole premise of R2.4.4: stopping *watching* must not stop the
    //    *daemon*.
    assert!(
        still_running(&mut child),
        "the daemon process exited after a code-watch stop -- that is exactly \
         the conflated-lifecycle bug this split exists to fix"
    );

    // 3. The producer really stopped: a file change now produces no reindex.
    drain_pending(&reindex_rx);
    std::fs::write(&watched, "def v2():\n    pass\n").unwrap();
    if let Ok(line) = reindex_rx.recv_timeout(Duration::from_secs(5)) {
        panic!("code-watching was stopped, but the daemon still reindexed: {line}");
    }

    // 4. Write-serving is untouched by the code-watch stop.
    let index_reply =
        submit_write_request(&requests_dir, &WriteRequest::FullReindex, REPLY_TIMEOUT)
            .expect("daemon stopped serving writes after a code-watch stop");
    assert!(
        matches!(index_reply, WriteResult::FullReindexOk { .. }),
        "a write request must still be served while code-watching is stopped, got {index_reply:?}"
    );
    assert!(
        still_running(&mut child),
        "the daemon process exited while serving a write request"
    );

    // 5. Restarting code-watching brings reindexing back.
    let start_reply = submit_write_request(
        &requests_dir,
        &WriteRequest::WatchControl {
            role: WatchRole::Code,
            action: WatchAction::Start,
        },
        REPLY_TIMEOUT,
    )
    .expect("daemon never replied to WatchControl { Code, Start }");
    assert!(
        matches!(start_reply, WriteResult::Ok { .. }),
        "WatchControl {{ Code, Start }} must succeed, got {start_reply:?}"
    );

    // The freshly-spawned producer registers its notify watcher
    // asynchronously; give it a moment so the write below lands after the
    // watch is live rather than racing it.
    std::thread::sleep(Duration::from_millis(1000));
    drain_pending(&reindex_rx);
    std::fs::write(&watched, "def v3():\n    pass\n").unwrap();

    let resumed = reindex_rx.recv_timeout(Duration::from_secs(60));
    // Kill before asserting so a failure can't leave the daemon writing into
    // a tempdir that is about to be removed. `KillOnDrop` still guards every
    // earlier exit path.
    let _ = child.kill();
    let _ = child.wait();
    resumed.expect(
        "after WatchControl { Code, Start } the daemon must reindex file changes again \
         (timed out waiting for a '[watch] modified:' line on its stdout)",
    );
}
