//! Pre-write snapshots and restore, per docs/DESIGN-hardening.md R3.2.
//!
//! Distinct from `quarantine`'s two pools (`graph.corrupt.<ts>` for
//! corruption evidence, `graph.previous.<ts>` for the daemon's
//! build-then-swap full reindex): this module covers the *local, in-place*
//! full-reindex path (`infigraph index --full`), which today wipes the
//! entire `.infigraph/` tree via `ops::wipe_infigraph_preserving_index_lock`
//! with no backup at all. `create_snapshot` fills that gap; `restore` then
//! unifies all three pools behind one CLI-facing interface (R3.2.2), since
//! the quarantine/previous pools had no restore path either before this.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// How many pre-write snapshots to retain (R7.3: nothing in `.infigraph/`
/// may grow without a cap). Oldest evicted first.
const SNAPSHOT_RETENTION: usize = 2;

const SNAPSHOTS_DIR: &str = "snapshots";

/// Base name of the live graph file within `.infigraph/` (matches
/// `watch::mod::LIVE_NAME`; duplicated rather than shared because that
/// constant is private to the daemon's swap logic).
const LIVE_GRAPH_NAME: &str = "graph";

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Entries never copied into -- or removed by -- a snapshot: the lock a
/// caller holds across the wipe this precedes, the snapshots pool itself
/// (never nest a snapshot inside its own future snapshot), and the
/// quarantine/retired-previous pools (already-bounded backup data of their
/// own -- folding them into every new snapshot would compound pool growth
/// without adding recovery value, since they're independently restorable).
pub(crate) fn should_skip(name: &str) -> bool {
    name == "index.lock"
        || name == SNAPSHOTS_DIR
        || name.starts_with("graph.corrupt.")
        || name.starts_with("graph.previous.")
}

/// Snapshot the current `.infigraph/` state into `.infigraph/snapshots/<ts>/`
/// before a destructive full-reindex wipe (R3.2.1). Copies everything except
/// what `should_skip` excludes -- a real, independent copy per file (see
/// `copy_entry_recursive` for why a plain hard link isn't safe here).
///
/// Callers are responsible for holding whatever lock serializes writes to
/// `infigraph_dir` before calling this -- mirrors `quarantine`'s contract;
/// snapshotting itself does not lock.
pub fn create_snapshot(infigraph_dir: &Path) -> Result<PathBuf> {
    evict_oldest_if_at_bound(infigraph_dir)?;

    // now_epoch_secs() is second-granularity, so two calls within the same
    // wall-clock second (e.g. restore_from_snapshot's self-snapshot landing
    // in the same second as the snapshot it's about to read from) would
    // otherwise collide on this directory name -- and since the copy loop
    // below writes into an existing dir rather than failing, a collision
    // would silently overwrite the earlier snapshot's contents instead of
    // creating a distinct entry. Walk forward to a genuinely free name.
    let snapshots_root = infigraph_dir.join(SNAPSHOTS_DIR);
    let mut ts = now_epoch_secs();
    while snapshots_root.join(ts.to_string()).exists() {
        ts += 1;
    }
    let dest = snapshots_root.join(ts.to_string());
    std::fs::create_dir_all(&dest)
        .with_context(|| format!("snapshot: create {}", dest.display()))?;

    if infigraph_dir.exists() {
        for entry in std::fs::read_dir(infigraph_dir)
            .with_context(|| format!("snapshot: read_dir {}", infigraph_dir.display()))?
            .flatten()
        {
            let name = entry.file_name();
            if should_skip(&name.to_string_lossy()) {
                continue;
            }
            copy_entry_recursive(&entry.path(), &dest.join(&name))?;
        }
    }

    Ok(dest)
}

