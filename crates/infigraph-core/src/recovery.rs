//! Automatic recovery from a dead-holder WAL (R3.1.4a, #115) and its
//! crash-loop breaker (R3.1.4c). Two pieces of state persist under
//! `.infigraph/`, mirroring the short-lived-file-then-append-log shape
//! `dirty.rs`/`audit.rs` already use elsewhere in this crate: a
//! "recovery needed" sentinel the daemon coordinator polls for
//! (`drain_recovery_sentinel`, called from `daemon::run_write_coordinator`),
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

/// The daemon coordinator's one-line integration point (called from
/// `daemon::run_write_coordinator`'s `serve_requests` tick). No-op when no
/// sentinel is pending. Otherwise: under the crash-loop threshold, submits
/// a synthetic `WriteRequest::FullReindex` request file (the SAME request
/// type and code path `infigraph index --full` uses) for this same tick's
/// existing request-directory scan to pick up; at or over the threshold,
/// writes the crash-loop marker instead and submits nothing.
pub fn drain_recovery_sentinel(infigraph_dir: &Path) -> Result<()> {
    if !pending_recovery(infigraph_dir) {
        return Ok(());
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
        return clear_recovery_needed(infigraph_dir);
    }

    let requests_dir = infigraph_dir.join("requests");
    let request_path = requests_dir.join("auto-recovery.request");
    let serialized = serde_json::to_string(&crate::daemon_protocol::WriteRequest::FullReindex)
        .expect("WriteRequest::FullReindex always serializes");
    crate::daemon_protocol::write_atomic(&request_path, &serialized)?;
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
    fn drain_recovery_sentinel_submits_a_full_reindex_request_under_the_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        mark_recovery_needed(dir, 1, &dir.join("graph.corrupt.1")).unwrap();

        drain_recovery_sentinel(dir).unwrap();

        assert!(!pending_recovery(dir), "sentinel must be cleared");
        let requested: Vec<_> = std::fs::read_dir(dir.join("requests"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            requested.iter().any(|n| n.ends_with(".request")),
            "expected a .request file, got {requested:?}"
        );
        assert_eq!(recent_recovery_attempts(dir).unwrap().len(), 1);
        assert!(crash_loop_detected(dir).is_none());
    }

    #[test]
    fn drain_recovery_sentinel_trips_the_breaker_at_the_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for _ in 0..CRASH_LOOP_THRESHOLD {
            record_recovery_attempt(dir).unwrap();
        }
        mark_recovery_needed(dir, 1, &dir.join("graph.corrupt.99")).unwrap();

        drain_recovery_sentinel(dir).unwrap();

        assert!(!pending_recovery(dir), "sentinel must still be cleared");
        assert!(
            crash_loop_detected(dir).is_some(),
            "must trip after CRASH_LOOP_THRESHOLD prior attempts in the window"
        );
        let requested = std::fs::read_dir(dir.join("requests"))
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(
            requested, 0,
            "must NOT submit another FullReindex once tripped"
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
