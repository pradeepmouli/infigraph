use std::path::Path;

use anyhow::{Context, Result};

/// Paths, relative to `.infigraph/`, excluded from a clone: lock files (a copied
/// lock would falsely claim the destination is held by the source's possibly-live
/// process) and logs (source-specific, meaningless at the destination). The
/// restore pools are excluded too, by [`crate::snapshot::is_restore_pool_entry`]
/// (the one list `worktree clean` and a snapshot's own copy use): they are the
/// source graph's history, and a clone can always re-clone from the source.
const EXCLUDED_RELATIVE_PATHS: &[&str] = &[
    "graph.lock",
    "watch.lock",
    "mcp.lock",
    "index.lock",
    "docs-op.lock",
    "docs.kuzu.lock",
    "config.lock",
    "logs",
];

/// Copy `<src_root>/.infigraph/` to `<dst_root>/.infigraph/`, excluding lock files,
/// logs and the restore pools. Does not index -- the caller runs `infigraph index` afterward.
pub fn clone_infigraph_dir(src_root: &Path, dst_root: &Path) -> Result<()> {
    let src_dir = src_root.join(".infigraph");
    anyhow::ensure!(
        src_dir.is_dir(),
        "nothing to clone from: {} has no .infigraph directory",
        src_root.display()
    );

    let dst_dir = dst_root.join(".infigraph");
    std::fs::create_dir_all(&dst_dir).with_context(|| format!("create {}", dst_dir.display()))?;

    copy_dir_excluding(&src_dir, &dst_dir, &src_dir)
}

fn copy_dir_excluding(src: &Path, dst: &Path, base: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        // `rel` is the whole relative path, so only a top-level entry can be a
        // pool: `sessions/snapshots` is somebody's data.
        if EXCLUDED_RELATIVE_PATHS.iter().any(|ex| rel == *ex)
            || crate::snapshot::is_restore_pool_entry(&rel)
        {
            continue;
        }

        let dst_path = dst.join(entry.file_name());
        if path.is_dir() {
            std::fs::create_dir_all(&dst_path)
                .with_context(|| format!("create {}", dst_path.display()))?;
            copy_dir_excluding(&path, &dst_path, base)?;
        } else {
            std::fs::copy(&path, &dst_path)
                .with_context(|| format!("copy {} to {}", path.display(), dst_path.display()))?;
        }
    }
    Ok(())
}