fn copy_entry_recursive(src: &Path, dest: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(src)
        .with_context(|| format!("snapshot: stat {}", src.display()))?;
    if meta.is_dir() {
        std::fs::create_dir_all(dest)
            .with_context(|| format!("snapshot: mkdir {}", dest.display()))?;
        for entry in std::fs::read_dir(src)
            .with_context(|| format!("snapshot: read_dir {}", src.display()))?
            .flatten()
        {
            copy_entry_recursive(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else {
        // A real, independent copy -- not a POSIX hard link. A hard link is
        // just a second name for the same inode: it provides no isolation,
        // so a later in-place mutation of `src` (e.g. Kuzu checkpointing
        // pages into its base file) would silently mutate the "snapshot"
        // too, defeating the entire point (caught by
        // `restore_from_snapshot_replaces_live_state_and_self_snapshots_first`
        // before this shipped). A true copy-on-write clone (APFS
        // `clonefile(2)`, Linux `reflink`) would be genuinely free AND safe,
        // but that needs platform-specific FFI this pass doesn't add; see
        // the tracking issue for that optimization.
        std::fs::copy(src, dest)
            .with_context(|| format!("snapshot: copy {} to {}", src.display(), dest.display()))?;
    }
    Ok(())
}

fn evict_oldest_if_at_bound(infigraph_dir: &Path) -> Result<()> {
    let snapshots_root = infigraph_dir.join(SNAPSHOTS_DIR);
    let mut existing: Vec<(u64, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&snapshots_root) {
        for e in entries.flatten() {
            if let Ok(ts) = e.file_name().to_string_lossy().parse::<u64>() {
                existing.push((ts, e.path()));
            }
        }
    }
    if existing.len() < SNAPSHOT_RETENTION {
        return Ok(());
    }
    existing.sort_by_key(|(ts, _)| *ts);
    let to_evict = existing.len() - (SNAPSHOT_RETENTION - 1);
    for (_, path) in existing.into_iter().take(to_evict) {
        let _ = std::fs::remove_dir_all(&path);
    }
    Ok(())
}

/// Which pool a restore point came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePointKind {
    /// `.infigraph/snapshots/<ts>/` -- whole-tree, pre-full-reindex (R3.2.1).
    Snapshot,
    /// `.infigraph/graph.previous.<ts>` -- healthy graph superseded by a
    /// daemon-routed full reindex or an earlier restore (`quarantine`).
    Previous,
    /// `.infigraph/graph.corrupt.<ts>` -- corruption evidence (`quarantine`).
    Corrupt,
}

impl RestorePointKind {
    fn label(self) -> &'static str {
        match self {
            RestorePointKind::Snapshot => "snapshot",
            RestorePointKind::Previous => "previous",
            RestorePointKind::Corrupt => "corrupt",
        }
    }
}

/// One entry a user can restore from.
#[derive(Debug, Clone)]
pub struct RestorePoint {
    pub kind: RestorePointKind,
    pub timestamp: u64,
    pub path: PathBuf,
}

impl RestorePoint {
    /// Stable identifier for CLI selection, e.g. `"snapshot:1787184237"`.
    pub fn id(&self) -> String {
        format!("{}:{}", self.kind.label(), self.timestamp)
    }
}

/// Enumerate every restorable point across all three pools, newest first:
/// whole-tree snapshots, plus the quarantine and retired-previous pools
/// (R3.2.2 -- those two had no CLI-visible restore path before this).
pub fn list_restore_points(infigraph_dir: &Path) -> Vec<RestorePoint> {
    let mut points = Vec::new();

    let snapshots_root = infigraph_dir.join(SNAPSHOTS_DIR);
    if let Ok(entries) = std::fs::read_dir(&snapshots_root) {
        for e in entries.flatten() {
            if let Ok(ts) = e.file_name().to_string_lossy().parse::<u64>() {
                points.push(RestorePoint {
                    kind: RestorePointKind::Snapshot,
                    timestamp: ts,
                    path: e.path(),
                });
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(infigraph_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            for (prefix, kind) in [
                ("graph.previous.", RestorePointKind::Previous),
                ("graph.corrupt.", RestorePointKind::Corrupt),
            ] {
                // Only an exact "<prefix><digits>" match, so WAL-family
                // siblings (e.g. "graph.corrupt.<ts>.wal") aren't listed as
                // restore points of their own -- they move with their base
                // entry inside restore_from_graph_pool.
                if let Some(rest) = name.strip_prefix(prefix) {
                    if let Ok(ts) = rest.parse::<u64>() {
                        points.push(RestorePoint {
                            kind,
                            timestamp: ts,
                            path: e.path(),
                        });
                    }
                }
            }
        }
    }

    points.sort_by_key(|p| std::cmp::Reverse(p.timestamp));
    points
}

/// Restore `.infigraph/` from a chosen point (R3.2.2). Never destructive to
/// what it replaces: the current live state is itself filed aside first, so
/// restoring the wrong point is always recoverable with another restore.
///
/// Callers are responsible for holding whatever lock serializes writes to
/// `infigraph_dir` before calling this (see `create_snapshot`).
pub fn restore(infigraph_dir: &Path, point: &RestorePoint) -> Result<()> {
    match point.kind {
        RestorePointKind::Snapshot => restore_from_snapshot(infigraph_dir, point.timestamp),
        RestorePointKind::Previous | RestorePointKind::Corrupt => {
            restore_from_graph_pool(infigraph_dir, point.kind, point.timestamp)
        }
    }
}

fn restore_from_snapshot(infigraph_dir: &Path, timestamp: u64) -> Result<()> {
    let src = infigraph_dir
        .join(SNAPSHOTS_DIR)
        .join(timestamp.to_string());
    if !src.is_dir() {
        anyhow::bail!("snapshot {timestamp} not found at {}", src.display());
    }

    // Safety net: snapshot the current live state before overwriting it, so
    // restoring the wrong point is itself just another restore away.
    create_snapshot(infigraph_dir)?;

    if infigraph_dir.exists() {
        for entry in std::fs::read_dir(infigraph_dir)
            .with_context(|| format!("restore: read_dir {}", infigraph_dir.display()))?
            .flatten()
        {
            let name = entry.file_name();
            if should_skip(&name.to_string_lossy()) {
                continue;
            }
            let path = entry.path();
            let result = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            result.with_context(|| format!("restore: remove {}", path.display()))?;
        }
    }

    for entry in std::fs::read_dir(&src)
        .with_context(|| format!("restore: read_dir {}", src.display()))?
        .flatten()
    {
        copy_entry_recursive(&entry.path(), &infigraph_dir.join(entry.file_name()))?;
    }

    Ok(())
}

fn restore_from_graph_pool(
    infigraph_dir: &Path,
    kind: RestorePointKind,
    timestamp: u64,
) -> Result<()> {
    let infix = match kind {
        RestorePointKind::Previous => "previous",
        RestorePointKind::Corrupt => "corrupt",
        RestorePointKind::Snapshot => unreachable!("handled by restore_from_snapshot"),
    };
    let src_stem = format!("{LIVE_GRAPH_NAME}.{infix}.{timestamp}");
    let src = infigraph_dir.join(&src_stem);
    if !src.exists() {
        anyhow::bail!(
            "{infix} restore point {timestamp} not found at {}",
            src.display()
        );
    }

    // Relocate the chosen restore point (and its WAL-family siblings) out of
    // its pool namespace *before* retiring the current live graph below.
    // `retire_previous_graph` evicts its pool down to its retention bound as
    // part of adding a new entry -- when the point being restored lives in
    // that same "previous" pool (kind == Previous), leaving it in place
    // would let that eviction delete the very entry this function is
    // restoring from (caught by
    // `restore_from_previous_pool_retires_current_live_graph_first` before
    // this shipped).
    const STAGING_STEM: &str = "graph.restoring";
    let staging = infigraph_dir.join(STAGING_STEM);
    std::fs::rename(&src, &staging)
        .with_context(|| format!("restore: stage {} to {}", src.display(), staging.display()))?;
    for path in crate::graph::wal_family_paths(&src) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let suffix = name.strip_prefix(&src_stem).unwrap_or(&name).to_owned();
        let dest = infigraph_dir.join(format!("{STAGING_STEM}{suffix}"));
        let _ = crate::quarantine::move_wal_sibling(&path, &dest);
    }

    let live_path = infigraph_dir.join(LIVE_GRAPH_NAME);
    if live_path.exists() {
        // File the current live graph aside as a "previous" entry -- a
        // healthy graph being superseded by a restore, not corruption
        // evidence -- before it's overwritten.
        crate::quarantine::retire_previous_graph(infigraph_dir, LIVE_GRAPH_NAME)?;
    }

    std::fs::rename(&staging, &live_path).with_context(|| {
        format!(
            "restore: rename {} to {}",
            staging.display(),
            live_path.display()
        )
    })?;

    for path in crate::graph::wal_family_paths(&staging) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let suffix = name.strip_prefix(STAGING_STEM).unwrap_or(&name).to_owned();
        let dest = infigraph_dir.join(format!("{LIVE_GRAPH_NAME}{suffix}"));
        let _ = crate::quarantine::move_wal_sibling(&path, &dest);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn snapshot_copies_live_state_and_skips_pools() {
        let tmp = tempfile::tempdir().unwrap();
        let tg = tmp.path().join(".infigraph");
        write(&tg.join("graph"), "graph-data");
        write(&tg.join("embeddings.bin"), "embed-data");
        write(&tg.join("sessions").join("s1.json"), "session-data");
        write(&tg.join("index.lock"), "");
        write(&tg.join("graph.corrupt.111"), "old-corrupt");

        let dest = create_snapshot(&tg).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("graph")).unwrap(),
            "graph-data"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("embeddings.bin")).unwrap(),
            "embed-data"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("sessions").join("s1.json")).unwrap(),
            "session-data"
        );
        assert!(!dest.join("index.lock").exists());
        assert!(!dest.join("graph.corrupt.111").exists());
    }

    #[test]
    fn snapshot_retention_evicts_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let tg = tmp.path().join(".infigraph");
        write(&tg.join("graph"), "v1");

        for ts in [100u64, 200, 300] {
            std::fs::create_dir_all(tg.join(SNAPSHOTS_DIR).join(ts.to_string())).unwrap();
        }
        // Pre-seed 3 fake snapshots (above SNAPSHOT_RETENTION=2) directly,
        // then take one real one -- eviction must bring the pool back to
        // exactly SNAPSHOT_RETENTION entries, keeping the newest.
        create_snapshot(&tg).unwrap();

        let remaining: std::collections::BTreeSet<String> =
            std::fs::read_dir(tg.join(SNAPSHOTS_DIR))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
        assert_eq!(remaining.len(), SNAPSHOT_RETENTION);
        assert!(remaining.contains("300"));
        assert!(!remaining.contains("100"));
    }

    #[test]
    fn list_restore_points_covers_all_three_pools() {
        let tmp = tempfile::tempdir().unwrap();
        let tg = tmp.path().join(".infigraph");
        std::fs::create_dir_all(tg.join(SNAPSHOTS_DIR).join("500")).unwrap();
        write(&tg.join("graph.previous.400"), "prev");
        write(&tg.join("graph.corrupt.300"), "corrupt");
        write(&tg.join("graph.corrupt.300.wal"), "wal-sibling");

        let points = list_restore_points(&tg);
        let ids: Vec<String> = points.iter().map(|p| p.id()).collect();

        assert_eq!(ids, vec!["snapshot:500", "previous:400", "corrupt:300"]);
    }

    #[test]
    fn restore_from_snapshot_replaces_live_state_and_self_snapshots_first() {
        let tmp = tempfile::tempdir().unwrap();
        let tg = tmp.path().join(".infigraph");
        write(&tg.join("graph"), "old-live");
        let snap = create_snapshot(&tg).unwrap();
        let ts: u64 = snap.file_name().unwrap().to_str().unwrap().parse().unwrap();

        write(&tg.join("graph"), "newer-live-to-be-discarded");

        let point = RestorePoint {
            kind: RestorePointKind::Snapshot,
            timestamp: ts,
            path: snap,
        };
        restore(&tg, &point).unwrap();

        assert_eq!(
            std::fs::read_to_string(tg.join("graph")).unwrap(),
            "old-live"
        );
        // The pre-restore state was itself snapshotted before being wiped.
        let snapshots_after: Vec<_> = std::fs::read_dir(tg.join(SNAPSHOTS_DIR))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(snapshots_after.len(), 2);
    }

    #[test]
    fn restore_from_previous_pool_retires_current_live_graph_first() {
        let tmp = tempfile::tempdir().unwrap();
        let tg = tmp.path().join(".infigraph");
        write(&tg.join("graph"), "current-live");
        write(&tg.join("graph.previous.999"), "older-graph");

        let point = RestorePoint {
            kind: RestorePointKind::Previous,
            timestamp: 999,
            path: tg.join("graph.previous.999"),
        };
        restore(&tg, &point).unwrap();

        assert_eq!(
            std::fs::read_to_string(tg.join("graph")).unwrap(),
            "older-graph"
        );
        // The graph that was live before the restore is now in the
        // "previous" pool under a fresh timestamp, not lost.
        let previous_entries: Vec<_> = std::fs::read_dir(&tg)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("graph.previous.")
            })
            .collect();
        assert_eq!(previous_entries.len(), 1);
        assert_eq!(
            std::fs::read_to_string(previous_entries[0].path()).unwrap(),
            "current-live"
        );
    }
}
