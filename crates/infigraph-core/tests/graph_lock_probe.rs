//! Cross-process regression tests for the lbug lock-contention fd leak
//! (sittir daemon, Aug 2026: 7,600 leaked fds on one graph file).
//!
//! lbug's `openFile` leaks the fd it opened whenever `fcntl(F_SETLK)` loses
//! to another process. The fix is to never poll the lock by re-opening:
//! `graph::lock_probe` answers "who holds it" with `F_GETLK`, and
//! `Infigraph::init` waits on that probe instead. These tests hold the
//! graph from a *second OS process* (re-exec of this test binary), because
//! fcntl locks are per-process and an in-process second open succeeds.
#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use infigraph_core::graph::lock_probe::{probe_graph_lock, LockProbe, ProbeFor};
use infigraph_core::graph::GraphStore;
use infigraph_core::lang::LanguageRegistry;
use infigraph_core::Infigraph;

fn open_fds() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

/// Re-exec of this test binary that opens the graph named by
/// `HOLD_GRAPH_PATH` and keeps it open until killed.
#[test]
fn hold_graph_helper() {
    let Ok(p) = std::env::var("HOLD_GRAPH_PATH") else {
        return;
    };
    let _held = GraphStore::open(Path::new(&p)).expect("holder open");
    std::thread::sleep(Duration::from_secs(60));
}

struct Holder(std::process::Child);

impl Holder {
    fn spawn(db_path: &Path) -> Self {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["hold_graph_helper", "--exact", "--nocapture"])
            .env("HOLD_GRAPH_PATH", db_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Wait until the child actually holds the lock.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while probe_graph_lock(db_path, ProbeFor::Write) == LockProbe::Free {
            assert!(
                std::time::Instant::now() < deadline,
                "holder never acquired the lock"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Holder(child)
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fresh_graph() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join(".infigraph").join("graph");
    drop(GraphStore::open(&db_path).expect("create"));
    (dir, db_path)
}

#[test]
fn probe_reports_the_holding_pid_then_free_after_release() {
    let (_dir, db_path) = fresh_graph();
    assert_eq!(probe_graph_lock(&db_path, ProbeFor::Write), LockProbe::Free);

    let holder = Holder::spawn(&db_path);
    assert_eq!(
        probe_graph_lock(&db_path, ProbeFor::Write),
        LockProbe::Locked(Some(holder.pid())),
        "F_GETLK must name the process holding lbug's fcntl lock"
    );
    // A writer's exclusive lock also blocks read-only opens.
    assert!(matches!(
        probe_graph_lock(&db_path, ProbeFor::Read),
        LockProbe::Locked(_)
    ));
    drop(holder);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while probe_graph_lock(&db_path, ProbeFor::Write) != LockProbe::Free {
        assert!(std::time::Instant::now() < deadline, "lock never released");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn probing_does_not_leak_fds_and_does_not_disturb_our_own_open() {
    let (_dir, db_path) = fresh_graph();
    // This process holds the graph; probing it repeatedly must neither
    // leak fds nor (by closing a probe fd) release our own fcntl lock --
    // observable as another process being able to open it.
    let _ours = GraphStore::open(&db_path).unwrap();
    let before = open_fds();
    for _ in 0..50 {
        let _ = probe_graph_lock(&db_path, ProbeFor::Write);
    }
    let after = open_fds();
    assert!(
        after <= before + 1,
        "probe must keep at most one long-lived fd per path (before={before}, after={after})"
    );
    // Our lock is intact: a second process cannot take it.
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["hold_graph_helper", "--exact", "--nocapture"])
        .env("HOLD_GRAPH_PATH", &db_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "a probe must not release this process's own lbug lock"
    );
}

#[test]
fn init_under_contention_waits_on_the_probe_instead_of_reopening() {
    let (dir, db_path) = fresh_graph();
    let holder = Holder::spawn(&db_path);

    let before = open_fds();
    let start = std::time::Instant::now();
    let mut ig = Infigraph::open(dir.path(), LanguageRegistry::new()).unwrap();
    let err = ig
        .init()
        .expect_err("write open must fail while another process holds the graph");
    let elapsed = start.elapsed();
    let after = open_fds();

    // Exactly one lbug open (one leaked fd) plus at most the probe's own
    // long-lived fd. Before the fix this was ~20 fds per call.
    assert!(
        after <= before + 2,
        "lock wait must not re-open lbug per poll (before={before}, after={after})"
    );
    assert!(
        elapsed >= Duration::from_millis(2500),
        "must still wait out the contention budget: {elapsed:?}"
    );
    let msg = format!("{err:#}");
    assert!(
        msg.contains(&format!("PID {}", holder.pid())),
        "error must name the holding pid: {msg}"
    );
}
