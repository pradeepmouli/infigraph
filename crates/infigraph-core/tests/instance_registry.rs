use infigraph_core::instances::{
    current_process_start_time, instances_dir, is_stale, list_instances, reap_orphans_once,
    register_instance, InstanceInfo,
};

/// Serializes tests that mutate the process-global INFIGRAPH_INSTANCES_DIR
/// env var — cargo runs this binary's tests on parallel threads, so one
/// test's override must not leak into another's window (same lesson as the
/// IDLE_ENV mutex in the R2.2.3 idle-self-termination test suite).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn is_stale_pure_cases() {
    // Same process, same recorded start time: live.
    assert!(!is_stale(1000, Some(1000)));
    // No such process anymore: dead.
    assert!(is_stale(1000, None));
    // A process exists at that PID, but its start time doesn't match what
    // was recorded: the original process is gone, PID was reused.
    assert!(is_stale(1000, Some(2000)));
}

#[test]
fn current_process_start_time_finds_self() {
    let pid = std::process::id();
    let first = current_process_start_time(pid);
    assert!(first.is_some(), "expected to find our own running process");
    let second = current_process_start_time(pid);
    assert_eq!(
        first, second,
        "a process's own start time must not change between two lookups"
    );
}

#[test]
fn register_and_list_round_trip() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_INSTANCES_DIR", dir.path());

    assert_eq!(instances_dir(), dir.path());

    let info = InstanceInfo::current("/tmp/some-project", "stdio");
    let guard = register_instance(&info).expect("register_instance");

    let listed = list_instances();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].1, info);

    drop(guard);
    assert!(
        list_instances().is_empty(),
        "dropping the guard must remove the instance file (clean-shutdown path)"
    );

    std::env::remove_var("INFIGRAPH_INSTANCES_DIR");
}

use infigraph_core::instances::{classify_instances, InstanceStatus};
use std::collections::HashMap;

fn fake_entry(pid: u32, started_at: u64) -> (std::path::PathBuf, InstanceInfo) {
    (
        std::path::PathBuf::from(format!("/fake/{pid}.json")),
        InstanceInfo {
            pid,
            started_at,
            project_path: "/fake/project".to_string(),
            transport: "stdio".to_string(),
            host_agent_hint: None,
        },
    )
}

#[test]
fn classify_instances_distinguishes_live_dead_and_reused() {
    let entries = vec![
        fake_entry(100, 1000), // live: lookup returns matching start time
        fake_entry(200, 1000), // dead: lookup returns None
        fake_entry(300, 1000), // reused: lookup returns a different start time
        fake_entry(999, 1000), // own_pid: must be skipped entirely
    ];
    let mut actual_starts: HashMap<u32, Option<u64>> = HashMap::new();
    actual_starts.insert(100, Some(1000));
    actual_starts.insert(200, None);
    actual_starts.insert(300, Some(9999));
    actual_starts.insert(999, Some(1000));

    let classified = classify_instances(&entries, 999, |pid| {
        actual_starts.get(&pid).copied().flatten()
    });

    assert_eq!(classified.len(), 3, "own_pid entry must be excluded");
    let status_for = |pid: u32| {
        classified
            .iter()
            .find(|(_, info, _)| info.pid == pid)
            .map(|(_, _, status)| status)
    };
    assert_eq!(status_for(100), Some(&InstanceStatus::LivePeer));
    assert_eq!(status_for(200), Some(&InstanceStatus::Orphan));
    assert_eq!(status_for(300), Some(&InstanceStatus::Orphan));
}

#[test]
fn list_instances_skips_unparseable_entries() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_INSTANCES_DIR", dir.path());

    std::fs::write(dir.path().join("99999999.json"), b"not valid json").unwrap();
    let info = InstanceInfo::current("/tmp/some-project", "stdio");
    let _guard = register_instance(&info).expect("register_instance");

    let listed = list_instances();
    assert_eq!(
        listed.len(),
        1,
        "the unparseable file must be skipped, not error the whole scan"
    );
    assert_eq!(listed[0].1, info);

    std::env::remove_var("INFIGRAPH_INSTANCES_DIR");
}

/// Regression test for a Critical bug: `reap_orphan` used to send real
/// SIGTERM/SIGKILL to whatever process currently held the recorded PID,
/// without re-checking `started_at`. But `classify_instances` only ever
/// marks an entry `Orphan` when either the PID is dead, or a live process
/// exists at that PID with a *different* start time than recorded — and
/// that second case, by the PID-reuse guard's own definition, means the
/// live process is provably not the one the entry named. So the old code's
/// only live target was always an innocent, unrelated process. This test
/// spawns a real child process, registers it under a deliberately wrong
/// `started_at` (simulating the PID-reuse classification case), reaps it,
/// and asserts the child is still alive — proving `reap_orphan` never
/// signals a process, only removes the stale file.
#[test]
fn reap_orphan_never_kills_a_pid_reused_process() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_INSTANCES_DIR", dir.path());

    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();

    // Deliberately wrong: real start times are always far larger than 1,
    // so this is guaranteed to mismatch whatever `current_process_start_time`
    // reports for the real child, simulating "PID reused" classification.
    let info = InstanceInfo {
        pid,
        started_at: 1,
        project_path: "/fake/project".to_string(),
        transport: "stdio".to_string(),
        host_agent_hint: None,
    };
    let path = dir.path().join(format!("{pid}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&info).unwrap()).unwrap();

    let own_pid = std::process::id();
    assert_ne!(
        own_pid, pid,
        "test process pid must differ from the spawned child's pid"
    );
    reap_orphans_once(own_pid);

    assert_eq!(
        child.try_wait().expect("try_wait on child"),
        None,
        "reap_orphans_once must never signal a live process, even one \
         classified Orphan due to a mismatched recorded start time"
    );
    assert!(
        !path.exists(),
        "the stale registry file must still be removed"
    );

    child.kill().expect("kill spawned child");
    child.wait().expect("wait for spawned child");
    std::env::remove_var("INFIGRAPH_INSTANCES_DIR");
}
