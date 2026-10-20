//! Automatic recovery from a dead-holder WAL (R3.1.4a, #115) and its
//! crash-loop breaker (R3.1.4c). Two pieces of state persist under
//! `.infigraph/`, mirroring the short-lived-file-then-append-log shape
//! `dirty.rs`/`audit.rs` already use elsewhere in this crate: a
//! "recovery needed" sentinel the daemon coordinator polls for
//! (`recovery_rebuild_wanted`, called from `daemon::run_write_coordinator`),
//! and a rolling attempts log the coordinator consults before acting on it.
//!
//! The read path (`graph::store::open_read_only_or_degrade`) only ever
//! writes the sentinel -- it never submits a write request itself. Per
//! docs/superpowers/specs/2026-08-24-r3-1-4-wal-recovery-hardening-design.md's
//! Architecture section, only the daemon coordinator thread may initiate a
//! rebuild, preserving R2.1.3's single-writer invariant.

use crate::snapshot::{list_restore_points, RestorePointKind};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RECOVERY_NEEDED_FILE: &str = "recovery-needed";
const UNCLEAN_EXIT_FILE: &str = "unclean-exit.json";
const RECOVERY_ATTEMPTS_LOG: &str = "recovery-attempts.log";
const CRASH_LOOP_MARKER_FILE: &str = "recovery-crash-loop";

/// Matches the observed incident exactly (two rebuild generations within
/// ~1 hour) and mirrors `quarantine::QUARANTINE_RETENTION`'s existing N=2
/// convention.
pub const CRASH_LOOP_THRESHOLD: usize = 2;
pub const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(3600);

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn recovery_needed_path(infigraph_dir: &Path) -> PathBuf {
    infigraph_dir.join(RECOVERY_NEEDED_FILE)
}

/// Called from the read path the moment it quarantines a dead-holder-WAL
/// graph. Records why, both for the coordinator and for a human reading
/// the sentinel file directly during an incident.
pub fn mark_recovery_needed(
    infigraph_dir: &Path,
    dead_pid: u32,
    quarantined_to: &Path,
) -> Result<()> {
    std::fs::create_dir_all(infigraph_dir)
        .with_context(|| format!("create {}", infigraph_dir.display()))?;
    let payload = serde_json::json!({
        "dead_pid": dead_pid,
        "quarantined_to": quarantined_to.display().to_string(),
        "detected_at": now_epoch_secs(),
    });
    crate::daemon_protocol::write_atomic(
        &recovery_needed_path(infigraph_dir),
        &serde_json::to_string_pretty(&payload)?,
    )
}

pub fn pending_recovery(infigraph_dir: &Path) -> bool {
    recovery_needed_path(infigraph_dir).exists()
}

pub fn clear_recovery_needed(infigraph_dir: &Path) -> Result<()> {
    let path = recovery_needed_path(infigraph_dir);
    if path.exists() {
        std::fs::remove_file(&path).context("remove recovery-needed sentinel")?;
    }
    Ok(())
}

/// Timestamps of auto-triggered rebuilds still inside the crash-loop window.
pub fn recent_recovery_attempts(infigraph_dir: &Path) -> Result<Vec<u64>> {
    let path = infigraph_dir.join(RECOVERY_ATTEMPTS_LOG);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path).context("read recovery-attempts.log")?;
    let cutoff = now_epoch_secs().saturating_sub(CRASH_LOOP_WINDOW.as_secs());
    Ok(content
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .filter(|&ts| ts >= cutoff)
        .collect())
}

pub fn record_recovery_attempt(infigraph_dir: &Path) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(infigraph_dir)
        .with_context(|| format!("create {}", infigraph_dir.display()))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(infigraph_dir.join(RECOVERY_ATTEMPTS_LOG))
        .context("open recovery-attempts.log for append")?;
    writeln!(f, "{}", now_epoch_secs()).context("append recovery-attempts.log")?;
    f.sync_all().ok();
    Ok(())
}

/// Where a deliberate hard exit records itself (#146).
pub fn unclean_exit_marker_path(infigraph_dir: &Path) -> PathBuf {
    infigraph_dir.join(UNCLEAN_EXIT_FILE)
}

/// What a process was doing when it hard-exited, for the next opener.
#[derive(Debug, Clone)]
pub struct UncleanExit {
    pub pid: u32,
    pub reason: String,
    /// The lbug write in flight at the time, from `write_phase::current`.
    pub phase: Option<String>,
    pub at: u64,
}

