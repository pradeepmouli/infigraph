//! Persistent dirty-file set, shared across watcher/index/hook triggers
//! (R3.3.5, docs/DESIGN-hardening.md §3.3.5, github.com/pradeepmouli/infigraph#28).
//!
//! The watcher's own batching (`watch::batch::ChangeBatch`) exists only in
//! the watching process's memory: a crash between a file write and the
//! batch's debounce-window flush loses that pending-reindex signal
//! entirely. This module gives that signal a crash-durable home so a
//! restarted watcher (or any other trigger) can recover the exact set of
//! paths left mid-flight instead of falling back to a full rescan.
//!
//! Storage is a single append-only log, `.infigraph/dirty.log` (one
//! relative path per line), guarded by a short-held `.infigraph/dirty.lock`
//! -- not `index.lock`, which is held for the duration of a whole
//! index/drain operation and would make every single mark contend with it.
//! Appending is O(1) per mark (matches the per-fsevent call granularity);
//! compaction (deduping, dropping cleared entries) only happens inside
//! `clear_dirty`, at drain granularity, not per mark.
//!
//! Contract: `mark_dirty` records that a path's graph state may be stale.
//! `clear_dirty` must only be called once the caller has *confirmed* that
//! path's graph state was actually written (a successful upsert or
//! removal) -- clearing on mere hand-off to a batch would reopen the exact
//! crash window this module exists to close. `pending_dirty` never
//! mutates the log. All three are idempotent: marking an already-dirty
//! path, clearing an already-clear path, or clearing/marking an empty
//! slice are safe no-ops.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DIRTY_LOG_NAME: &str = "dirty.log";
const DIRTY_LOCK_NAME: &str = "dirty.lock";
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

fn dirty_log_path(infigraph_dir: &Path) -> PathBuf {
    infigraph_dir.join(DIRTY_LOG_NAME)
}

fn dirty_lock_path(infigraph_dir: &Path) -> PathBuf {
    infigraph_dir.join(DIRTY_LOCK_NAME)
}

fn parse_lines(content: &str) -> impl Iterator<Item = &str> {
    content.lines().filter(|l| !l.is_empty())
}

/// Mark `rel_paths` dirty: append a durable record that each path's graph
/// state may not match reality yet. Duplicate marks (a path already
/// pending) are harmless -- `pending_dirty` dedupes, and `clear_dirty`
/// compacts the log.
pub fn mark_dirty(infigraph_dir: &Path, rel_paths: &[String]) -> Result<()> {
    if rel_paths.is_empty() {
        return Ok(());
    }
    // Never create `.infigraph/` here. No `.infigraph/` means no indexed
    // project, so there is no graph state for these paths to be stale
    // against; and creating it is exactly how a watcher's last events,
    // delivered after the project directory was removed, resurrected the
    // root under the same path and kept a daemon alive on it (#136).
    if !infigraph_dir.is_dir() {
        return Ok(());
    }
    let _lock =
        crate::lockfile::acquire(&dirty_lock_path(infigraph_dir), "dirty-mark", LOCK_TIMEOUT)
            .context("acquire dirty-set lock for mark")?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dirty_log_path(infigraph_dir))
        .context("open dirty.log for append")?;
    for p in rel_paths {
        writeln!(f, "{p}").context("append to dirty.log")?;
    }
    f.sync_all().context("fsync dirty.log")?;
    Ok(())
}

/// Read the current pending dirty set. Does not clear anything -- repeated
/// calls with no intervening `mark_dirty`/`clear_dirty` return the same
/// set.
pub fn pending_dirty(infigraph_dir: &Path) -> Result<HashSet<String>> {
    let path = dirty_log_path(infigraph_dir);
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let _lock =
        crate::lockfile::acquire(&dirty_lock_path(infigraph_dir), "dirty-read", LOCK_TIMEOUT)
            .context("acquire dirty-set lock for read")?;
    let content = std::fs::read_to_string(&path).context("read dirty.log")?;
    Ok(parse_lines(&content).map(str::to_string).collect())
}

