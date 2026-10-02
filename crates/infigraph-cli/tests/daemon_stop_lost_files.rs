//! `daemon-stop --wait` must never report success while the daemon is alive.
//!
//! The worktree hook runs it before `git worktree remove`. A daemon whose
//! `watch.lock` and control socket are gone -- unlinked by a half-finished
//! remove, a `git clean`, or a hand `rm` -- is alive but unreachable, and
//! both of its old witnesses (the lock half of `confirm_daemon_exited`, and
//! the `daemon_is_alive` early return) read a missing file as "no daemon".
//! It then reported "stopped" and git raced a live writer. The process itself
//! is found by argv + cwd instead, as `scip-enrich` is.

#![cfg(unix)]

mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};

/// Kills and reaps the daemon on drop, so a failing assertion never leaves a
/// real daemon running against an abandoned tempdir.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_daemon_then_lose_its_files() -> (tempfile::TempDir, KillOnDrop) {
    let dir = tempfile::Builder::new()
        .prefix("infigraph-lost-files-")
        .tempdir()
        .unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("a.py"), "def a():\n    pass\n").unwrap();
    let indexed = support::infigraph()
        .arg("index")
        .current_dir(root)
        .env("INFIGRAPH_BACKEND", "kuzu")
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .unwrap();
    assert!(indexed.status.success(), "{indexed:?}");

    let child = support::infigraph()
        .args(["daemon", "--debounce", "50"])
        .current_dir(root)
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        // No idle exit: the daemon must outlive the unlinked files.
        .env("INFIGRAPH_DAEMON_IDLE_GRACE_SECS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let daemon = KillOnDrop(child);
    let lock = root.join(".infigraph").join("watch.lock");
    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(&lock, Duration::from_secs(30)),
        "daemon never acquired watch.lock"
    );

    // Where the endpoint is a file (macOS: under /tmp), lose it too. On Linux
    // the socket lives in the abstract namespace and there is no file to
    // lose (`ReadEndpoint::socket_path` is `None` by design), so only the
    // lock goes -- the daemon is still alive and unreachable by file.
    if let Some(socket) =
        infigraph_core::daemon::read_endpoint::ReadEndpoint::for_root(root).socket_path()
    {
        std::fs::remove_file(&socket).unwrap();
    }
    std::fs::remove_file(&lock).unwrap();
    (dir, daemon)
}

fn exited_within(daemon: &mut KillOnDrop, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if daemon.0.try_wait().unwrap().is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn daemon_stop_wait_stops_a_daemon_whose_lock_and_socket_are_gone() {
    let (dir, mut daemon) = start_daemon_then_lose_its_files();
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "the daemon must still be running before the stop"
    );

    let out = support::infigraph()
        .args(["daemon-stop", "--wait"])
        .current_dir(dir.path())
        .env("INFIGRAPH_BACKEND", "kuzu")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    let gone = exited_within(&mut daemon, Duration::from_secs(1));
    // Success is only honest if the daemon is gone; failure is honest either
    // way, and must say which process to deal with.
    if out.status.success() {
        assert!(
            gone,
            "daemon-stop --wait reported success but the daemon is still running:\n{stdout}"
        );
    } else {
        assert!(
            stderr.contains(&daemon.0.id().to_string()),
            "a failed stop must name the pid: {stderr}"
        );
    }
    assert!(
        gone,
        "a daemon with no lock and no socket must still be stopped by daemon-stop --wait \
         (stdout: {stdout}, stderr: {stderr})"
    );
}