/// Record that this process is about to hard-exit, and why (#146).
///
/// A hard exit mid-write leaves an unreplayed WAL, which the dead-holder
/// guard then refuses -- correctly, but with no way to tell a deliberate
/// self-initiated exit apart from an unexplained death, so the user is told
/// their graph may be corrupt when in fact the daemon shut itself down. This
/// marker is what lets the refusal say what actually happened.
///
/// Best-effort and never fatal: it runs on the way out of a process that is
/// already giving up, so a failure here must not preempt the exit.
pub fn record_unclean_exit(infigraph_dir: &Path, reason: &str) {
    let payload = serde_json::json!({
        "pid": std::process::id(),
        "reason": reason,
        "phase": crate::write_phase::current().map(|(p, n)| format!("{p} (x{n})")),
        "at": now_epoch_secs(),
    });
    if let Ok(text) = serde_json::to_string_pretty(&payload) {
        let _ =
            crate::daemon_protocol::write_atomic(&unclean_exit_marker_path(infigraph_dir), &text);
    }
}

/// The recorded hard exit, if it was this `pid`.
///
/// Matched on pid so a stale marker from an older, unrelated exit cannot
/// explain away a genuinely unexplained death.
pub fn unclean_exit_for_pid(infigraph_dir: &Path, pid: u32) -> Option<UncleanExit> {
    let text = std::fs::read_to_string(unclean_exit_marker_path(infigraph_dir)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let recorded = u32::try_from(v.get("pid")?.as_u64()?).ok()?;
    if recorded != pid {
        return None;
    }
    Some(UncleanExit {
        pid: recorded,
        reason: v.get("reason")?.as_str()?.to_string(),
        phase: v.get("phase").and_then(|p| p.as_str()).map(str::to_string),
        at: v.get("at").and_then(|a| a.as_u64()).unwrap_or(0),
    })
}

/// Drop the marker once the graph it explains has been rebuilt.
pub fn clear_unclean_exit(infigraph_dir: &Path) {
    let _ = std::fs::remove_file(unclean_exit_marker_path(infigraph_dir));
}

pub fn crash_loop_marker_path(infigraph_dir: &Path) -> PathBuf {
    infigraph_dir.join(CRASH_LOOP_MARKER_FILE)
}

pub fn write_crash_loop_marker(infigraph_dir: &Path, attempts: &[u64]) -> Result<()> {
    let payload = serde_json::json!({ "attempts": attempts, "detected_at": now_epoch_secs() });
    crate::daemon_protocol::write_atomic(
        &crash_loop_marker_path(infigraph_dir),
        &serde_json::to_string_pretty(&payload)?,
    )
}

/// `Some(prior attempt timestamps)` once the breaker has tripped. A human
/// clears this by deleting the marker file after fixing the underlying
/// cause -- matching the existing "delete the lock if you don't expect a
/// watcher here" convention `doctor`'s remediation hints already use.
pub fn crash_loop_detected(infigraph_dir: &Path) -> Option<Vec<u64>> {
    let content = std::fs::read_to_string(crash_loop_marker_path(infigraph_dir)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("attempts")?
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
}

// ---------------------------------------------------------------------------
// The open window: a process that dies INSIDE `Database::new`
// ---------------------------------------------------------------------------
//
// `write_phase`'s breadcrumb sees only SIGABRT, and the rebuild breaker above
// counts rebuilds. Neither sees a SIGSEGV in lbug's WAL replay: the daemon
// dies, the next start opens the same graph and the same WAL, and dies the
// same way, with nothing in either record. So a process writes a sentinel
// before it opens the graph and removes it after; a sentinel whose writer is
// gone is one death of an opener, whatever the signal.

const OPEN_SENTINEL_PREFIX: &str = "graph-opening.";
const OPEN_DEATHS_LOG: &str = "open-deaths.log";

/// Test hook: while the file this variable names exists, die by SIGSEGV
/// inside the window, as lbug's replay did. A file holding `once` is removed
/// by the process that dies on it, so the start after it is healthy.
#[cfg(unix)]
pub const TEST_DIE_IN_OPEN_WINDOW_ENV: &str = "INFIGRAPH_TEST_DIE_IN_OPEN_WINDOW";

static OPEN_WINDOW_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The span of one `Database::new` on the main graph's WRITER open. A
/// read-only open (`GraphStore::open_read_only`, the CLI's and the daemon's
/// per-request reads) does not enter one: a reader that dies mid-open leaves
/// no sentinel, so a crash there is invisible to this breaker -- do not assume
/// a reader is covered. Opens are per request in the daemon, and a sentinel
/// per read would put a file write on every one. Leaving it by any
/// return path removes the sentinel; only [`OpenWindow::opened`] also clears
/// the death log, because only a completed open ends a loop.
pub struct OpenWindow {
    sentinel: PathBuf,
    dir: PathBuf,
}

impl OpenWindow {
    /// Enter the window. A process that cannot write the sentinel opens
    /// anyway: the breaker is a safety net, not a gate.
    pub fn enter(infigraph_dir: &Path) -> OpenWindow {
        let seq = OPEN_WINDOW_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();
        let sentinel = infigraph_dir.join(format!("{OPEN_SENTINEL_PREFIX}{pid}.{seq}"));
        let payload = serde_json::json!({
            "pid": pid,
            "started_at": crate::instances::current_process_start_time(pid).unwrap_or(0),
            "at": now_epoch_secs(),
        });
        let _ = crate::daemon_protocol::write_atomic(&sentinel, &payload.to_string());
        #[cfg(unix)]
        if let Some(trigger) =
            std::env::var_os(TEST_DIE_IN_OPEN_WINDOW_ENV).filter(|f| Path::new(f).exists())
        {
            if std::fs::read_to_string(&trigger).is_ok_and(|t| t.trim() == "once") {
                let _ = std::fs::remove_file(&trigger);
            }
            // SAFETY: restores the default action and raises; Rust's own
            // SIGSEGV handler would otherwise swallow a raise() that is not a
            // real fault.
            unsafe {
                libc::signal(libc::SIGSEGV, libc::SIG_DFL);
                libc::raise(libc::SIGSEGV);
            }
        }
        OpenWindow {
            sentinel,
            dir: infigraph_dir.to_path_buf(),
        }
    }

    /// The open returned: the loop, if there was one, is over.
    pub fn opened(self) {
        let _ = std::fs::remove_file(self.dir.join(OPEN_DEATHS_LOG));
    }
}

impl Drop for OpenWindow {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sentinel);
    }
}

