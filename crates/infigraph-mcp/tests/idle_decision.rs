use infigraph_mcp::idle::{idle_grace_period, idle_poll_interval, should_exit_idle};
use std::time::Duration;

/// Serializes tests that mutate the process-global INFIGRAPH_MCP_IDLE_*
/// env vars — cargo runs this binary's tests on parallel threads, so a
/// lowered override in one test must not leak into another test's window
/// (lesson from an identical env-var race caught in PR6's lockfile tests).
static IDLE_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn should_exit_idle_boundary() {
    assert!(!should_exit_idle(
        Duration::from_secs(299),
        Duration::from_secs(300)
    ));
    assert!(should_exit_idle(
        Duration::from_secs(300),
        Duration::from_secs(300)
    ));
    assert!(should_exit_idle(
        Duration::from_secs(301),
        Duration::from_secs(300)
    ));
}

#[test]
fn idle_grace_period_default_is_five_minutes() {
    let _env = IDLE_ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_IDLE_GRACE_SECS");
    assert_eq!(idle_grace_period(), Duration::from_secs(300));
}

#[test]
fn idle_grace_period_env_override() {
    let _env = IDLE_ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_MCP_IDLE_GRACE_SECS", "2");
    assert_eq!(idle_grace_period(), Duration::from_secs(2));
    std::env::remove_var("INFIGRAPH_MCP_IDLE_GRACE_SECS");
}

#[test]
fn idle_poll_interval_default_and_override() {
    let _env = IDLE_ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_IDLE_POLL_SECS");
    assert_eq!(idle_poll_interval(), Duration::from_secs(10));
    std::env::set_var("INFIGRAPH_MCP_IDLE_POLL_SECS", "1");
    assert_eq!(idle_poll_interval(), Duration::from_secs(1));
    std::env::remove_var("INFIGRAPH_MCP_IDLE_POLL_SECS");
}
