use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// How many quarantined copies of a given graph name to retain. When a new
/// quarantine would exceed this, the oldest (by embedded timestamp, not
/// filesystem mtime) is deleted first.
const QUARANTINE_RETENTION: usize = 2;

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Rename a corrupt graph directory (and its WAL-family siblings) aside into
/// a bounded quarantine pool instead of deleting it, per
/// docs/DESIGN-hardening.md R3.1.2. `infigraph_dir` is the `.infigraph/`
/// directory; `graph_name` is the base name of the graph within it (e.g.
/// `"graph"`). Returns the path the graph was moved to.
///
/// Callers are responsible for holding whatever write lock guards
/// `graph_name` before calling this — quarantine itself does not lock,
/// mirroring `wipe_graph`'s existing contract where the caller already
/// acquired `graph.lock` before deciding to wipe.
pub fn quarantine_graph(infigraph_dir: &Path, graph_name: &str) -> Result<PathBuf> {
    evict_oldest_if_at_bound(infigraph_dir, graph_name)?;

    let ts = now_epoch_secs();
    let quarantine_path = infigraph_dir.join(format!("{graph_name}.corrupt.{ts}"));
    let source = infigraph_dir.join(graph_name);

    std::fs::rename(&source, &quarantine_path).with_context(|| {
        format!(
            "quarantine: rename {} to {}",
            source.display(),
            quarantine_path.display()
        )
    })?;

    // Move WAL-family siblings alongside the quarantined graph so a future
    // investigation has the full picture, not just the base image.
    let wal = infigraph_dir.join(format!("{graph_name}.wal"));
    if wal.exists() {
        let _ = std::fs::rename(&wal, quarantine_path.join(format!("{graph_name}.wal")));
    }
    if let Ok(entries) = std::fs::read_dir(infigraph_dir) {
        let prefix = format!("{graph_name}.wal.");
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) {
                let _ = std::fs::rename(e.path(), quarantine_path.join(&name));
            }
        }
    }

    Ok(quarantine_path)
}

fn evict_oldest_if_at_bound(infigraph_dir: &Path, graph_name: &str) -> Result<()> {
    let prefix = format!("{graph_name}.corrupt.");
    let mut existing: Vec<(u64, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(infigraph_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(ts_str) = name.strip_prefix(&prefix) {
                if let Ok(ts) = ts_str.parse::<u64>() {
                    existing.push((ts, e.path()));
                }
            }
        }
    }
    if existing.len() < QUARANTINE_RETENTION {
        return Ok(());
    }
    existing.sort_by_key(|(ts, _)| *ts);
    // existing.len() >= QUARANTINE_RETENTION and we're about to add one more,
    // so evict enough of the oldest entries to land at QUARANTINE_RETENTION - 1
    // before the new one is created (bringing the total back to QUARANTINE_RETENTION).
    let to_evict = existing.len() - (QUARANTINE_RETENTION - 1);
    for (_, path) in existing.into_iter().take(to_evict) {
        let _ = std::fs::remove_dir_all(&path);
    }
    Ok(())
}