/// Whether the process that wrote an open sentinel is gone. A recorded start
/// time tells a reused pid from the original (`instances`, as `lockfile`).
fn opener_is_gone(sentinel: &serde_json::Value) -> bool {
    let Some(pid) = sentinel
        .get("pid")
        .and_then(|p| p.as_u64())
        .and_then(|p| u32::try_from(p).ok())
    else {
        return true;
    };
    if pid == std::process::id() {
        return false;
    }
    let recorded = sentinel
        .get("started_at")
        .and_then(|s| s.as_u64())
        .unwrap_or(0);
    match crate::instances::current_process_start_time(pid) {
        None => true,
        Some(actual) => recorded != 0 && actual != recorded,
    }
}

/// Timestamps of opener deaths still inside the crash-loop window, after
/// folding in every sentinel whose writer is gone (each is removed as it is
/// counted, so two sweepers cannot count one death twice).
pub fn open_deaths(infigraph_dir: &Path) -> Vec<u64> {
    use std::io::Write;
    if let Ok(entries) = std::fs::read_dir(infigraph_dir) {
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(OPEN_SENTINEL_PREFIX)
            {
                continue;
            }
            let gone = std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .is_none_or(|v| opener_is_gone(&v));
            if gone && std::fs::remove_file(entry.path()).is_ok() {
                if let Ok(mut log) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(infigraph_dir.join(OPEN_DEATHS_LOG))
                {
                    let _ = writeln!(log, "{}", now_epoch_secs());
                }
            }
        }
    }
    let cutoff = now_epoch_secs().saturating_sub(CRASH_LOOP_WINDOW.as_secs());
    std::fs::read_to_string(infigraph_dir.join(OPEN_DEATHS_LOG))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .filter(|&ts| ts >= cutoff)
        .collect()
}

