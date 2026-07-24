use infigraph_core::instances::{
    current_process_start_time, instances_dir, is_stale, list_instances, register_instance,
    InstanceInfo,
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
