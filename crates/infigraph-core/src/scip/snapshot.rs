//! What the source tree looked like when a SCIP indexer started, so the import
//! can tell which documents it describes no longer.
//!
//! An indexer reads files at one moment and its output is imported minutes
//! later, while the watcher keeps the graph current. A document whose file
//! changed in between would put back symbols a tree-sitter reindex has since
//! removed. The snapshot is `(length, mtime)` per file, taken before the
//! indexer starts and stored beside its scratch output; the import skips any
//! document whose file no longer matches and the next enrichment covers it.
//! No snapshot (a hand-made `scip-import`, an output from an older build)
//! means every document is imported, as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};

use crate::graph::store_util::fwd_slash_path;
use crate::ignore_rules::{walk_builder, Domain};

/// File stamp: length and modification time in nanoseconds.
type Stamp = (u64, u128);

/// The snapshot that belongs to the scratch output `scip_path`
/// (`<indexer>.<run>.scip` -> `<indexer>.<run>.stat`).
pub fn snapshot_path(scip_path: &Path) -> PathBuf {
    scip_path.with_extension("stat")
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    let nanos = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((meta.len(), nanos))
}

/// Records every indexable source file of `root` next to `scip_path`. Call it
/// before the indexer starts reading.
pub fn write_snapshot(root: &Path, scip_path: &Path) -> Result<()> {
    let mut files: HashMap<String, Stamp> = HashMap::new();
    for entry in walk_builder(root, Domain::Code).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        if let Some(s) = stamp(entry.path()) {
            files.insert(fwd_slash_path(rel), s);
        }
    }
    let path = snapshot_path(scip_path);
    std::fs::write(&path, serde_json::to_vec(&files)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// The files of `documents` (relative paths) that changed since the snapshot
/// of `scip_path` was taken, or are not in it. Empty when there is no readable
/// snapshot: its absence is not evidence of change.
pub fn changed_since(root: &Path, scip_path: &Path, documents: &[&str]) -> Vec<String> {
    let Ok(bytes) = std::fs::read(snapshot_path(scip_path)) else {
        return Vec::new();
    };
    let Ok(then) = serde_json::from_slice::<HashMap<String, Stamp>>(&bytes) else {
        return Vec::new();
    };
    documents
        .iter()
        .filter(|rel| then.get(**rel).copied() != stamp(&root.join(rel)))
        .map(|rel| rel.to_string())
        .collect()
}

/// Removes a scratch output and its snapshot.
pub fn remove_scratch(scip_path: &Path) {
    let _ = std::fs::remove_file(scip_path);
    let _ = std::fs::remove_file(snapshot_path(scip_path));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_edited_after_the_snapshot_is_reported_and_an_untouched_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.ts"), "one").unwrap();
        std::fs::write(dir.path().join("b.ts"), "two").unwrap();
        let scip = dir.path().join("ts.1-1.scip");
        write_snapshot(dir.path(), &scip).unwrap();
        std::fs::write(dir.path().join("a.ts"), "three!").unwrap();
        std::fs::write(dir.path().join("c.ts"), "new").unwrap();

        let changed = changed_since(dir.path(), &scip, &["a.ts", "b.ts", "c.ts"]);
        assert_eq!(changed, vec!["a.ts", "c.ts"]);
    }

    #[test]
    fn no_snapshot_means_nothing_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let scip = dir.path().join("ts.1-1.scip");
        assert!(changed_since(dir.path(), &scip, &["a.ts"]).is_empty());
    }

    #[test]
    fn removing_a_scratch_output_takes_its_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let scip = dir.path().join("ts.1-1.scip");
        std::fs::write(&scip, "x").unwrap();
        write_snapshot(dir.path(), &scip).unwrap();
        remove_scratch(&scip);
        assert!(!scip.exists() && !snapshot_path(&scip).exists());
    }
}
