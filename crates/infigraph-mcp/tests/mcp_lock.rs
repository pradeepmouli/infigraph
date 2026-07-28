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
