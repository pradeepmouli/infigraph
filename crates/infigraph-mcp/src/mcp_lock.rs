//! `mcp.lock` lifecycle: identity (via `infigraph_core::lockfile`),
//! heartbeat, wedged-holder detection (R2.3.3/R2.3.5), and build-hash
//! mismatch takeover (R2.3.1/R2.3.2/R2.3.2a). This is the lock that
//! decides which of possibly-several concurrently-running infigraph-mcp
//! processes on a machine is the "primary" allowed to run watchers.
//!
//! Invariant: a stale/wedged heartbeat alone never grants Primary -- it is
//! surfaced only as a loud log (`check_wedged_and_log`). The only way a
//! challenger becomes Primary is winning the real OS-level flock via
//! `lockfile::try_acquire`, either because the lock was free or because a
//! build-hash-mismatch handover request caused the incumbent to
//! voluntarily release and exit.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use infigraph_core::lockfile::{self, LockFile};
use serde::{Deserialize, Serialize};

infigraph_core::settings! {
    mcp_lock {
        heartbeat_secs: u64 = 15,
        wedged_secs: u64 = 60,
        takeover_poll_secs: u64 = 1,
        takeover_timeout_secs: u64 = 10,
    }
}

fn resolved_lock_settings() -> McpLock {
    let cli = RawMcpLock::parse_from(std::iter::empty::<String>());
    McpLock::resolve(cli, None)
}

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
    Duration::from_secs(resolved_lock_settings().heartbeat_secs)
}

/// How stale a holder's heartbeat must be before it's logged as possibly
/// wedged. Overridable via `INFIGRAPH_MCP_LOCK_WEDGED_SECS`.
pub fn wedged_threshold_secs() -> u64 {
    resolved_lock_settings().wedged_secs
}

/// Non-blocking: try to become mcp.lock's primary. `None` if another
/// process already holds it. Test-only seam; production code goes through
/// `acquire_with_takeover`, which wraps this with the build-hash-mismatch
/// handover handshake.
pub fn acquire_primary() -> Option<LockFile> {
    lockfile::try_acquire(&lock_path(), "mcp-primary")
        .ok()
        .flatten()
}

/// One heartbeat tick: refresh `last_heartbeat`. Logs a WARN on I/O
/// failure but never panics -- a heartbeat write failure alone shouldn't
/// bring down the primary. Production code calls this via
/// `heartbeat_and_check_handover`, which additionally checks for a
/// pending handover request after each tick.
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

/// A pending handover request, but only if the challenger that wrote it
/// could still be waiting on it. A request whose writer died before its own
/// timeout-triggered clear ran would otherwise survive on disk indefinitely
/// and kill the next process to become primary -- possibly hours later, for
/// an entirely unrelated reason -- on its very first heartbeat tick. Both
/// gates clear the file, so a discarded request isn't re-examined every
/// tick forever.
fn live_handover_request() -> Option<HandoverRequest> {
    let req = read_handover_request()?;
    let now = now_epoch_secs();
    let too_old = lockfile::is_holder_wedged(req.requested_at, now, handover_request_stale_secs());
    let requester_gone = !crate::lifecycle::process_alive(req.pid);
    if too_old || requester_gone {
        crate::mcp_log(
            "WARN",
            &format!(
                "Discarding orphaned mcp.lock handover request from PID {} ({}s old, \
                 requester {}) -- keeping the lock",
                req.pid,
                now.saturating_sub(req.requested_at),
                if requester_gone { "gone" } else { "alive" }
            ),
        );
        clear_handover_request();
        return None;
    }
    Some(req)
}

/// How often a challenger, having requested takeover, re-tries acquiring
/// the lock while waiting for the incumbent to honor the request.
/// Overridable via `INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS`.
pub fn takeover_poll_interval() -> Duration {
    Duration::from_secs(resolved_lock_settings().takeover_poll_secs)
}

/// How long a challenger waits for a handover request to be honored
/// before giving up and falling back to Secondary. Overridable via
/// `INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS`.
pub fn takeover_wait_timeout() -> Duration {
    Duration::from_secs(resolved_lock_settings().takeover_timeout_secs)
}