/// What to do before opening `graph_name` after earlier openers died inside
/// the open. None died: nothing. Some did: the WAL is the usual cause, so ask
/// `recover` (the out-of-process probe in production) to set a torn one aside
/// -- `graph.torn-wal.<ts>.wal.checkpoint` -- and open without it. A graph
/// that opens in neither state is `GraphCorruption`, which `CodeIndex::init`
/// answers with its rebuild; one a probe opens fine but that keeps killing
/// its opener trips the existing crash-loop marker instead of looping.
pub fn settle_open_deaths(
    infigraph_dir: &Path,
    graph_name: &str,
    recover: &dyn Fn(&Path, &str) -> crate::quarantine::WalRecovery,
) -> Result<()> {
    use crate::quarantine::WalRecovery;
    let deaths = open_deaths(infigraph_dir);
    if deaths.is_empty() {
        return Ok(());
    }
    eprintln!(
        "[graph] {} earlier open(s) of {graph_name} died inside the open (a fatal signal, not an \
         error); checking it before opening again",
        deaths.len()
    );
    match recover(infigraph_dir, graph_name) {
        WalRecovery::Recovered => Ok(()),
        WalRecovery::NotRecoverable => {
            Err(anyhow::Error::new(crate::graph::GraphCorruption {
                detail: format!(
                    "opening {graph_name} has killed the process {} time(s) and it does not open                      in a probe process either, with its WAL or without",
                    deaths.len()
                ),
            }))
        }
        WalRecovery::AlreadyHealthy if deaths.len() >= CRASH_LOOP_THRESHOLD => {
            write_crash_loop_marker(infigraph_dir, &deaths)?;
            let _ = std::fs::remove_file(infigraph_dir.join(OPEN_DEATHS_LOG));
            anyhow::bail!(
                "crash-loop detected: opening {graph_name} has killed the process {} times within \
                 the last hour, though a probe process opens it -- refusing to open it again. \
                 Look at daemon.log and the crash reports, then delete {} to retry.",
                deaths.len(),
                crash_loop_marker_path(infigraph_dir).display()
            )
        }
        WalRecovery::AlreadyHealthy => Ok(()),
    }
}

/// Most recent cleanly-retired healthy graph for `graph_name`, if any --
/// the degrade fallback (R3.1.4b).
pub fn find_most_recent_previous(infigraph_dir: &Path, graph_name: &str) -> Option<PathBuf> {
    list_restore_points(infigraph_dir)
        .into_iter()
        .find(|p| {
            matches!(p.kind, RestorePointKind::Previous)
                && p.path
                    .file_name()
                    .map(|n| n.to_string_lossy().starts_with(graph_name))
                    .unwrap_or(false)
        })
        .map(|p| p.path)
}

/// Whether a pending dead-holder-WAL sentinel wants a rebuild -- the
/// daemon coordinator's integration point, asked every tick. `true` while a
/// sentinel is pending and under the crash-loop threshold: the coordinator
/// queues a `FullReindex`, the same request type and code path `infigraph
/// rebuild` uses. The sentinel is *not* cleared here, only once a rebuild
/// starts ([`rebuild_started`]): the queued request lives in the
/// coordinator's memory, so a dropped request or a daemon restart must
/// leave the sentinel behind to ask again (#204). At or over the threshold,
/// writes the crash-loop marker, clears the sentinel and answers `false`.
pub fn recovery_rebuild_wanted(infigraph_dir: &Path) -> Result<bool> {
    if !pending_recovery(infigraph_dir) {
        return Ok(false);
    }

    let attempts = recent_recovery_attempts(infigraph_dir)?;
    if attempts.len() >= CRASH_LOOP_THRESHOLD {
        write_crash_loop_marker(infigraph_dir, &attempts)?;
        crate::audit::audit_log(
            "recovery",
            "crash-loop-detected",
            &format!("{} auto-rebuilds within the last hour", attempts.len()),
            &infigraph_dir.display().to_string(),
        );
        clear_recovery_needed(infigraph_dir)?;
        return Ok(false);
    }
    Ok(true)
}

/// A full reindex has started. If a recovery sentinel is pending, this is
/// its rebuild: record the attempt (the crash-loop budget counts rebuilds
/// that ran, not requests) and clear the sentinel. Any rebuild answers it --
/// a client's included.
pub fn rebuild_started(infigraph_dir: &Path) -> Result<()> {
    if !pending_recovery(infigraph_dir) {
        return Ok(());
    }
    record_recovery_attempt(infigraph_dir)?;
    crate::audit::audit_log(
        "recovery",
        "auto-triggered-full-reindex",
        "dead-holder WAL quarantined; auto-rebuild triggered",
        &infigraph_dir.display().to_string(),
    );
    clear_recovery_needed(infigraph_dir)
}

#[cfg(test)]
mod tests {

    /// #146 round trip: a hard exit records itself, including whatever lbug
    /// write was in flight, and only answers for its own pid.
    #[test]
    fn record_unclean_exit_round_trips_with_the_in_flight_phase() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();

        {
            let _phase = crate::write_phase::enter(&"scip-import", 113_000);
            record_unclean_exit(dir.path(), "daemon graceful shutdown exceeded its budget");
        }

