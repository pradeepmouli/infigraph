//! `verify` and `doctor` read a project with a live daemon through that
//! daemon (#209 item 12): a second process opening the graph beside its
//! writer is the shape `concurrent_writer_reader` crashes in. With no daemon
//! they keep opening it themselves, read-only.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use infigraph_core::doctor::{check_one_compaction_drift, CheckStatus};
use infigraph_core::graph::compaction::{stamp_compaction_baseline, TableStats};
use infigraph_core::verify::run_verify;

/// The daemon must be a separate PROCESS: an in-process one shares the
/// observer's address space, where a second open of the graph happens to
/// work, which is not the situation this guards (measured: the same checks
/// pass in-process on the unrouted code). Same small-helper duplication
/// precedent as `daemon_kuzu_e2e.rs`.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli_command() -> Command {
    let cli = infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    )
    .expect("infigraph CLI binary must already be built (shared target dir)");
    let mut command = Command::new(cli);
    command.env(infigraph_core::scip_switch::enabled_env_name(), "0");
    command
}

/// Index `project` directly (no daemon), then start a real detached daemon
/// on it and wait for `watch.lock`.
fn live_daemon(project: &Path) -> KillOnDrop {
    std::fs::write(project.join("main.py"), "def main():\n    pass\n").unwrap();
    let status = cli_command()
        .arg("index")
        .current_dir(project)
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .env("INFIGRAPH_NO_WATCH", "1")
        .status()
        .unwrap();
    assert!(status.success(), "bootstrap index failed");
    let daemon = KillOnDrop(
        cli_command()
            .arg("daemon")
            .current_dir(project)
            .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
            .spawn()
            .unwrap(),
    );
    let lock = project.join(".infigraph").join("watch.lock");
    let start = Instant::now();
    while !(lock.exists() && std::fs::metadata(&lock).unwrap().len() > 0) {
        assert!(start.elapsed() < Duration::from_secs(20), "no watch.lock");
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon
}

fn live_project() -> (tempfile::TempDir, KillOnDrop) {
    let tmp = tempfile::tempdir().unwrap();
    let daemon = live_daemon(tmp.path());
    (tmp, daemon)
}

/// Whether this process holds a lease on the daemon. A read routed through
/// `RemoteExec` takes one (`lease::in_use`), a direct open never does, and
/// `status` itself does not (`KEEPS_ALIVE = false`) -- so it is the proof that
/// an observer asked the daemon. A direct open beside a live daemon succeeds
/// on this machine, so "it did not fail" could not tell the two apart.
fn this_process_leases(root: &Path) -> bool {
    infigraph_core::daemon::control::query_status(root)
        .expect("the daemon answers status")
        .lease_owners
        .iter()
        .any(|o| o.pid == std::process::id())
}

fn status<'a>(
    results: &'a [infigraph_core::doctor::CheckResult],
    name: &str,
) -> &'a infigraph_core::doctor::CheckResult {
    results
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no check {name:?} in {results:#?}"))
}

#[test]
fn verify_of_a_project_with_a_live_daemon_is_answered_by_the_daemon() {
    let (tmp, daemon) = live_project();
    assert!(!this_process_leases(tmp.path()));

    let results = run_verify(tmp.path());

    assert!(
        this_process_leases(tmp.path()),
        "verify never reached the daemon"
    );

    let open = status(&results, "graph: open");
    assert_eq!(open.status, CheckStatus::Pass, "{results:#?}");
    assert!(
        open.message.contains("daemon"),
        "the open check should say the daemon served it: {open:#?}"
    );
    assert_eq!(
        status(&results, "graph: symbol->file references").status,
        CheckStatus::Pass,
        "{results:#?}"
    );
    drop(daemon);
}

#[test]
fn the_compaction_drift_check_measures_through_a_live_daemon() {
    let (tmp, daemon) = live_project();
    let ig = tmp.path().join(".infigraph");
    let base = |pages, rows| TableStats { pages, rows };
    stamp_compaction_baseline(
        &ig,
        &[
            ("Symbol".to_string(), base(1000, 1)),
            ("File".to_string(), base(1000, 1)),
        ],
    );

    assert!(!this_process_leases(tmp.path()));
    let result = check_one_compaction_drift(tmp.path())
        .expect("a live daemon's graph must be measurable, not skipped");

    assert!(
        this_process_leases(tmp.path()),
        "the drift check never reached the daemon"
    );
    assert_ne!(result.status, CheckStatus::Fail, "{result:#?}");
    assert!(
        !result.message.contains("no table has enough rows") || result.message.contains("drift"),
        "{result:#?}"
    );
    drop(daemon);
}

#[test]
fn without_a_daemon_verify_still_opens_the_graph_itself() {
    let tmp = tempfile::tempdir().unwrap();
    drop(
        infigraph_core::graph::GraphStore::open(&tmp.path().join(".infigraph").join("graph"))
            .unwrap(),
    );

    let results = run_verify(tmp.path());

    let open = status(&results, "graph: open");
    assert_eq!(open.status, CheckStatus::Pass, "{results:#?}");
    assert!(!open.message.contains("daemon"), "{open:#?}");
}
