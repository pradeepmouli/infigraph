//! Crash / corrupt-index recovery helpers (code graph + document store).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;

/// Collect the set of project roots to reindex after a crash.
///
/// The MCP server may be launched in a repo that was never registered in
/// `~/.infigraph/registry.json` (standalone use is the common case), so the
/// registry alone is not enough: with an empty registry the old recovery was
/// a no-op and the crashed repo stayed broken. The supervisor's startup
/// directory is therefore always considered a candidate.
///
/// Only paths that actually contain a `.infigraph/` directory are returned,
/// deduplicated by canonical path so a registered startup dir isn't indexed
/// twice.
pub fn collect_reindex_targets(
    startup_dir: Option<&Path>,
    registry_paths: &[PathBuf],
    groups_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut targets = Vec::new();

    let mut push = |path: &Path| {
        // Must be a directory — a stray regular file named `.infigraph`
        // is not an index and must not trigger a reindex.
        if !path.join(".infigraph").is_dir() {
            return;
        }
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if seen.insert(key) {
            targets.push(path.to_path_buf());
        }
    };

    // The repo the MCP server was actually serving comes first.
    if let Some(dir) = startup_dir {
        push(dir);
    }
    for path in registry_paths {
        push(path);
    }
    if let Some(gd) = groups_dir {
        if let Ok(entries) = std::fs::read_dir(gd) {
            for entry in entries.flatten() {
                push(&entry.path());
            }
        }
    }

    targets
}

/// Proactively starts watching the directory this MCP server was launched
/// in, rather than only ever starting a watcher reactively after some write
/// happens to touch it. Only takes effect when both daemon mode
/// (`INFIGRAPH_BACKEND=daemon`) and the `[watch].auto_start_on_boot`
/// config toggle (env override: `INFIGRAPH_AUTO_START_WATCH`) are on. Runs
/// a true-up reindex first (see the "True-up" comment inline below) so the
/// watcher starts from a caught-up baseline instead of only ever reacting
/// to changes from this point forward.
///
/// Scoped to just `startup_dir` -- an earlier version swept the whole
/// project registry plus the groups dir (reusing `collect_reindex_targets`,
/// which crash recovery still uses for its own, legitimately broader,
/// purpose), but that meant every MCP server startup spun up a daemon for
/// every registered project regardless of whether this server instance was
/// actually serving it: stale temp-test directories, a groups config dir
/// that isn't really a "project", and other repos not even open in this
/// session. Only the repo this server was launched in is this server's
/// concern.
///
/// Deliberately synchronous (unlike `main.rs`'s SIGSEGV recovery path,
/// which fires deep into a crash-handling loop): `main.rs::run()` wraps this
/// call in its own thread so this doesn't delay MCP server readiness, but
/// keeping the logic itself synchronous means tests can observe its effect
/// deterministically. Lives in the library crate rather than as a
/// `main.rs`-only function, specifically so integration tests in `tests/`
/// can reach it -- `main.rs` compiles to a separate `[[bin]]` target with no
/// unit-test history of its own.
pub fn start_daemon_watcher_for_startup_dir(startup_dir: Option<&Path>) {
    if !infigraph_core::daemon_backend_selected()
        || !crate::session_context::auto_start_watch_on_boot_enabled()
        || !infigraph_core::watch::config::watch_enabled("watch")
    {
        return;
    }

    let Some(dir) = startup_dir else {
        return;
    };
    // Must be a directory that's actually been indexed -- mirrors
    // `collect_reindex_targets`'s own guard, for the same reason: a fresh,
    // never-indexed cwd has no `.infigraph/watch.lock` home for a daemon to
    // coordinate through, and there's nothing to watch yet regardless.
    if !dir.join(".infigraph").is_dir() {
        return;
    }

    let path_str = dir.to_string_lossy().to_string();

    // True-up: catch drift accumulated while nothing was watching this
    // project (MCP was down, or a prior watcher exceeded its restart
    // budget and gave up) before starting to watch -- the watcher itself
    // only reacts to *future* filesystem events, so without this a file
    // added or changed during that gap stays invisible until something
    // else happens to touch it again. Reuses the same tool a client would
    // call to reindex by hand: incremental (mtime/hash based) under the
    // hood, so this is near-free when nothing drifted and does real work
    // only when it did. Errors are logged, not propagated -- a failed
    // true-up must not block starting the watcher below.
    match crate::tools::index::tool_index_project(&serde_json::json!({ "path": path_str })) {
        Ok(msg) => crate::mcp_log("INFO", &format!("Startup true-up index: {path_str}: {msg}")),
        Err(e) => crate::mcp_log(
            "WARN",
            &format!("Startup true-up index failed for {path_str}: {e}"),
        ),
    }

    if let Some(msg) = crate::tools::watch::auto_start_watch(&path_str) {
        crate::mcp_log("INFO", &format!("Startup watch: {path_str}: {msg}"));
    }
    crate::tools::docs::auto_start_doc_watch(&path_str);
}

/// Wipe code graph and document index artifacts under `root/.infigraph/`.
/// Used by SIGSEGV auto-reindex so both stores are rebuilt by `infigraph index`.
pub fn wipe_code_and_docs(root: &Path) -> anyhow::Result<()> {
    wipe_code_and_docs_with_timeout(root, Duration::from_secs(5))
}

