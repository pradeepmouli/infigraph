//! Operation-scoped locks: coarser than the per-call graph write lock,
//! held across a whole logical operation (an index run, a SCIP import, a
//! watcher batch) so two operations never interleave their write batches.
//! The fine-grained `graph.lock` remains the corruption floor beneath.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::lockfile::{self, LockFile, LockInfo};

#[derive(Debug)]
pub struct IndexOpGuard {
    _lock: LockFile,
}

#[derive(Debug)]
pub enum IndexOpOutcome {
    Acquired(IndexOpGuard),
    /// Lock held by a live operation; holder identity when readable.
    AlreadyRunning(Option<LockInfo>),
}

impl IndexOpOutcome {
    pub fn skip_note(&self) -> Option<String> {
        match self {
            IndexOpOutcome::Acquired(_) => None,
            IndexOpOutcome::AlreadyRunning(Some(h)) => {
                let started = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs().saturating_sub(h.acquired_at))
                    .unwrap_or(0);
                Some(format!(
                    "index already in progress ({}, PID {}, started {}s ago) — skipped",
                    h.role, h.pid, started
                ))
            }
            IndexOpOutcome::AlreadyRunning(None) => {
                Some("index already in progress (unknown holder) — skipped".to_string())
            }
        }
    }

    /// Same reason as `skip_note`, without the trailing "— skipped" suffix —
    /// for callers that compose their own "skipped" phrasing (e.g. group
    /// operations reporting one line per member: "repo: skipped — <reason>").
    pub fn skip_reason(&self) -> Option<String> {
        self.skip_note()
            .map(|n| n.trim_end_matches(" — skipped").to_string())
    }
}

fn index_lock_path(root: &Path) -> std::path::PathBuf {
    root.join(".infigraph").join("index.lock")
}

/// Whether `index.lock` is currently held by this same process. Used by
/// the watch daemon's shutdown watchdog (R5.4/#79) to tell "still doing a
/// legitimately long write" apart from "wedged" -- both look identical
/// from the outside (a graceful shutdown that hasn't returned yet), but
/// only one of them is safe to hard-kill. A full reindex can take
/// minutes, so a fixed timeout can't make that call on its own; whether
/// `begin_index_op`'s guard is still held by us is the same signal
/// `watch_project_with_periodic`'s own shutdown path already waits on
/// before it will let the process return.
pub fn index_op_held_by_self(root: &Path) -> bool {
    lockfile::read_holder(&index_lock_path(root)).is_some_and(|h| h.pid == std::process::id())
}

pub fn begin_index_op(root: &Path, role: &str, wait: Duration) -> Result<IndexOpOutcome> {
    let path = index_lock_path(root);
    if wait.is_zero() {
        match lockfile::try_acquire(&path, role)? {
            Some(lock) => Ok(IndexOpOutcome::Acquired(IndexOpGuard { _lock: lock })),
            None => Ok(IndexOpOutcome::AlreadyRunning(lockfile::read_holder(&path))),
        }
    } else {
        let lock = lockfile::acquire(&path, role, wait)?;
        Ok(IndexOpOutcome::Acquired(IndexOpGuard { _lock: lock }))
    }
}