/// Clear `rel_paths` from the pending dirty set. Callers must only pass
/// paths whose graph state they have *confirmed* was written -- see the
/// module-level contract. Also compacts the log (dedupes, drops cleared
/// entries), so repeated marks for the same hot path don't grow it
/// unboundedly. Idempotent: clearing a path that isn't pending, or calling
/// with an empty slice, is a safe no-op.
pub fn clear_dirty(infigraph_dir: &Path, rel_paths: &[String]) -> Result<()> {
    if rel_paths.is_empty() {
        return Ok(());
    }
    let path = dirty_log_path(infigraph_dir);
    if !path.exists() {
        return Ok(());
    }
    let _lock =
        crate::lockfile::acquire(&dirty_lock_path(infigraph_dir), "dirty-clear", LOCK_TIMEOUT)
            .context("acquire dirty-set lock for clear")?;
    let content = std::fs::read_to_string(&path).context("read dirty.log")?;
    let to_clear: HashSet<&str> = rel_paths.iter().map(String::as_str).collect();
    let mut remaining: Vec<&str> = parse_lines(&content)
        .filter(|l| !to_clear.contains(l))
        .collect();
    remaining.sort_unstable();
    remaining.dedup();

    if remaining.is_empty() {
        std::fs::remove_file(&path).context("remove empty dirty.log")?;
    } else {
        let mut buf = String::new();
        for r in &remaining {
            buf.push_str(r);
            buf.push('\n');
        }
        crate::embed::atomic_write(&path, buf.as_bytes()).context("compact dirty.log")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_infigraph_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".infigraph");
        // An indexed project's `.infigraph/` already exists; `mark_dirty`
        // deliberately never creates it (see the #136 note there).
        std::fs::create_dir_all(&dir).unwrap();
        (tmp, dir)
    }

    /// #136: a mark against a root whose `.infigraph/` is gone (the watcher's
    /// last events after a project was removed) must not bring it back.
    #[test]
    fn mark_dirty_does_not_create_a_missing_infigraph_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gone").join(".infigraph");
        mark_dirty(&dir, &["a.py".to_string()]).unwrap();
        assert!(!dir.exists(), "mark_dirty must not resurrect .infigraph/");
        assert!(!tmp.path().join("gone").exists());
    }

    #[test]
    fn pending_dirty_is_empty_before_any_mark() {
        let (_tmp, dir) = tmp_infigraph_dir();
        assert!(pending_dirty(&dir).unwrap().is_empty());
    }

    #[test]
    fn mark_then_pending_reports_the_marked_paths() {
        let (_tmp, dir) = tmp_infigraph_dir();
        mark_dirty(&dir, &["a.py".to_string(), "b.py".to_string()]).unwrap();
        let pending = pending_dirty(&dir).unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.contains("a.py"));
        assert!(pending.contains("b.py"));
    }

    #[test]
    fn marking_an_already_dirty_path_is_a_harmless_duplicate() {
        let (_tmp, dir) = tmp_infigraph_dir();
        mark_dirty(&dir, &["a.py".to_string()]).unwrap();
        mark_dirty(&dir, &["a.py".to_string()]).unwrap();
        let pending = pending_dirty(&dir).unwrap();
        assert_eq!(
            pending.len(),
            1,
            "pending_dirty must dedupe repeated marks of the same path"
        );
    }

    #[test]
    fn clear_removes_only_the_named_paths() {
        let (_tmp, dir) = tmp_infigraph_dir();
        mark_dirty(
            &dir,
            &["a.py".to_string(), "b.py".to_string(), "c.py".to_string()],
        )
        .unwrap();
        clear_dirty(&dir, &["b.py".to_string()]).unwrap();
        let pending = pending_dirty(&dir).unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.contains("a.py"));
        assert!(pending.contains("c.py"));
        assert!(!pending.contains("b.py"));
    }

    #[test]
    fn clearing_everything_removes_the_log_file() {
        let (_tmp, dir) = tmp_infigraph_dir();
        mark_dirty(&dir, &["a.py".to_string()]).unwrap();
        clear_dirty(&dir, &["a.py".to_string()]).unwrap();
        assert!(
            !dirty_log_path(&dir).exists(),
            "an empty dirty set should not leave a stray file behind"
        );
        assert!(pending_dirty(&dir).unwrap().is_empty());
    }

    #[test]
    fn clear_is_idempotent_for_a_path_that_was_never_dirty() {
        let (_tmp, dir) = tmp_infigraph_dir();
        mark_dirty(&dir, &["a.py".to_string()]).unwrap();
        // Clearing a path that was never marked must not error or disturb
        // the paths that are actually pending.
        clear_dirty(&dir, &["never-marked.py".to_string()]).unwrap();
        let pending = pending_dirty(&dir).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending.contains("a.py"));
    }

    #[test]
    fn clear_on_an_empty_set_is_a_safe_no_op() {
        let (_tmp, dir) = tmp_infigraph_dir();
        clear_dirty(&dir, &["a.py".to_string()]).unwrap();
        assert!(pending_dirty(&dir).unwrap().is_empty());
    }

    #[test]
    fn crash_between_mark_and_clear_leaves_the_path_recoverable() {
        // Simulates the exact scenario R3.3.5 exists to fix: a raw fsevent
        // is marked dirty, then the process "crashes" (nothing further
        // happens -- no clear_dirty call) before the debounce-window flush
        // would have run. A fresh process reading pending_dirty must still
        // see the path.
        let (_tmp, dir) = tmp_infigraph_dir();
        mark_dirty(&dir, &["src/crashed_mid_batch.py".to_string()]).unwrap();
        // No clear_dirty call -- simulated crash.
        let recovered = pending_dirty(&dir).unwrap();
        assert!(recovered.contains("src/crashed_mid_batch.py"));
    }

    #[test]
    fn repeated_mark_and_clear_cycles_compact_rather_than_grow_unboundedly() {
        let (_tmp, dir) = tmp_infigraph_dir();
        for _ in 0..20 {
            mark_dirty(&dir, &["hot.py".to_string()]).unwrap();
            clear_dirty(&dir, &["hot.py".to_string()]).unwrap();
        }
        assert!(
            !dirty_log_path(&dir).exists(),
            "20 mark/clear cycles for the same path must not leave a growing log behind"
        );
    }
}
