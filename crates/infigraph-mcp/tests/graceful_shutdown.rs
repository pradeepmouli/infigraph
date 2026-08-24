//! R5.4 (#79): a termination signal must deregister the worker's instance
//! and exit cleanly -- not kill the process mid-anything with Drop
//! handlers skipped, leaving a stale registration for a later reap to
//! find.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn sigterm_deregisters_the_instance_and_exits_promptly() {
    let tmp = tempfile::tempdir().unwrap();
    let instances_dir = tmp.path().join("instances");
    std::fs::create_dir_all(&instances_dir).unwrap();
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_infigraph-mcp"))
        .arg("--worker")
        .arg("--mcp")
        .current_dir(&project)
        // Hermetic: never touch the real lock/registry, and CI mode keeps
        // the worker from spawning daemons/watchers for the temp project.
        .env("INFIGRAPH_MCP_LOCK_PATH", tmp.path().join("mcp.lock"))
        .env("INFIGRAPH_INSTANCES_DIR", &instances_dir)
        .env("CI", "true")
        .env_remove("INFIGRAPH_BACKEND")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .stdin(Stdio::piped()) // held open -- stdin-close is the OTHER exit path
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infigraph-mcp worker");

    let pid = child.id();
    let instance_file = instances_dir.join(format!("{pid}.json"));

    // Wait for the worker to register itself (startup includes lock
    // acquisition and tool-table setup; generous budget, early exit).
    let start = Instant::now();
    while !instance_file.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("worker exited before registering: {status:?}");
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "worker never registered an instance file at {}",
            instance_file.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // SIGTERM -- the graceful path under test.
    let term = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    assert!(term.success());

    // R5.4's bound: prompt exit (well under 5s) with the registration gone.
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "worker did not exit within 10s of SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        status.success(),
        "graceful termination must exit 0, got {status:?}"
    );
    assert!(
        !instance_file.exists(),
        "the instance registration must be deregistered on SIGTERM"
    );
}

/// Companion to the worker test above, but SIGTERM hits the *supervisor*
/// (no `--worker` flag -- what a user actually sends when they Ctrl-C the
/// terminal or `kill` the process they started). Before this, the
/// supervisor had no panic hook and no signal handler at all: it still
/// exited on SIGTERM (default disposition), but logged nothing, leaving
/// the same blank trail an uncatchable SIGKILL does. This pins the fix:
/// the supervisor logs why it exited, exits promptly, and its worker
/// child -- which gets no signal of its own -- notices via
/// `spawn_parent_monitor`'s poll and deregisters instead of orphaning.
#[test]
fn sigterm_to_the_supervisor_logs_why_and_exits_promptly_and_the_worker_follows() {
    let tmp = tempfile::tempdir().unwrap();
    let instances_dir = tmp.path().join("instances");
    std::fs::create_dir_all(&instances_dir).unwrap();
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let log_path = tmp.path().join("mcp.log");

    let mut child = Command::new(env!("CARGO_BIN_EXE_infigraph-mcp"))
        .arg("--mcp")
        .current_dir(&project)
        .env("INFIGRAPH_MCP_LOCK_PATH", tmp.path().join("mcp.lock"))
        .env("INFIGRAPH_INSTANCES_DIR", &instances_dir)
        .env("INFIGRAPH_MCP_LOG_PATH", &log_path)
        .env("CI", "true")
        .env_remove("INFIGRAPH_BACKEND")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infigraph-mcp supervisor");

    let supervisor_pid = child.id();

    // Wait for the worker child (spawned by the supervisor) to register --
    // proof the supervisor actually got a worker up before we kill it.
    let start = Instant::now();
    loop {
        let count = std::fs::read_dir(&instances_dir)
            .map(|d| d.flatten().count())
            .unwrap_or(0);
        if count == 1 {
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("supervisor exited before its worker registered: {status:?}");
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "worker never registered an instance file under {}",
            instances_dir.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let term = Command::new("kill")
        .args(["-TERM", &supervisor_pid.to_string()])
        .status()
        .unwrap();
    assert!(term.success());

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "supervisor did not exit within 10s of SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        status.success(),
        "supervisor's SIGTERM handler must exit 0, got {status:?}"
    );

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains(&format!(
            "supervisor (pid {supervisor_pid}): termination signal received -- exiting"
        )),
        "expected an exit-reason log line, got: {log}"
    );

    // No signal was delivered to the worker directly (`kill -TERM <pid>`
    // only targets the supervisor's pid); it must self-exit once it
    // notices its supervisor is gone (polled every 5s) rather than being
    // left an orphan holding the instance registration.
    let start = Instant::now();
    loop {
        let count = std::fs::read_dir(&instances_dir)
            .map(|d| d.flatten().count())
            .unwrap_or(0);
        if count == 0 {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "worker did not deregister after its supervisor died"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
