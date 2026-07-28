use std::time::Duration;

/// Serializes tests that mutate the process-global INFIGRAPH_MCP_LOCK_*
/// env vars -- cargo runs this binary's tests on parallel threads, so a
/// lowered override in one test must not leak into another test's window
/// (same lesson as idle.rs's/instances.rs's own env-mutation tests).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn heartbeat_interval_default_and_override() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
    assert_eq!(
        infigraph_mcp::mcp_lock::heartbeat_interval(),
        Duration::from_secs(15)
    );
    std::env::set_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS", "2");
    assert_eq!(
        infigraph_mcp::mcp_lock::heartbeat_interval(),
        Duration::from_secs(2)
    );
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
}

#[test]
fn wedged_threshold_default_and_override() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_WEDGED_SECS");
    assert_eq!(infigraph_mcp::mcp_lock::wedged_threshold_secs(), 60);
    std::env::set_var("INFIGRAPH_MCP_LOCK_WEDGED_SECS", "5");
    assert_eq!(infigraph_mcp::mcp_lock::wedged_threshold_secs(), 5);
    std::env::remove_var("INFIGRAPH_MCP_LOCK_WEDGED_SECS");
}

#[test]
fn acquire_primary_then_busy_then_free_again() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));

    let first = infigraph_mcp::mcp_lock::acquire_primary();
    assert!(first.is_some(), "lock should be free on first acquire");

    let second = infigraph_mcp::mcp_lock::acquire_primary();
    assert!(second.is_none(), "lock is held, second acquire must fail");

    drop(first);

    let third = infigraph_mcp::mcp_lock::acquire_primary();
    assert!(
        third.is_some(),
        "lock must be free again after the holder drops"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
}

#[test]
fn heartbeat_tick_advances_last_heartbeat() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));

    let mut lock = infigraph_mcp::mcp_lock::acquire_primary().expect("lock should be free");
    let path = infigraph_mcp::mcp_lock::lock_path();
    let before = infigraph_core::lockfile::read_holder(&path).unwrap();

    std::thread::sleep(Duration::from_millis(1100));
    infigraph_mcp::mcp_lock::heartbeat_tick(&mut lock);

    let after = infigraph_core::lockfile::read_holder(&path).unwrap();
    assert!(after.last_heartbeat > before.last_heartbeat);

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
}

#[test]
fn check_wedged_and_log_does_not_panic_on_fresh_or_stale_heartbeat() {
    // This function's only observable effect is a log line; there's no
    // return value to assert on directly (mcp_log has no test hook). This
    // test exists to catch a panic (e.g. an integer underflow bug in the
    // staleness math) on both a fresh and a very stale heartbeat -- the
    // real coverage of the underlying pure math is
    // `is_holder_wedged_pure_cases` in infigraph-core's own lockfile.rs
    // tests (Task 1).
    let fresh = infigraph_core::lockfile::LockInfo {
        pid: 1,
        role: "mcp-primary".to_string(),
        build_hash: "abc".to_string(),
        acquired_at: 1000,
        last_heartbeat: 1000,
    };
    infigraph_mcp::mcp_lock::check_wedged_and_log(&fresh, 1005);

    let stale = infigraph_core::lockfile::LockInfo {
        pid: 1,
        role: "mcp-primary".to_string(),
        build_hash: "abc".to_string(),
        acquired_at: 1000,
        last_heartbeat: 1000,
    };
    infigraph_mcp::mcp_lock::check_wedged_and_log(&stale, 1000 + wedged_secs_for_test() + 1);
}

fn wedged_secs_for_test() -> u64 {
    infigraph_mcp::mcp_lock::wedged_threshold_secs()
}

/// The build_hash comparison is the ONLY thing that gates a takeover
/// request in `acquire_with_takeover` -- staleness alone never triggers
/// one (see `check_wedged_and_log`, which only logs). Tested directly with
/// hand-supplied strings, same pattern as `is_stale`/`should_exit_idle`/
/// `is_holder_wedged` elsewhere in this codebase: a same-process
/// integration test can't produce a genuine mismatch, since both the
/// incumbent and challenger would compute the same real
/// `infigraph_core::build_hash()` -- see the doc comment on
/// `takeover_succeeds_when_incumbent_honors_handover_request` below for how
/// that test compensates.
#[test]
fn build_hash_mismatch_pure_cases() {
    assert!(!infigraph_mcp::mcp_lock::build_hash_mismatch("abc", "abc"));
    assert!(infigraph_mcp::mcp_lock::build_hash_mismatch("abc", "def"));
    assert!(!infigraph_mcp::mcp_lock::build_hash_mismatch("", ""));
}

