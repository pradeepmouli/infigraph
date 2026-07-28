//! `mcp.lock` lifecycle: identity (via `infigraph_core::lockfile`),
//! heartbeat, wedged-holder detection (R2.3.3/R2.3.5), and build-hash
//! mismatch takeover (R2.3.1/R2.3.2/R2.3.2a). This is the lock that
//! decides which of possibly-several concurrently-running infigraph-mcp
//! processes on a machine is the "primary" allowed to run watchers.

use std::path::PathBuf;
use std::time::Duration;

use infigraph_core::lockfile::{self, LockFile};

/// Full path to `mcp.lock`. Overridable via `INFIGRAPH_MCP_LOCK_PATH`
/// (tests) so tests never touch the real `~/.infigraph/mcp.lock`.
pub fn lock_path() -> PathBuf {
    if let Ok(path) = std::env::var("INFIGRAPH_MCP_LOCK_PATH") {
        return PathBuf::from(path);
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".infigraph")
        .join("mcp.lock")
}

/// How often the primary's heartbeat thread refreshes `mcp.lock`'s
/// `last_heartbeat` and checks for a pending handover request.
/// Overridable via `INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS`.
pub fn heartbeat_interval() -> Duration {
    std::env::var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(15))
}

/// How stale a holder's heartbeat must be before it's logged as possibly
/// wedged. Overridable via `INFIGRAPH_MCP_LOCK_WEDGED_SECS`.
pub fn wedged_threshold_secs() -> u64 {
    std::env::var("INFIGRAPH_MCP_LOCK_WEDGED_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60)
}

/// Non-blocking: try to become mcp.lock's primary. `None` if another
/// process already holds it. No takeover logic yet -- Task 4 wraps this
/// into `acquire_with_takeover`.
pub fn acquire_primary() -> Option<LockFile> {
    lockfile::try_acquire(&lock_path(), "mcp-primary")
        .ok()
        .flatten()
}

/// One heartbeat tick: refresh `last_heartbeat`. Logs a WARN on I/O
/// failure but never panics -- a heartbeat write failure alone shouldn't
/// bring down the primary. No handover check yet -- Task 4 extends this
/// into `heartbeat_and_check_handover`.
pub fn heartbeat_tick(lock: &mut LockFile) {
    if let Err(e) = lock.heartbeat() {
        crate::mcp_log("WARN", &format!("mcp.lock heartbeat failed: {e:#}"));
    }
}

/// Logs a loud WARN if `holder`'s heartbeat is stale enough to suspect
/// it's wedged -- still holding the OS-level flock (so alive in the
/// liveness sense) but not doing whatever periodic heartbeat work it's
/// supposed to be doing. Advisory only: this never forces a takeover by
/// itself (see the module doc comment) -- it's what makes a wedged holder
/// visible instead of silently blocking every other process forever.
pub fn check_wedged_and_log(holder: &infigraph_core::lockfile::LockInfo, now: u64) {
    if lockfile::is_holder_wedged(holder.last_heartbeat, now, wedged_threshold_secs()) {
        let stale_for = now.saturating_sub(holder.last_heartbeat);
        crate::mcp_log(
            "WARN",
            &format!(
                "mcp.lock is held by PID {} but its heartbeat is {stale_for}s stale \
                 (threshold {}s) -- it may be wedged. Run `infigraph watch-status` \
                 or check the process directly.",
                holder.pid,
                wedged_threshold_secs()
            ),
        );
    }
}
