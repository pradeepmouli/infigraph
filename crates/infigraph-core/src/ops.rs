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
/// `run_write_coordinator`'s own shutdown path already waits on
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
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if crate::snapshot::should_skip(&name) || kept_across_a_code_rebuild(&name) {
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

/// What a from-scratch *code* rebuild leaves alone in `.infigraph/`: the
/// project's own `config.toml` (`[docs] enabled`, `[index] include`, ...),
/// which is the user's rather than derived, and the document index with its
/// sidecars and lock, which the code graph does not feed. Wiping the docs
/// lock while it is held would also split it.
fn kept_across_a_code_rebuild(name: &str) -> bool {
    name == "config.toml"
        || name == "config.lock"
        || name == crate::docs_switch::DOCS_OP_LOCK
        || is_docs_store_entry(name)
}

/// The document index and its sidecars (`docs.kuzu*`, `docs_*`).
fn is_docs_store_entry(name: &str) -> bool {
    name.starts_with("docs.kuzu") || name.starts_with("docs_")
}

/// What `infigraph worktree clean` makes of one entry of `.infigraph/`.
/// Everything not named here is [`EntryClass::Kept`]: the list is an allow-list,
/// so a file nobody listed survives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryClass {
    /// Rebuilt by `infigraph index`: the graph, its sidecars, locks, logs and
    /// scratch directories, and the last-run records of the runs that built it.
    Derived,
    /// The document index and its sidecars: derived, but it can hold Confluence
    /// pages and manifest nodes that need credentials and a network to bring
    /// back, and documents are opt-in, so it is removed only on request.
    DocsStore,
    /// Snapshots, quarantined and retired graphs: a user's safety nets.
    RestorePoint,
    /// The user's: `config.toml`, `sessions/`, `structured-schemas/`, `learned/`,
    /// and anything else not listed.
    Kept,
}

/// Names, exactly, that a rebuild of the graph recreates. The lock files are
/// [`crate::ps::PROJECT_LOCKS`], not repeated here.
const DERIVED_NAMES: &[&str] = &[
    "bm25_cache.bin",
    "dedup_state.json",
    "dirty.lock",
    "logs",
    "requests",
    "scip-enrich.log",
    "scip-imports.json",
    "scip-tmp",
    "daemon.log",
    "worktree-hook.log",
    crate::daemon::writes::SIDECAR_DIR,
];

/// Names that start with these are derived too: the embeddings file and the
/// sidecars written beside it, the vector index, and the last-run records of
/// the runs that built what is being removed.
const DERIVED_PREFIXES: &[&str] = &["embeddings.bin", "hnsw_index.usearch", "last-run."];

/// Sort one entry of a project's `.infigraph/`. An allow-list: only what is
/// named in [`DERIVED_NAMES`], [`DERIVED_PREFIXES`], the lock list, the graph
/// itself and the two restore/docs predicates is anything but `Kept`.
pub fn classify_entry(name: &str) -> EntryClass {
    if crate::snapshot::is_restore_pool_entry(name) {
        return EntryClass::RestorePoint;
    }
    // `docs-op.lock` and `docs.kuzu.lock` are in the lock list *and* part of
    // the document store: they go with it.
    if is_docs_store_entry(name) || name == crate::docs_switch::DOCS_OP_LOCK {
        return EntryClass::DocsStore;
    }
    let derived = name == "graph"
        || name.starts_with("graph.")
        || crate::ps::PROJECT_LOCKS.contains(&name)
        || DERIVED_NAMES.contains(&name)
        || DERIVED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix));
    if derived {
        EntryClass::Derived
    } else {
        EntryClass::Kept
    }
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
    fn the_graph_and_its_sidecars_are_derived() {
        for name in [
            "graph",
            "graph.wal",
            "graph.health.json",
            "graph.health.recorded",
            "graph.ckpt.lock",
            "bm25_cache.bin",
            "embeddings.bin",
            "embeddings.bin.generation",
            "hnsw_index.usearch",
            "hnsw_index.usearch.meta",
            "write-tmp",
            "scip-tmp",
            "scip-imports.json",
            "scip-enrich.log",
            "daemon.log",
            "worktree-hook.log",
            "dedup_state.json",
            "dirty.lock",
            "requests",
            "logs",
            "last-run.index.json",
            "last-run.embeddings.json",
        ] {
            assert_eq!(classify_entry(name), EntryClass::Derived, "{name}");
        }
    }

    #[test]
    fn every_lock_a_process_can_hold_is_derived_except_the_configs() {
        for lock in crate::ps::PROJECT_LOCKS {
            let class = classify_entry(lock);
            assert!(
                class == EntryClass::Derived || class == EntryClass::DocsStore,
                "{lock} classed {class:?}"
            );
        }
        assert_eq!(classify_entry("config.lock"), EntryClass::Kept);
    }

    #[test]
    fn the_docs_store_is_its_own_class() {
        for name in [
            "docs.kuzu",
            "docs.kuzu.wal",
            "docs.kuzu.lock",
            "docs_embeddings.bin",
            "docs_bm25_cache.bin",
        ] {
            assert_eq!(classify_entry(name), EntryClass::DocsStore, "{name}");
        }
    }

    #[test]
    fn the_restore_pools_are_their_own_class() {
        for name in [
            "snapshots",
            "graph.corrupt.1790000000",
            "graph.corrupt.1790000000.wal",
            "graph.previous.1790000000",
        ] {
            assert_eq!(classify_entry(name), EntryClass::RestorePoint, "{name}");
        }
    }

    #[test]
    fn what_nobody_listed_is_kept() {
        for name in [
            "config.toml",
            "sessions",
            "structured-schemas",
            "learned",
            "my-notes.txt",
            "graph-notes.md",
            ".infigraph-sessions-backup",
        ] {
            assert_eq!(classify_entry(name), EntryClass::Kept, "{name}");
        }
    }

    #[test]
    fn what_a_code_rebuild_keeps_is_never_derived() {
        // The keep-list of the rebuild wipe and this allow-list must not
        // overlap, except for the docs store, which a rebuild keeps and a
        // clean removes only when asked.
        for name in [
            "config.toml",
            "config.lock",
            crate::docs_switch::DOCS_OP_LOCK,
        ] {
            assert!(kept_across_a_code_rebuild(name));
            assert_ne!(classify_entry(name), EntryClass::Derived, "{name}");
        }
    }

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
