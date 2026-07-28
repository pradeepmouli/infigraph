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
