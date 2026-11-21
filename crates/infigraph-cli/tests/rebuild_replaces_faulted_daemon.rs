//! #165: `infigraph rebuild` replaces a daemon that has latched a fault a
//! fresh process can clear, and only such a daemon. The fault is planted
//! under the running daemon's own identity (read from its `watch.lock`), the
//! same record the daemon writes when its drains hit ENOSPC.

mod support;

use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use infigraph_core::daemon::fault::{fault_path, DaemonFault, FaultClass};

/// A bootstrapped project with a real daemon running against it.
fn project_with_daemon() -> (tempfile::TempDir, tempfile::TempDir, Child) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n").unwrap();

    let bootstrap = support::infigraph()
        .arg("index")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .status()
        .unwrap();
    assert!(bootstrap.success(), "bootstrap index must succeed");

    let mut daemon = support::infigraph()
        .arg("daemon")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .spawn()
        .unwrap();
    if holder_pid(project.path(), Duration::from_secs(10)).is_none() {
        let _ = daemon.kill();
        panic!("daemon never acquired watch.lock");
    }
    (project, home, daemon)
}

fn watch_lock(project: &Path) -> std::path::PathBuf {
    project.join(".infigraph").join("watch.lock")
}

fn holder_pid(project: &Path, budget: Duration) -> Option<u32> {
    let start = Instant::now();
    loop {
        if let Some(h) = infigraph_core::lockfile::read_holder(&watch_lock(project)) {
            return Some(h.pid);
        }
        if start.elapsed() > budget {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Record `class` as the running daemon's fault, exactly as it would.
fn plant_fault(project: &Path, class: FaultClass) {
    let holder = infigraph_core::lockfile::read_holder(&watch_lock(project)).unwrap();
    let fault = DaemonFault {
        since: holder.acquired_at,
        holder,
        class,
        error: "No space left on device (os error 28)".to_string(),
    };
    let path = fault_path(&project.join(".infigraph"));
    std::fs::write(path, serde_json::to_string(&fault).unwrap()).unwrap();
}

fn rebuild(project: &Path, home: &Path) -> std::process::Output {
    support::infigraph()
        .args(["rebuild", "--no-embed"])
        .current_dir(project)
        .env("HOME", home)
        .env("INFIGRAPH_BACKEND", "daemon")
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .output()
        .unwrap()
}

/// Stop whatever daemon holds the project now, including one `rebuild`
/// started detached.
fn stop_daemons(project: &Path, first: &mut Child) {
    let _ = first.kill();
    let _ = first.wait();
    if let Some(pid) = holder_pid(project, Duration::ZERO) {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
}

#[test]
fn rebuild_replaces_a_daemon_latched_on_a_full_disk() {
    let (project, home, mut daemon) = project_with_daemon();
    plant_fault(project.path(), FaultClass::DiskFull);

    let output = rebuild(project.path(), home.path());
    let exited = daemon.try_wait().unwrap().is_some();
    let replacement = holder_pid(project.path(), Duration::from_secs(5));
    let first_pid = daemon.id();
    stop_daemons(project.path(), &mut daemon);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "rebuild must succeed on the fresh daemon:\nstderr={stderr}"
    );
    assert!(
        stderr.contains("Stopped a daemon that could not recover"),
        "{stderr}"
    );
    assert!(exited, "the faulted daemon must be gone");
    assert!(
        replacement.is_some_and(|pid| pid != first_pid),
        "a fresh daemon must hold the project"
    );
}

#[test]
fn rebuild_goes_through_a_daemon_refusing_for_growth_without_restarting_it() {
    let (project, home, mut daemon) = project_with_daemon();
    plant_fault(project.path(), FaultClass::GrowthRefused);

    let output = rebuild(project.path(), home.path());
    let still_running = daemon.try_wait().unwrap().is_none();
    let fault_cleared = !fault_path(&project.path().join(".infigraph")).exists();
    stop_daemons(project.path(), &mut daemon);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "rebuild must succeed:\nstderr={stderr}"
    );
    assert!(
        still_running,
        "a growth refusal never costs readers a restart"
    );
    assert!(fault_cleared, "the successful rebuild clears the refusal");
}

/// A tripped crash-loop breaker is reset by the rebuild its message names.
/// Deleting the marker, which the message used to prescribe, reset nothing:
/// the next start re-derived the refusal from the attempts log, so a project
/// stayed refused until its attempts aged out of the hour. Through the
/// daemon, because a local `rebuild` wipes `.infigraph/` (the log with it)
/// anyway, while the daemon's rebuild swaps only the graph.
#[test]
fn rebuild_resets_a_tripped_crash_loop_breaker() {
    use infigraph_core::recovery;
    let (project, home, mut daemon) = project_with_daemon();
    let dir = project.path().join(".infigraph");
    for _ in 0..recovery::CRASH_LOOP_THRESHOLD {
        recovery::record_recovery_attempt(&dir).unwrap();
    }
    recovery::write_crash_loop_marker(&dir, &recovery::recent_recovery_attempts(&dir).unwrap())
        .unwrap();

    let output = rebuild(project.path(), home.path());
    let marker_gone = recovery::crash_loop_detected(&dir).is_none();
    let attempts = recovery::recent_recovery_attempts(&dir).unwrap();
    stop_daemons(project.path(), &mut daemon);

    assert!(
        output.status.success(),
        "rebuild must succeed:\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(marker_gone, "the marker is gone");
    assert!(
        attempts.is_empty(),
        "the attempts before the rebuild no longer count: {attempts:?}"
    );
}
