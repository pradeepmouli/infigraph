//! #204 end to end: writes over the socket to a real write coordinator.

mod common;
use common::daemon::{start, stop, ENV_LOCK};

use infigraph_core::daemon::writes::{submit, WriteOpts};
use infigraph_core::daemon_protocol::{WriteRequest, WriteResult};
use std::time::{Duration, Instant};

fn opts() -> WriteOpts<'static> {
    WriteOpts {
        timeout: Duration::from_secs(120),
        cancel: None,
        on_slot_wait: None,
    }
}

/// Success criterion 1: on an idle daemon a write no longer waits for a
/// 200ms tick.
#[test]
fn an_idle_daemon_answers_a_write_within_one_tick() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    // Warm: the graph is open before anything is timed.
    submit(dir.path(), &WriteRequest::Index { paths: None }, opts()).unwrap();
    let mut samples: Vec<Duration> = (0..5)
        .map(|_| {
            let t = Instant::now();
            let r = submit(
                dir.path(),
                &WriteRequest::UpsertRepo {
                    namespace: "n".into(),
                },
                opts(),
            )
            .unwrap();
            assert!(matches!(r, WriteResult::Ok { .. }), "{r:?}");
            t.elapsed()
        })
        .collect();
    samples.sort();
    assert!(
        samples[2] < Duration::from_millis(200),
        "median {:?} of {samples:?}",
        samples[2]
    );
    stop(d);
}

/// Both concurrent clients are answered by a rebuild. That it is *one*
/// rebuild is pinned at the unit level
/// (`a_full_reindex_requested_while_one_runs_joins_it_instead_of_queuing_another`);
/// this pins that joining works through the socket.
#[test]
fn concurrent_rebuilds_are_all_answered() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    let clients: Vec<_> = (0..2)
        .map(|_| {
            let root = dir.path().to_path_buf();
            std::thread::spawn(move || submit(&root, &WriteRequest::FullReindex, opts()))
        })
        .collect();
    for c in clients {
        let r = c.join().unwrap().unwrap();
        assert!(matches!(r, WriteResult::FullReindexOk { .. }), "{r:?}");
    }
    stop(d);
}

/// D3 end to end: a rebuild deferred behind a stalled coordinator, whose
/// client is killed, never runs. A rebuild swaps a new graph file in, so an
/// unchanged inode proves it did not happen.
#[cfg(unix)]
#[test]
fn a_killed_clients_deferred_rebuild_never_runs() {
    use std::os::unix::fs::MetadataExt as _;
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    // Read once at coordinator start; the loop then stalls while it exists.
    let stall = dir.path().join("stall");
    std::env::set_var("INFIGRAPH_TEST_COORDINATOR_STALL_FILE", &stall);
    let d = start(dir.path());
    std::env::remove_var("INFIGRAPH_TEST_COORDINATOR_STALL_FILE");
    submit(dir.path(), &WriteRequest::Index { paths: None }, opts()).unwrap();
    let graph = dir.path().join(".infigraph").join("graph");
    let inode_before = std::fs::metadata(&graph).unwrap().ino();
    std::fs::write(&stall, b"").unwrap();

    // A client in a child process, so killing it closes its socket for real.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "submit_full_reindex_child",
            "--ignored",
            "--nocapture",
        ])
        .env("INFIGRAPH_TEST_WRITE_ROOT", dir.path())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    child.kill().unwrap();
    let _ = child.wait();
    std::fs::remove_file(&stall).unwrap();

    // Long enough for a started rebuild of a one-file project to swap.
    std::thread::sleep(Duration::from_secs(5));
    let status = infigraph_core::daemon::control::query_status(dir.path()).unwrap();
    assert!(!status.work_in_flight, "nothing deferred or running");
    assert_eq!(
        std::fs::metadata(&graph).unwrap().ino(),
        inode_before,
        "no rebuild swapped in"
    );
    stop(d);
}

/// Child half of the test above: submits a FullReindex and waits to be
/// killed.
#[test]
#[ignore]
fn submit_full_reindex_child() {
    let Some(root) = std::env::var_os("INFIGRAPH_TEST_WRITE_ROOT") else {
        return;
    };
    let _ = submit(
        std::path::Path::new(&root),
        &WriteRequest::FullReindex,
        opts(),
    );
}