fn wipe_code_and_docs_with_timeout(root: &Path, timeout: Duration) -> anyhow::Result<()> {
    let ig = root.join(".infigraph");
    if !ig.exists() {
        return Ok(());
    }

    // A wipe must never race a live writer: take the same per-graph lock
    // writers hold. Busy here means a live process -- refuse, don't destroy.
    let _lock = infigraph_core::lockfile::acquire(&ig.join("graph.lock"), "graph-wipe", timeout)
        .with_context(|| "refusing to wipe: graph write lock is held by a live process")?;

    let graph_path = ig.join("graph");
    if graph_path.exists() {
        let _ = infigraph_core::quarantine::quarantine_graph(&ig, "graph");
    }
    let _ = std::fs::remove_file(&graph_path);
    let _ = std::fs::remove_dir_all(&graph_path);
    let _ = std::fs::remove_file(ig.join("graph.wal"));
    // Also remove Kuzu's WAL-family temp siblings (e.g. graph.wal.checkpoint):
    // one left behind carries the old database's ID and permanently blocks
    // opening a freshly rebuilt graph.
    if let Ok(entries) = std::fs::read_dir(&ig) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("graph.wal.") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    if let Ok(mut idx) = infigraph_docs::DocIndex::open(root) {
        let _ = idx.clean();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_wipe_code_and_docs_removes_graph_and_docs() {
        let dir = tempfile::tempdir().unwrap();
        let ig = dir.path().join(".infigraph");
        fs::create_dir_all(&ig).unwrap();
        fs::write(ig.join("graph"), b"fake-graph").unwrap();
        fs::write(ig.join("graph.wal"), b"wal").unwrap();
        fs::write(ig.join("docs.kuzu"), b"fake-docs").unwrap();
        fs::write(ig.join("docs_embeddings.bin"), b"emb").unwrap();
        fs::write(ig.join("docs_hnsw_index.usearch"), b"hnsw").unwrap();
        fs::write(ig.join("docs_hnsw_index.meta"), b"meta").unwrap();
        // sessions must survive
        fs::write(ig.join("sessions_keep.txt"), b"keep").unwrap();

        wipe_code_and_docs(dir.path()).unwrap();

        assert!(!ig.join("graph").exists());
        assert!(!ig.join("graph.wal").exists());
        assert!(!ig.join("docs.kuzu").exists());
        assert!(!ig.join("docs_embeddings.bin").exists());
        assert!(!ig.join("docs_hnsw_index.usearch").exists());
        assert!(!ig.join("docs_hnsw_index.meta").exists());
        assert!(
            ig.join("sessions_keep.txt").exists(),
            "non-index files under .infigraph must not be wiped"
        );
    }

    /// Regression test: recovery used to iterate only registry repos, so with
    /// an empty registry (the standalone default) it recovered nothing — the
    /// crashed repo the MCP server was actually serving stayed broken.
    #[test]
    fn collect_targets_includes_startup_dir_with_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".infigraph")).unwrap();

        let targets = collect_reindex_targets(Some(dir.path()), &[], None);
        assert_eq!(targets, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn collect_targets_skips_dirs_without_infigraph() {
        let dir = tempfile::tempdir().unwrap(); // no .infigraph inside
        let targets = collect_reindex_targets(Some(dir.path()), &[], None);
        assert!(targets.is_empty());
    }

    #[test]
    fn collect_targets_dedups_startup_dir_against_registry() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".infigraph")).unwrap();

        // Syntactic alias of the same directory — dedup must go through
        // canonicalization, not string equality.
        let registry = vec![dir.path().join(".")];
        let targets = collect_reindex_targets(Some(dir.path()), &registry, None);
        assert_eq!(
            targets.len(),
            1,
            "same repo via startup dir and registry must be indexed once"
        );
    }

    #[test]
    fn collect_targets_skips_infigraph_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        // A stray regular file named `.infigraph` is not an index.
        fs::write(dir.path().join(".infigraph"), b"not a dir").unwrap();

        let targets = collect_reindex_targets(Some(dir.path()), &[], None);
        assert!(
            targets.is_empty(),
            "regular file named .infigraph must not trigger recovery"
        );
    }

    #[test]
    fn collect_targets_includes_registry_repos_and_groups() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".infigraph")).unwrap();

        let groups = tempfile::tempdir().unwrap();
        let group = groups.path().join("my-group");
        fs::create_dir_all(group.join(".infigraph")).unwrap();

        let registry = vec![repo.path().to_path_buf()];
        let targets = collect_reindex_targets(None, &registry, Some(groups.path()));
        assert_eq!(targets.len(), 2);
        assert!(targets.contains(&repo.path().to_path_buf()));
        assert!(targets.contains(&group));
    }

    #[test]
    fn test_wipe_code_and_docs_missing_infigraph_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        wipe_code_and_docs(dir.path()).unwrap(); // must not panic
    }

    #[test]
    fn test_wipe_refuses_while_graph_lock_held() {
        let dir = tempfile::tempdir().unwrap();
        let ig = dir.path().join(".infigraph");
        fs::create_dir_all(&ig).unwrap();
        fs::write(ig.join("graph"), b"live-graph").unwrap();
        let _held = infigraph_core::lockfile::try_acquire(&ig.join("graph.lock"), "test-writer")
            .unwrap()
            .expect("free");

        // Short timeout: the lock is held for the test's whole duration, so
        // there's no point waiting the real 5s budget to observe the refusal.
        let res = wipe_code_and_docs_with_timeout(dir.path(), Duration::from_millis(50));
        assert!(
            res.is_err(),
            "wipe must refuse while the write lock is held"
        );
        assert!(
            ig.join("graph").exists(),
            "graph must survive a refused wipe"
        );
    }
}
