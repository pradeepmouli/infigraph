//! Crash / corrupt-index recovery helpers (code graph + document store).

use std::path::Path;

/// Wipe code graph and document index artifacts under `root/.infigraph/`.
/// Used by SIGSEGV auto-reindex so both stores are rebuilt by `infigraph index`.
pub fn wipe_code_and_docs(root: &Path) {
    let ig = root.join(".infigraph");
    if !ig.exists() {
        return;
    }

    let graph_path = ig.join("graph");
    if graph_path.exists() {
        let _ = infigraph_core::quarantine::quarantine_graph(&ig, "graph");
    }
    let _ = std::fs::remove_file(&graph_path);
    let _ = std::fs::remove_dir_all(&graph_path);
    let _ = std::fs::remove_file(ig.join("graph.wal"));

    if let Ok(mut idx) = infigraph_docs::DocIndex::open(root) {
        let _ = idx.clean();
    }
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

        wipe_code_and_docs(dir.path());

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
        wipe_code_and_docs(dir.path()); // must not panic
    }
}