        let exit = unclean_exit_for_pid(dir.path(), me).expect("recorded for this process");
        assert_eq!(exit.pid, me);
        assert_eq!(exit.reason, "daemon graceful shutdown exceeded its budget");
        let phase = exit.phase.expect("the in-flight write must be captured");
        assert!(phase.contains("scip-import"), "{phase}");
        assert!(phase.contains("113000"), "and how far in: {phase}");

        assert!(
            unclean_exit_for_pid(dir.path(), me.wrapping_add(1)).is_none(),
            "a marker must not explain a different process's death"
        );

        clear_unclean_exit(dir.path());
        assert!(unclean_exit_for_pid(dir.path(), me).is_none());
    }

    /// The marker records the reason even when this test is not itself
    /// inside a write.
    ///
    /// It deliberately does NOT assert `exit.phase.is_none()`. `write_phase`
    /// keeps one process-global slot rather than a `thread_local!`, because a
    /// signal handler cannot safely read thread-local storage (see that
    /// module's docs). So "no write in flight" is a property of the whole
    /// test binary, not of this test: any of the sibling tests writing to a
    /// graph populates that slot concurrently. Asserting it empty passed on
    /// macOS purely by scheduling luck and failed on Linux. The
    /// with-a-phase case is covered deterministically by the sibling test,
    /// which enters a phase itself.
    #[test]
    fn record_unclean_exit_with_no_write_in_flight_still_records_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        record_unclean_exit(dir.path(), "stale build self-check");
        let exit = unclean_exit_for_pid(dir.path(), std::process::id()).unwrap();
        assert_eq!(exit.reason, "stale build self-check");
    }
    use super::*;

    #[test]
    fn sentinel_round_trips_and_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert!(!pending_recovery(dir));
        mark_recovery_needed(dir, 4242, &dir.join("graph.corrupt.1")).unwrap();
        assert!(pending_recovery(dir));
        clear_recovery_needed(dir).unwrap();
        assert!(!pending_recovery(dir));
    }

    #[test]
    fn attempts_outside_the_window_are_not_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir).unwrap();
        // A timestamp far outside the 1h window, written directly (bypassing
        // record_recovery_attempt, which always stamps "now").
        std::fs::write(dir.join("recovery-attempts.log"), "100\n").unwrap();
        assert!(recent_recovery_attempts(dir).unwrap().is_empty());
    }

    #[test]
    fn a_pending_sentinel_asks_for_a_rebuild_until_one_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        mark_recovery_needed(dir, 1, &dir.join("graph.corrupt.1")).unwrap();

        // Review finding (#204): asking is not doing. The request lives only
        // in the coordinator's memory, so the sentinel -- which survives a
        // restart, and a dropped request -- stays until a rebuild starts.
        assert!(recovery_rebuild_wanted(dir).unwrap(), "a rebuild is wanted");
        assert!(pending_recovery(dir), "still pending until one starts");
        assert!(
            recovery_rebuild_wanted(dir).unwrap(),
            "so it is asked again"
        );
        assert!(recent_recovery_attempts(dir).unwrap().is_empty());
        assert!(
            !dir.join("requests").exists(),
            "and no file is written for it"
        );

        rebuild_started(dir).unwrap();
        assert!(!pending_recovery(dir), "a started rebuild clears it");
        assert_eq!(recent_recovery_attempts(dir).unwrap().len(), 1);
        assert!(!recovery_rebuild_wanted(dir).unwrap());
        rebuild_started(dir).unwrap();
        assert_eq!(
            recent_recovery_attempts(dir).unwrap().len(),
            1,
            "a rebuild with no sentinel pending is not a recovery attempt"
        );
        assert!(crash_loop_detected(dir).is_none());
    }

    #[test]
    fn a_pending_sentinel_trips_the_breaker_at_the_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for _ in 0..CRASH_LOOP_THRESHOLD {
            record_recovery_attempt(dir).unwrap();
        }
        mark_recovery_needed(dir, 1, &dir.join("graph.corrupt.99")).unwrap();

        assert!(
            !recovery_rebuild_wanted(dir).unwrap(),
            "no rebuild once tripped"
        );

        assert!(!pending_recovery(dir), "sentinel must still be cleared");
        assert!(
            crash_loop_detected(dir).is_some(),
            "must trip after CRASH_LOOP_THRESHOLD prior attempts in the window"
        );
    }

    #[test]
    fn find_most_recent_previous_prefers_the_newest_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("graph.previous.100"), b"old").unwrap();
        std::fs::write(dir.join("graph.previous.200"), b"new").unwrap();
        let found = find_most_recent_previous(dir, "graph").unwrap();
        assert_eq!(found.file_name().unwrap(), "graph.previous.200");
    }
}