/// Wipe everything under a `.infigraph/` directory (`tg_dir`) except
/// `index.lock` itself, for a `--full` reindex.
///
/// A flock is held on the underlying inode, not the path — deleting the
/// lock file out from under a live `IndexOpGuard` would just unlink that
/// name, so a second process could then create a fresh `index.lock` and
/// acquire an uncontended lock on it for the rest of the reindex,
/// defeating the mutual exclusion the lock exists to provide. This walks
/// the directory and removes everything *except* `index.lock` (kept in
/// place as the live rendezvous point), so a lock held on it by the
/// caller stays valid for the whole wipe-and-reindex.
///
/// Also preserves the snapshot/quarantine/retired-previous backup pools
/// (`crate::snapshot::should_skip`, R3.2) — a caller that just took a
/// pre-write snapshot of this same directory (see `full_reindex_wipe`)
/// would otherwise have that snapshot destroyed by the wipe it was meant to
/// protect against, along with any existing corruption-quarantine or
/// retired-previous-graph entries.
///
/// Callers are responsible for their own `sessions/` preserve-across-wipe
/// dance where that applies (rename out, wipe, rename back) — this only
/// handles the lock-safe wipe of everything else. Callers should check
/// `tg_dir.exists()` before calling, matching the existing call-site
/// convention (a missing directory is a no-op, not an error here).
pub fn wipe_infigraph_preserving_index_lock(tg_dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(tg_dir)?.flatten() {
        if crate::snapshot::should_skip(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// The full snapshot-then-wipe sequence shared by every full-reindex call
/// site that destructively clears `.infigraph/` for a from-scratch rebuild
/// (R3.2.1/docs/DESIGN-hardening.md §3.2): snapshot the current state,
/// preserve `sessions/` across the wipe (conversation history, not derived
/// index data, so it isn't backed up by the snapshot's restore semantics),
/// wipe, restore sessions. A failed snapshot aborts before anything
/// destructive runs, rather than proceeding with an unprotected wipe.
///
/// Callers are responsible for holding whatever lock serializes writes to
/// `tg_dir` before calling this (typically `index.lock` via
/// `begin_index_op`) — mirrors `snapshot::create_snapshot`'s own contract.
/// That lock alone is *not* sufficient, though: `index.lock` only coalesces
/// callers that go through `begin_index_op` (full/incremental `index()`,
/// group/multi-repo builds) — it does not cover `GraphStore::upsert_file`,
/// which only takes the finer-grained `graph.lock` and is reachable from the
/// public `Infigraph::index_file` API. Without also taking `graph.lock`
/// here, a concurrent single-file write could run against the graph while
/// this function is mid-copy or mid-delete (caught by adversarial review
/// before this shipped). Acquiring it here blocks that class of writer out
/// for the duration of the snapshot+wipe, same as every other graph writer.
pub fn full_reindex_wipe(tg_dir: &Path) -> Result<()> {
    let graph_lock_path = crate::graph::store::db_lock_path(&tg_dir.join("graph"));
    let _graph_lock = crate::lockfile::acquire(
        &graph_lock_path,
        "full-reindex-wipe",
        std::time::Duration::from_secs(30),
    )
    .context("could not acquire the graph write lock for the pre-reindex snapshot+wipe")?;

    crate::snapshot::create_snapshot(tg_dir)
        .context("pre-reindex snapshot failed; aborting full reindex")?;

    let sessions_dir = tg_dir.join("sessions");
    let sessions_backup = tg_dir
        .parent()
        .unwrap_or(tg_dir)
        .join(".infigraph-sessions-backup");
    let had_sessions = sessions_dir.exists();
    if had_sessions {
        let _ = std::fs::rename(&sessions_dir, &sessions_backup);
    }

    let wipe_result = wipe_infigraph_preserving_index_lock(tg_dir).context("wipe failed");

    if had_sessions {
        let _ = std::fs::rename(&sessions_backup, &sessions_dir);
    }

    wipe_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_op_held_by_self_is_false_with_no_lock_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!index_op_held_by_self(tmp.path()));
    }

    /// The exact signal the daemon shutdown watchdog (R5.4/#79) relies on:
    /// while `begin_index_op`'s guard is alive, `index_op_held_by_self` must
    /// report true so the watchdog keeps deferring instead of hard-killing
    /// a process mid-write.
    #[test]
    fn index_op_held_by_self_is_true_while_the_guard_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();

        let outcome = begin_index_op(tmp.path(), "test-op", Duration::ZERO).unwrap();
        let IndexOpOutcome::Acquired(guard) = outcome else {
            panic!("expected to acquire an uncontended index.lock");
        };

        assert!(
            index_op_held_by_self(tmp.path()),
            "index_op_held_by_self must be true while this process holds index.lock"
        );

        drop(guard);

        assert!(
            !index_op_held_by_self(tmp.path()),
            "index_op_held_by_self must go false once the guard releases the lock"
        );
    }
}
