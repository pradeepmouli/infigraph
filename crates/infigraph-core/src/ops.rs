//! Operation-scoped locks: coarser than the per-call graph write lock,
//! held across a whole logical operation (an index run, a SCIP import, a
//! watcher batch) so two operations never interleave their write batches.
//! The fine-grained `graph.lock` remains the corruption floor beneath.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;

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
/// Callers are responsible for their own `sessions/` preserve-across-wipe
/// dance where that applies (rename out, wipe, rename back) — this only
/// handles the lock-safe wipe of everything else. Callers should check
/// `tg_dir.exists()` before calling, matching the existing call-site
/// convention (a missing directory is a no-op, not an error here).
pub fn wipe_infigraph_preserving_index_lock(tg_dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(tg_dir)?.flatten() {
        if entry.file_name() == "index.lock" {
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
