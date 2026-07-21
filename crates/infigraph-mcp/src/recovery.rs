//! Crash / corrupt-index recovery helpers (code graph + document store).

use std::path::Path;
use std::time::Duration;

use anyhow::Context;

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
        let _ = std::fs::remove_file(&graph_path);
        let _ = std::fs::remove_dir_all(&graph_path);
    }
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
