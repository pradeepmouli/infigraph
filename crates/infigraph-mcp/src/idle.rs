//! Idle self-termination for the MCP worker after its stdio client
//! disconnects — implements DESIGN-hardening.md R2.2.3 ("Live orphans
//! self-terminate after a configurable idle grace, default 5 min with
//! stdin closed"), closing incident I-5 (orphaned infigraph-mcp processes
//! found running indefinitely after their spawning client was gone).
//!
//! Scope: self-termination only. This does NOT implement the broader
//! instance registry (R2.2.1) or cross-process peer-vs-orphan
//! discrimination (R2.2.2) — those are separate, larger pieces of the
//! same P0 requirement, deliberately out of scope here.

use std::time::Duration;

const DEFAULT_GRACE_SECS: u64 = 300;
const DEFAULT_POLL_SECS: u64 = 10;

/// Grace period after the MCP client's stdio connection closes before this
/// process exits, if it's still alive only to serve the local UI.
/// Overridable via `INFIGRAPH_MCP_IDLE_GRACE_SECS` (seconds).
pub fn idle_grace_period() -> Duration {
    std::env::var("INFIGRAPH_MCP_IDLE_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_GRACE_SECS))
}

/// How often the post-EOF loop wakes to re-check the grace period.
/// Overridable via `INFIGRAPH_MCP_IDLE_POLL_SECS` (seconds) — kept small in
/// tests so they don't wait a full production-sized interval.
pub fn idle_poll_interval() -> Duration {
    std::env::var("INFIGRAPH_MCP_IDLE_POLL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_POLL_SECS))
}

/// Pure: has `elapsed` (time since the MCP client's stdin closed) reached
/// or exceeded `grace`? Boundary is inclusive.
pub fn should_exit_idle(elapsed: Duration, grace: Duration) -> bool {
    elapsed >= grace
}