/// Ceiling on the automatically-derived part of the takeover wait. The whole
/// wait runs in `main.rs::run()` *before* the MCP transport exists, so every
/// second of it is dead time in which the server cannot answer a client at
/// all -- and against a genuinely wedged incumbent (one that never ticks, the
/// case `check_wedged_and_log` exists to surface) the challenger burns the
/// entire wait before degrading to Secondary. Common MCP client startup
/// budgets are ~30s (Claude Code's default `MCP_TIMEOUT`); 20s leaves ~10s of
/// that for the rest of startup, so "degrade to secondary" stays a degraded
/// server rather than a killed one.
const MAX_DERIVED_TAKEOVER_WAIT: Duration = Duration::from_secs(20);

/// The wait a challenger actually uses, with the heartbeat interval's
/// margin enforced in code rather than by convention: the incumbent only
/// ever notices a handover request on its own heartbeat tick, which lands
/// uniformly somewhere in `[0, heartbeat_interval)` after the request is
/// written. A wait shorter than the interval therefore fails a fraction of
/// the time by construction (the raw defaults -- 10s wait vs 15s heartbeat
/// -- failed ~1/3 of the time). One full interval plus 5s covers the tick
/// itself plus scheduling jitter, and the derived floor is then capped at
/// `MAX_DERIVED_TAKEOVER_WAIT`. Defaults land exactly on that cap: 20s, i.e.
/// 5s above the 15s heartbeat and 10s below a 30s client timeout.
///
/// A hard cap and an unbounded margin cannot both hold. If an operator raises
/// `heartbeat_interval` to at or above the cap, the derived wait no longer
/// covers a whole tick and the handshake regains the fractional-failure mode
/// the margin was added to remove. That is a disclosed trade-off -- startup
/// latency wins -- not an oversight. An explicitly configured
/// `takeover_wait_timeout` above the cap is still honored as-is: unlike the
/// derived floor, that wait was chosen rather than inherited.
pub fn effective_takeover_wait_timeout() -> Duration {
    let derived = (heartbeat_interval() + Duration::from_secs(5)).min(MAX_DERIVED_TAKEOVER_WAIT);
    takeover_wait_timeout().max(derived)
}

/// How old a pending handover request may be before the incumbent treats it
/// as abandoned. Double the challenger's own wait window, so a request can
/// only be judged stale well after the challenger that wrote it would
/// already have given up and cleared it.
fn handover_request_stale_secs() -> u64 {
    effective_takeover_wait_timeout()
        .as_secs()
        .saturating_mul(2)
}

/// Result of attempting to become mcp.lock's primary.
pub enum AcquireOutcome {
    Primary(LockFile),
    Secondary,
}

/// Try to become primary. If the lock is free, wins immediately. If it's
/// held, checks the incumbent's heartbeat (logs loudly if wedged, see
/// `check_wedged_and_log`) and build_hash: on a mismatch, requests
/// takeover and polls for up to `effective_takeover_wait_timeout()`; on a
/// match, or if the wait times out, falls back to Secondary.
pub fn acquire_with_takeover() -> AcquireOutcome {
    acquire_with_takeover_using(infigraph_core::build_hash())
}

/// `acquire_with_takeover` with the challenger's own build hash supplied
/// rather than read from `infigraph_core::build_hash()`. `pub` so tests can
/// drive the full request/poll/win/timeout loop against a real incumbent:
/// two "processes" inside one test binary otherwise compute the identical
/// real build hash, making a genuine mismatch -- the only thing that opens
/// this path -- impossible to produce.
pub fn acquire_with_takeover_using(own_build: &str) -> AcquireOutcome {
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

    let deadline = Instant::now() + effective_takeover_wait_timeout();
    while Instant::now() < deadline {
        std::thread::sleep(takeover_poll_interval());
        if let Ok(Some(lock)) = lockfile::try_acquire(&path, "mcp-primary") {
            clear_handover_request();
            return AcquireOutcome::Primary(lock);
        }
    }

    // The incumbent may have honored the request and released the lock in
    // the very instant the deadline was crossed, which would end with the
    // incumbent exited AND the challenger demoted -- no primary at all until
    // something restarts one. This last attempt narrows that window from
    // roughly a whole poll interval down to sub-millisecond; it does not
    // eliminate it, since the incumbent's `drop(lock)` can still land just
    // after this call returns. Closing it fully would mean watching for the
    // handover file's disappearance instead of retrying blindly -- out of
    // scope here.
    if let Ok(Some(lock)) = lockfile::try_acquire(&path, "mcp-primary") {
        clear_handover_request();
        return AcquireOutcome::Primary(lock);
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
    if let Some(req) = live_handover_request() {
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