#[test]
fn takeover_poll_interval_default_and_override() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS");
    assert_eq!(
        infigraph_mcp::mcp_lock::takeover_poll_interval(),
        Duration::from_secs(1)
    );
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS", "3");
    assert_eq!(
        infigraph_mcp::mcp_lock::takeover_poll_interval(),
        Duration::from_secs(3)
    );
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS");
}

#[test]
fn takeover_wait_timeout_default_and_override() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
    assert_eq!(
        infigraph_mcp::mcp_lock::takeover_wait_timeout(),
        Duration::from_secs(10)
    );
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS", "4");
    assert_eq!(
        infigraph_mcp::mcp_lock::takeover_wait_timeout(),
        Duration::from_secs(4)
    );
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
}

#[test]
fn acquire_with_takeover_wins_immediately_when_lock_is_free() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));

    let outcome = infigraph_mcp::mcp_lock::acquire_with_takeover();
    match outcome {
        infigraph_mcp::mcp_lock::AcquireOutcome::Primary(_) => {}
        infigraph_mcp::mcp_lock::AcquireOutcome::Secondary => {
            panic!("must win immediately when the lock is free")
        }
    }

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
}

/// Two builds with DIFFERENT build_hash would have the challenger request
/// and win a handover once the incumbent's own heartbeat loop honors it --
/// but a same-process test can't produce a genuine build_hash mismatch
/// (both sides compute the same real `infigraph_core::build_hash()`; see
/// `build_hash_mismatch_pure_cases` for direct coverage of that gate
/// instead). This test exercises the handover *mechanics* that fire once a
/// request already exists on disk: `write_handover_request` stands in for
/// whatever a real challenger's `acquire_with_takeover` would have written
/// after a genuine mismatch, and `heartbeat_and_check_handover` is the real
/// incumbent-side function under test -- it must see the request, log,
/// clear it, and signal the caller to release and exit. After the
/// incumbent drops its lock, a fresh acquire must succeed, standing in for
/// the challenger's poll loop finally winning.
#[test]
fn takeover_succeeds_when_incumbent_honors_handover_request() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));

    let mut incumbent = infigraph_mcp::mcp_lock::acquire_primary().expect("free");

    infigraph_mcp::mcp_lock::write_handover_request().expect("write handover request");
    let handover_path = dir.path().join("mcp.lock.handover");
    assert!(
        handover_path.exists(),
        "handover request must be written to disk before the incumbent can see it"
    );

    let handed_over = infigraph_mcp::mcp_lock::heartbeat_and_check_handover(&mut incumbent);
    assert!(
        handed_over,
        "incumbent must see the pending handover request"
    );
    assert!(
        !handover_path.exists(),
        "handover request must be cleared once honored"
    );

    drop(incumbent);

    let reacquired = infigraph_mcp::mcp_lock::acquire_primary();
    assert!(
        reacquired.is_some(),
        "lock must be free for a challenger to win after the incumbent releases it"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
}

/// The build_hash comparison in `acquire_with_takeover` is what gates
/// whether a handover request is even written. Since both the incumbent
/// and challenger in an in-process test share the same real
/// `infigraph_core::build_hash()`, this test verifies the SAME-build path
/// directly: no handover request should appear on disk, and the
/// challenger must give up as Secondary without ever unblocking the
/// incumbent.
#[test]
fn no_handover_request_written_when_build_hash_matches() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS", "1");
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS", "1");

    let _incumbent = infigraph_mcp::mcp_lock::acquire_primary().expect("free");

    let outcome = infigraph_mcp::mcp_lock::acquire_with_takeover();
    match outcome {
        infigraph_mcp::mcp_lock::AcquireOutcome::Secondary => {}
        infigraph_mcp::mcp_lock::AcquireOutcome::Primary(_) => {
            panic!("must not win the lock when build_hash matches the incumbent's")
        }
    }
    let handover_path = dir.path().join("mcp.lock.handover");
    assert!(
        !handover_path.exists(),
        "no handover request should be written or left behind on the same-build path"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
}
