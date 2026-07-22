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
    let quarantine_stem = format!("{graph_name}.corrupt.{ts}");
    let quarantine_path = infigraph_dir.join(&quarantine_stem);
    let source = infigraph_dir.join(graph_name);

    std::fs::rename(&source, &quarantine_path).with_context(|| {
        format!(
            "quarantine: rename {} to {}",
            source.display(),
            quarantine_path.display()
        )
    })?;

    // Move WAL-family siblings alongside the quarantined graph, as flat
    // sibling files sharing its "<name>.corrupt.<ts>" stem (e.g.
    // "graph.corrupt.<ts>.wal"), so a future investigation has the full
    // picture, not just the base image. `quarantine_path` is typically a
    // plain FILE here (Kuzu's on-disk graph is a single file, not a
    // directory -- see `wipe_graph`), so siblings can't be nested "inside"
    // it; they must live beside it under `infigraph_dir`.
    //
    // Each move prefers an atomic rename but falls back to copy+remove if
    // rename fails for any reason (e.g. certain filesystem edge cases).
    // This robustness is what actually matters here: both callers of this
    // function (`wipe_graph`, `wipe_code_and_docs_with_timeout`) run
    // unconditional cleanup of the ORIGINAL path immediately afterward as a
    // fallback for whatever quarantine didn't handle. If a sibling rename
    // silently failed and left the file at its original path, that cleanup
    // would delete it moments later -- turning a partial quarantine failure
    // into active data destruction. Falling back to copy+remove means the
    // content is safely duplicated into quarantine before we ever try to
    // remove the original, so even a failure removing the original
    // afterward is harmless (the caller's fallback cleanup just finishes
    // that removal; quarantine already holds the evidence).
    let wal = infigraph_dir.join(format!("{graph_name}.wal"));
    if wal.exists() {
        let dest = infigraph_dir.join(format!("{quarantine_stem}.wal"));
        if let Err(e) = move_wal_sibling(&wal, &dest) {
            eprintln!(
                "[quarantine] warning: could not relocate WAL sibling {} into quarantine ({e:#}) \
                 -- it may be destroyed by fallback cleanup at its original path",
                wal.display()
            );
        }
    }
    if let Ok(entries) = std::fs::read_dir(infigraph_dir) {
        let prefix = format!("{graph_name}.wal.");
        let siblings: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(&prefix))
                    .unwrap_or(false)
            })
            .collect();
        for path in siblings {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            // name looks like "<graph_name>.wal.checkpoint"; keep everything
            // after "<graph_name>" so the quarantined name stays recognizable
            // and collision-free (e.g. "graph.corrupt.<ts>.wal.checkpoint").
            let suffix = name.strip_prefix(graph_name).unwrap_or(name.as_str());
            let dest = infigraph_dir.join(format!("{quarantine_stem}{suffix}"));
            if let Err(e) = move_wal_sibling(&path, &dest) {
                eprintln!(
                    "[quarantine] warning: could not relocate WAL sibling {} into quarantine \
                     ({e:#}) -- it may be destroyed by fallback cleanup at its original path",
                    path.display()
                );
            }
        }
    }

    Ok(quarantine_path)
}

/// Move a WAL-family sibling into quarantine: try an atomic rename first
/// (fast, same-filesystem, the common case), and if that fails, fall back to
/// copy+remove so a rename failure that copy wouldn't hit (e.g. certain
/// filesystem edge cases) doesn't silently lose the file. Once the copy
/// succeeds the content is safe in quarantine, so a failure removing the
/// now-redundant original afterward is not treated as an error -- the
/// caller's existing fallback cleanup will finish removing that leftover.
/// Only returns `Err` if the content could not be relocated at all.
fn move_wal_sibling(src: &Path, dest: &Path) -> Result<()> {
    if std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dest).with_context(|| {
        format!(
            "quarantine: copy {} to {} (rename fallback)",
            src.display(),
            dest.display()
        )
    })?;
    let _ = std::fs::remove_file(src);
    Ok(())
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
    for (ts, path) in existing.into_iter().take(to_evict) {
        // The quarantine target is typically a plain FILE (Kuzu's on-disk
        // graph is a single file, not a directory -- see `wipe_graph`), so
        // `remove_dir_all` alone silently no-ops here (it errors on a
        // non-directory path and that error was being swallowed): dispatch
        // on the actual entry type instead of assuming a directory.
        let is_dir = std::fs::metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if is_dir {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
        // Also evict any WAL-family siblings quarantined alongside this
        // entry (e.g. "<name>.corrupt.<ts>.wal", "...wal.checkpoint") so
        // the pool doesn't leak unbounded copies of those either.
        let sibling_prefix = format!("{graph_name}.corrupt.{ts}.");
        if let Ok(entries) = std::fs::read_dir(infigraph_dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with(&sibling_prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    Ok(())
}
