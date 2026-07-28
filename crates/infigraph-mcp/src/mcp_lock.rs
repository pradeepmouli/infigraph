//! `mcp.lock` lifecycle: identity (via `infigraph_core::lockfile`),
//! heartbeat, wedged-holder detection (R2.3.3/R2.3.5), and build-hash
//! mismatch takeover (R2.3.1/R2.3.2/R2.3.2a). This is the lock that
//! decides which of possibly-several concurrently-running infigraph-mcp
//! processes on a machine is the "primary" allowed to run watchers.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use infigraph_core::lockfile::{self, LockFile};
use serde::{Deserialize, Serialize};

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

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pure: does a challenger's build differ from the incumbent's? Only a
/// mismatch justifies requesting takeover -- two processes running the
/// identical binary have no reason to fight over the lock. `pub` (like
/// `is_stale`/`should_exit_idle`/`is_holder_wedged` elsewhere in this
/// codebase) so it's directly unit-testable with hand-supplied strings --
/// a same-process integration test can't otherwise produce a genuine
/// mismatch, since both the incumbent and challenger would compute the
/// same real `infigraph_core::build_hash()`.
pub fn build_hash_mismatch(own: &str, holder: &str) -> bool {
    own != holder
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandoverRequest {
    pid: u32,
    build_hash: String,
    requested_at: u64,
}

fn handover_request_path() -> PathBuf {
    let lock = lock_path();
    let parent = lock
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    parent.join("mcp.lock.handover")
}

/// Writes a handover request naming this process. `pub` so tests can
/// simulate "a challenger has requested takeover" against the real
/// `heartbeat_and_check_handover` without needing a genuine build-hash
/// mismatch (impossible within a single test binary).
pub fn write_handover_request() -> std::io::Result<()> {
    let req = HandoverRequest {
        pid: std::process::id(),
        build_hash: infigraph_core::build_hash().to_string(),
        requested_at: now_epoch_secs(),
    };
    let json = serde_json::to_string(&req).unwrap_or_default();
    std::fs::write(handover_request_path(), json)
}

/// Best-effort read. `None` if missing, empty, or unparseable.
fn read_handover_request() -> Option<HandoverRequest> {
    let content = std::fs::read_to_string(handover_request_path()).ok()?;
    serde_json::from_str(&content).ok()
}

fn clear_handover_request() {
    let _ = std::fs::remove_file(handover_request_path());
}

/// How often a challenger, having requested takeover, re-tries acquiring
/// the lock while waiting for the incumbent to honor the request.
/// Overridable via `INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS`.
pub fn takeover_poll_interval() -> Duration {
    std::env::var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(1))
}

/// How long a challenger waits for a handover request to be honored
/// before giving up and falling back to Secondary. Overridable via
/// `INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS`.
pub fn takeover_wait_timeout() -> Duration {
    std::env::var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(10))
}

/// Result of attempting to become mcp.lock's primary.
pub enum AcquireOutcome {
    Primary(LockFile),
    Secondary,
}

/// Try to become primary. If the lock is free, wins immediately. If it's
/// held, checks the incumbent's heartbeat (logs loudly if wedged, see
/// `check_wedged_and_log`) and build_hash: on a mismatch, requests
/// takeover and polls for up to `takeover_wait_timeout()`; on a match, or
/// if the wait times out, falls back to Secondary.
pub fn acquire_with_takeover() -> AcquireOutcome {
    let path = lock_path();
    match lockfile::try_acquire(&path, "mcp-primary") {
        Ok(Some(lock)) => return AcquireOutcome::Primary(lock),
        Ok(None) => {}
        Err(e) => {
            crate::mcp_log("WARN", &format!("mcp.lock open failed: {e:#}"));
            return AcquireOutcome::Secondary;
        }
    }

    let Some(holder) = lockfile::read_holder(&path) else {
        return AcquireOutcome::Secondary;
    };

    check_wedged_and_log(&holder, now_epoch_secs());

    let own_build = infigraph_core::build_hash();
    if !build_hash_mismatch(own_build, &holder.build_hash) {
        return AcquireOutcome::Secondary;
    }

    crate::mcp_log(
        "INFO",
        &format!(
            "mcp.lock held by PID {} on build {} (ours: {own_build}) -- requesting handover",
            holder.pid, holder.build_hash
        ),
    );
    if write_handover_request().is_err() {
        return AcquireOutcome::Secondary;
    }

    let deadline = Instant::now() + takeover_wait_timeout();
    while Instant::now() < deadline {
        std::thread::sleep(takeover_poll_interval());
        if let Ok(Some(lock)) = lockfile::try_acquire(&path, "mcp-primary") {
            clear_handover_request();
            return AcquireOutcome::Primary(lock);
        }
    }

    crate::mcp_log(
        "WARN",
        "mcp.lock handover request timed out -- running as secondary",
    );
    clear_handover_request();
    AcquireOutcome::Secondary
}

/// One heartbeat tick for the primary: refresh `last_heartbeat`, then
/// check for a pending handover request. Returns `true` if one was found
/// and honored -- the caller must drop `lock` and exit the process. This
/// is a release-and-exit, not a graceful drain of in-flight work (that's
/// out of scope here).
pub fn heartbeat_and_check_handover(lock: &mut LockFile) -> bool {
    heartbeat_tick(lock);
    if let Some(req) = read_handover_request() {
        crate::mcp_log(
            "INFO",
            &format!(
                "Handover requested by PID {} (build {}) -- releasing mcp.lock and exiting",
                req.pid, req.build_hash
            ),
        );
        clear_handover_request();
        return true;
    }
    false
}
