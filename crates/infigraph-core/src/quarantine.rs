use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// How many quarantined copies of a given graph name to retain. When a new
/// quarantine would exceed this, the oldest (by embedded timestamp, not
/// filesystem mtime) is deleted first.
const QUARANTINE_RETENTION: usize = 2;

/// Infix distinguishing the corruption-evidence pool (`graph.corrupt.<ts>`).
const CORRUPT_INFIX: &str = "corrupt";

/// Infix for the superseded-by-full-reindex pool (`graph.previous.<ts>`).
const PREVIOUS_INFIX: &str = "previous";

/// How many superseded-by-full-reindex copies to retain. Deliberately
/// tighter than `QUARANTINE_RETENTION`: this pool holds *healthy* graphs
/// filed aside by a routine operation, so one rollback candidate is the
/// useful amount and each extra copy is a full graph's worth of disk.
const PREVIOUS_RETENTION: usize = 1;

/// Byte cap on a single CORRUPT-pool base image (R7.3 / #100). The sittir
/// incident quarantined a 9.9G corrupt store (40x the healthy rebuild of
/// the same repo) and filled the disk -- and a corrupt base image's pages
/// are mostly worthless for forensics anyway; the WAL family plus a
/// manifest carry the evidence that matters. An oversized base is
/// therefore dropped (with an R6.3 audit line) in favor of its WAL
/// siblings and a small manifest recording what was dropped.
///
/// The `previous` pool is deliberately NOT size-capped: retention=1
/// already bounds it to one healthy rollback candidate of about the live
/// graph's size, and truncating a healthy graph destroys its entire
/// rollback value.
///
/// Resolved via the `graph` settings group
/// (`INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES`; 0 disables the cap).
fn quarantine_max_bytes(infigraph_dir: &Path) -> u64 {
    let scope = crate::settings_file::ConfigScope::of_infigraph_dir(Some(infigraph_dir));
    crate::graph::Graph::resolve(crate::graph::RawGraph::default(), scope).quarantine_max_bytes
}

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
/// Decide what a graph whose open is failing actually needs -- and, where that
/// is a torn WAL, fix it by setting the WAL aside.
///
/// See [`WalRecovery`] for the three answers. Two of them forbid a quarantine,
/// so callers must match on the verdict rather than test it for truthiness.
///
/// Why this exists: of the twelve quarantined images currently retained on
/// this machine, re-probed on 2026-09-07, **none was corrupt**. They split
/// 7 / 5:
///
/// - **7 torn WAL** -- failed with their WAL attached, opened cleanly without
///   it. An intact base image under a torn tail, where the right outcome is to
///   lose the uncommitted tail rather than the entire index.
/// - **5 already healthy** -- opened exactly as they stood. Discarded over
///   something transient that outlasted the caller's retry budget.
///
/// Eight repositories lost indexes over five weeks that did not need to be
/// lost. (An earlier count here said 5 / 7 the other way. It described a
/// thirteen-image population that also held the one genuinely damaged image
/// ever seen -- the SIGBUS one below -- since evicted by
/// `QUARANTINE_RETENTION`. Treat these numbers as a dated sample, not an
/// invariant: the pool turns over.)
///
/// The probe runs out of process ([`crate::probe`]) because the thirteenth
/// image exits with SIGBUS rather than returning an error. Probing inline
/// would turn a recoverable situation into a crash loop.
///
/// On failure the WAL is put back, so a caller that goes on to quarantine
/// still preserves the complete picture for diagnosis.
///
/// Same locking contract as [`quarantine_graph`]: the caller already holds
/// `graph.lock`.
pub fn try_recover_by_setting_wal_aside(infigraph_dir: &Path, graph_name: &str) -> WalRecovery {
    try_recover_by_setting_wal_aside_with(infigraph_dir, graph_name, crate::probe::graph_opens)
}

/// What [`try_recover_by_setting_wal_aside`] concluded about a graph.
///
/// This is deliberately not a `bool`. It used to be, and two of the three
/// outcomes collapsed onto `false` -- "the image is fine, leave it alone" and
/// "the image is beyond saving, go ahead and quarantine", which are opposite
/// instructions to the caller. `Infigraph::init` compensated by re-probing
/// after a `false`; `GraphStore::open_read_only_or_degrade` did not, and
/// quarantined healthy graphs. Splitting the verdict is what stops a third
/// caller from having to rediscover that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalRecovery {
    /// The image opens exactly as it stands, WAL included. Nothing was wrong
    /// with it and nothing was touched. The caller MUST NOT quarantine: its
    /// own open failed for a reason that had nothing to do with this graph.
    AlreadyHealthy,
    /// The base image opened once a torn WAL was set aside. The WAL family has
    /// been filed as `<graph>.torn-wal.<ts>` evidence and the graph is usable.
    /// The caller MUST NOT quarantine. Changes committed since the last
    /// checkpoint are lost; the index is not.
    Recovered,
    /// The image does not open with its WAL or without it. Any WAL that was
    /// moved has been put back, so a quarantine now preserves the complete
    /// picture for diagnosis.
    NotRecoverable,
}

impl WalRecovery {
    /// Whether the caller may go on to quarantine. Only one outcome permits
    /// it; prefer this over matching `!= Recovered`, which is the exact
    /// mistake the old `bool` invited.
    pub fn permits_quarantine(self) -> bool {
        matches!(self, Self::NotRecoverable)
    }

    /// Whether the graph is usable right now without further recovery work.
    pub fn is_usable(self) -> bool {
        matches!(self, Self::AlreadyHealthy | Self::Recovered)
    }
}

/// [`try_recover_by_setting_wal_aside`] with the probe injected, so tests can
/// exercise the file shuffling without spawning a process. A test binary must
/// never reach the real probe: `current_exe()` there is libtest's harness,
/// which re-runs the whole suite instead of probing.
pub(crate) fn try_recover_by_setting_wal_aside_with(
    infigraph_dir: &Path,
    graph_name: &str,
    probe: impl Fn(&Path) -> bool,
) -> WalRecovery {
    let source = infigraph_dir.join(graph_name);
    if !source.exists() {
        return WalRecovery::NotRecoverable;
    }

    // Ask whether the image is ALREADY FINE before anything else. Setting a
    // healthy WAL aside is not a recovery -- it silently discards every change
    // committed since the last checkpoint.
    //
    // This runs BEFORE the WAL check below, not after, because "is this graph
    // healthy?" has nothing to do with whether a WAL happens to exist. Asking
    // in the other order meant a healthy graph with no WAL fell out as "not
    // recoverable" without ever being examined, and each caller had to re-probe
    // for itself to avoid quarantining it -- which only one of them did.
    if probe(&source) {
        return WalRecovery::AlreadyHealthy;
    }

    let wal_paths = crate::graph::wal_family_paths(&source);
    if wal_paths.is_empty() {
        // Does not open, and has no WAL to blame for it.
        return WalRecovery::NotRecoverable;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for path in &wal_paths {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let suffix = name.strip_prefix(graph_name).unwrap_or(&name).to_owned();
        let dest = crate::probe::torn_wal_path(infigraph_dir, graph_name, &suffix, ts);
        if std::fs::rename(path, &dest).is_err() {
            // Could not stage the experiment cleanly -- undo and decline
            // rather than leave the WAL family half-moved.
            for (from, to) in moved.iter().rev() {
                let _ = std::fs::rename(to, from);
            }
            return WalRecovery::NotRecoverable;
        }
        moved.push((path.clone(), dest));
    }

    if probe(&source) {
        eprintln!(
            "[graph] recovered: the base image opens once its WAL is set aside -- keeping the \
             graph and filing the torn WAL as {} (uncommitted changes since the last checkpoint \
             are lost; the index is not)",
            moved
                .first()
                .map(|(_, to)| to.display().to_string())
                .unwrap_or_default()
        );
        return WalRecovery::Recovered;
    }

    // Base image is bad too. Restore the WAL so the quarantine that follows
    // preserves everything.
    for (from, to) in moved.iter().rev() {
        let _ = std::fs::rename(to, from);
    }
    WalRecovery::NotRecoverable
}

pub fn quarantine_graph(infigraph_dir: &Path, graph_name: &str) -> Result<PathBuf> {
    move_graph_aside(
        infigraph_dir,
        graph_name,
        CORRUPT_INFIX,
        QUARANTINE_RETENTION,
    )
}

/// Rename the current live graph aside as `<graph_name>.previous.<ts>` after
/// a successful full reindex has built its replacement, keeping a bounded
/// rollback pool of superseded-but-healthy graphs.
///
/// Deliberately a *separate* pool from `quarantine_graph`'s: that one exists
/// to preserve corruption evidence for human diagnosis (R3.1.2), and routine
/// full reindexes filing healthy graphs into it would both evict real
/// evidence and label healthy graphs `.corrupt.`.
///
/// Same locking contract as `quarantine_graph`: the caller must already hold
/// whatever write lock guards `graph_name`.
pub fn retire_previous_graph(infigraph_dir: &Path, graph_name: &str) -> Result<PathBuf> {
    move_graph_aside(
        infigraph_dir,
        graph_name,
        PREVIOUS_INFIX,
        PREVIOUS_RETENTION,
    )
}

/// Shared implementation behind both bounded aside-pools. `infix` selects
/// the pool (`"corrupt"` / `"previous"`); `retention` is that pool's bound.
fn move_graph_aside(
    infigraph_dir: &Path,
    graph_name: &str,
    infix: &str,
    retention: usize,
) -> Result<PathBuf> {
    evict_oldest_if_at_bound(infigraph_dir, graph_name, infix, retention)?;

    // now_epoch_secs() is second-granularity: two calls into the same pool
    // within one wall-clock second would collide on the destination name,
    // and fs::rename on Unix silently REPLACES an existing regular-file
    // destination -- destroying the earlier entry in a module whose whole
    // purpose is not losing data. Same bug class (and same fix) as
    // snapshot::create_snapshot: walk forward to a genuinely free stem.
    let ts = next_free_aside_ts(infigraph_dir, graph_name, infix, now_epoch_secs());
    let quarantine_stem = format!("{graph_name}.{infix}.{ts}");
    let quarantine_path = infigraph_dir.join(&quarantine_stem);
    let source = infigraph_dir.join(graph_name);

    // Size cap, corrupt pool only (see `quarantine_max_bytes`). Runs
    // before the rename so the oversized base never enters the pool at
    // all: the WAL family still gets relocated below (it shares the stem
    // with the manifest), preserving the actually-useful evidence.
    let cap = quarantine_max_bytes(infigraph_dir);
    if infix == CORRUPT_INFIX && cap > 0 {
        let base_size = entry_size_bytes(&source);
        if base_size > cap {
            let manifest_path = infigraph_dir.join(format!("{quarantine_stem}.manifest.json"));
            let manifest = serde_json::json!({
                "dropped_base_image": source.display().to_string(),
                "dropped_bytes": base_size,
                "cap_bytes": cap,
                "reason": "corrupt base image exceeded INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES;                            WAL-family siblings retained for forensics",
                "dropped_at_epoch": ts,
            });
            let _ = std::fs::write(
                &manifest_path,
                serde_json::to_string_pretty(&manifest).unwrap_or_default(),
            );
            remove_entry(&source);
            crate::audit::audit_log(
                "quarantine",
                "drop-oversized-corrupt-base",
                &format!("{base_size} bytes exceeded the {cap}-byte quarantine cap"),
                &source.display().to_string(),
            );
            relocate_wal_family(infigraph_dir, graph_name, &quarantine_stem, &source);
            return Ok(manifest_path);
        }
    }

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
    relocate_wal_family(infigraph_dir, graph_name, &quarantine_stem, &source);

    crate::audit::audit_log(
        "quarantine",
        if infix == CORRUPT_INFIX {
            "quarantine-corrupt-graph"
        } else {
            "retire-previous-graph"
        },
        if infix == CORRUPT_INFIX {
            "graph failed a corruption verdict"
        } else {
            "superseded by a successful full reindex"
        },
        &quarantine_path.display().to_string(),
    );

    Ok(quarantine_path)
}

/// Move every WAL-family sibling of `source` beside the pool entry, as
/// flat files sharing its stem (e.g. "graph.corrupt.<ts>.wal") -- see the
/// long comment at the call site in `move_graph_aside` for why the
/// copy+remove fallback inside `move_wal_sibling` is load-bearing.
fn relocate_wal_family(
    infigraph_dir: &Path,
    graph_name: &str,
    quarantine_stem: &str,
    source: &Path,
) {
    for path in crate::graph::wal_family_paths(source) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        // name looks like "<graph_name>.wal" or "<graph_name>.wal.checkpoint";
        // keep everything after "<graph_name>" so the moved-aside name stays
        // recognizable and collision-free (e.g. "graph.corrupt.<ts>.wal.checkpoint").
        let suffix = name.strip_prefix(graph_name).unwrap_or(&name).to_owned();
        let dest = infigraph_dir.join(format!("{quarantine_stem}{suffix}"));
        if let Err(e) = move_wal_sibling(&path, &dest) {
            eprintln!(
                "[quarantine] warning: could not relocate WAL sibling {} into quarantine ({e:#}) \
                 -- it may be destroyed by fallback cleanup at its original path",
                path.display()
            );
        }
    }
}

/// Total size of a pool entry or source graph: the file's length, or a
/// directory's recursive sum (legacy directory layout).
fn entry_size_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            total += entry_size_bytes(&e.path());
        }
    }
    total
}

/// Delete a base entry, dispatching on file-vs-directory (remove_dir_all
/// errors on a plain file, and that error used to be silently swallowed
/// elsewhere -- see evict_oldest_if_at_bound's same dispatch).
fn remove_entry(path: &Path) {
    let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    if is_dir {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// First timestamp `>= start_ts` whose pool stem
/// (`<graph_name>.<infix>.<ts>`) is genuinely unused: neither the base
/// entry itself nor any sibling sharing the stem (e.g. `<stem>.wal`, a
/// stray leftover from a partial earlier move) exists. Checking siblings
/// too -- rather than a bare `exists()` on the base path -- matters
/// because the WAL-relocation loop in `move_graph_aside` writes
/// `<stem>.wal`-style names, and `move_wal_sibling`'s copy fallback would
/// silently overwrite one just as `fs::rename` overwrites the base.
///
/// The `stem == name` / `starts_with("<stem>.")` split (not a plain
/// `starts_with(stem)`) keeps `graph.corrupt.17` from falsely matching
/// `graph.corrupt.170`'s entries.
fn next_free_aside_ts(infigraph_dir: &Path, graph_name: &str, infix: &str, start_ts: u64) -> u64 {
    let mut ts = start_ts;
    loop {
        let stem = format!("{graph_name}.{infix}.{ts}");
        let stem_dot = format!("{stem}.");
        let in_use = std::fs::read_dir(infigraph_dir)
            .map(|entries| {
                entries.flatten().any(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name == stem || name.starts_with(&stem_dot)
                })
            })
            .unwrap_or(false);
        if !in_use {
            return ts;
        }
        ts += 1;
    }
}

/// Move a WAL-family sibling alongside the base image it belongs to: try an
/// atomic rename first
/// (fast, same-filesystem, the common case), and if that fails, fall back to
/// copy+remove so a rename failure that copy wouldn't hit (e.g. certain
/// filesystem edge cases) doesn't silently lose the file. Once the copy
/// succeeds the content is safe in quarantine, so a failure removing the
/// now-redundant original afterward is not treated as an error -- the
/// caller's existing fallback cleanup will finish removing that leftover.
/// Only returns `Err` if the content could not be relocated at all.
pub(crate) fn move_wal_sibling(src: &Path, dest: &Path) -> Result<()> {
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

fn evict_oldest_if_at_bound(
    infigraph_dir: &Path,
    graph_name: &str,
    infix: &str,
    retention: usize,
) -> Result<()> {
    let prefix = format!("{graph_name}.{infix}.");
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
    if existing.len() < retention {
        return Ok(());
    }
    existing.sort_by_key(|(ts, _)| *ts);
    // existing.len() >= retention and we're about to add one more, so evict
    // enough of the oldest entries to land at retention - 1 before the new
    // one is created (bringing the total back to retention).
    let to_evict = existing.len() - (retention - 1);
    for (_, path) in existing.into_iter().take(to_evict) {
        discard_set_aside(&path);
    }
    Ok(())
}

/// Delete an entry `move_graph_aside` set aside, together with the
/// WAL-family siblings moved alongside it (`<entry>.wal`,
/// `<entry>.wal.checkpoint`, ...), so neither pool eviction nor a discard
/// leaks copies of those.
///
/// The entry is typically a plain FILE (lbug's graph is a single file), so
/// this dispatches on its type -- `remove_dir_all` alone errors on a file,
/// and that error used to be swallowed, evicting nothing.
pub(crate) fn discard_set_aside(entry: &Path) {
    remove_entry(entry);
    let (Some(dir), Some(name)) = (entry.parent(), entry.file_name()) else {
        return;
    };
    let sibling_prefix = format!("{}.", name.to_string_lossy());
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&sibling_prefix) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests probe IN PROCESS. The real probe re-invokes `current_exe()`,
    /// which in a test binary is libtest -- it would re-run this whole suite
    /// per call. That is exactly the fork bomb `PROBE_CAPABLE` now prevents;
    /// this keeps the tests honest about the file shuffling either way.
    fn in_process_probe(graph: &Path) -> bool {
        crate::graph::GraphStore::open_read_only(graph).is_ok()
    }

    /// A real graph with a deliberately corrupted WAL must be RECOVERED, not
    /// quarantined: the base image is intact and only the uncommitted tail is
    /// lost.
    ///
    /// This is the shape twelve of thirteen real quarantined images on this
    /// machine turned out to have -- indexes destroyed across eight repos
    /// over five weeks for a torn tail.
    #[test]
    fn a_torn_wal_over_an_intact_base_image_is_recovered_not_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph");
        {
            let store = crate::graph::GraphStore::open(&graph).unwrap();
            let conn = store.connection().unwrap();
            conn.query(
                "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
                 language: 'rust', symbol_count: 0})",
            )
            .unwrap();
        }
        // Garbage where a WAL record header belongs.
        std::fs::write(dir.path().join("graph.wal"), b"not a wal record at all").unwrap();

        let before = std::fs::metadata(&graph).unwrap().len();
        assert_eq!(
            try_recover_by_setting_wal_aside_with(dir.path(), "graph", in_process_probe),
            WalRecovery::Recovered,
            "the base image opens without the WAL, so this must recover"
        );

        assert!(graph.exists(), "the graph itself must be left in place");
        assert_eq!(
            std::fs::metadata(&graph).unwrap().len(),
            before,
            "recovery must not rewrite the base image"
        );
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("torn-wal")),
            "the torn WAL must be kept as evidence, got: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("corrupt")),
            "nothing should have been quarantined, got: {names:?}"
        );
    }

    /// A healthy graph with no WAL is `AlreadyHealthy`, not "recovered" and
    /// not "unrecoverable".
    ///
    /// The health question is asked BEFORE the WAL question precisely so this
    /// case gets a real answer. While the order was the other way round, a
    /// graph like this fell out as an undifferentiated `false` -- and five of
    /// the twelve quarantined images on this machine are exactly this shape:
    /// no WAL sibling at all, opening perfectly well.
    #[test]
    fn a_healthy_graph_with_no_wal_reports_already_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph");
        drop(crate::graph::GraphStore::open(&graph).unwrap());
        let verdict = try_recover_by_setting_wal_aside_with(dir.path(), "graph", in_process_probe);
        assert_eq!(verdict, WalRecovery::AlreadyHealthy);
        assert!(
            !verdict.permits_quarantine(),
            "a graph that opens must never license a quarantine"
        );
    }

    /// The complement: no WAL and the image really is unopenable. Nothing to
    /// set aside, nothing to save -- the caller's quarantine is correct.
    #[test]
    fn an_unopenable_graph_with_no_wal_is_not_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("graph"), b"not a database").unwrap();
        let verdict = try_recover_by_setting_wal_aside_with(dir.path(), "graph", in_process_probe);
        assert_eq!(verdict, WalRecovery::NotRecoverable);
        assert!(verdict.permits_quarantine());
    }

    #[test]
    fn next_free_aside_ts_returns_start_when_nothing_collides() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            next_free_aside_ts(dir.path(), "graph", "corrupt", 1000),
            1000
        );
    }

    #[test]
    fn next_free_aside_ts_walks_past_an_existing_base_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("graph.corrupt.1000"), b"earlier entry").unwrap();
        assert_eq!(
            next_free_aside_ts(dir.path(), "graph", "corrupt", 1000),
            1001
        );
    }

    #[test]
    fn next_free_aside_ts_walks_past_a_run_of_occupied_seconds() {
        let dir = tempfile::tempdir().unwrap();
        for ts in 1000..1003u64 {
            std::fs::write(dir.path().join(format!("graph.corrupt.{ts}")), b"x").unwrap();
        }
        assert_eq!(
            next_free_aside_ts(dir.path(), "graph", "corrupt", 1000),
            1003
        );
    }

    #[test]
    fn next_free_aside_ts_treats_a_stray_wal_sibling_as_occupied() {
        let dir = tempfile::tempdir().unwrap();
        // Only the sibling exists (partial earlier move) -- the stem must
        // still count as taken, or move_wal_sibling's copy fallback would
        // silently overwrite it.
        std::fs::write(dir.path().join("graph.corrupt.1000.wal"), b"stray").unwrap();
        assert_eq!(
            next_free_aside_ts(dir.path(), "graph", "corrupt", 1000),
            1001
        );
    }

    #[test]
    fn next_free_aside_ts_does_not_false_match_a_longer_timestamp_prefix() {
        let dir = tempfile::tempdir().unwrap();
        // "graph.corrupt.100" must not be blocked by "graph.corrupt.1000".
        std::fs::write(dir.path().join("graph.corrupt.1000"), b"other entry").unwrap();
        assert_eq!(next_free_aside_ts(dir.path(), "graph", "corrupt", 100), 100);
    }

    #[test]
    fn discarding_a_set_aside_graph_takes_its_wal_family_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "graph",
            "graph.previous.100",
            "graph.previous.100.wal",
            "graph.previous.100.wal.checkpoint",
            "graph.previous.1000",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }

        discard_set_aside(&dir.path().join("graph.previous.100"));

        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec!["graph", "graph.previous.1000"],
            "neither the live graph nor a longer timestamp sharing the prefix"
        );
    }

    #[test]
    fn next_free_aside_ts_pools_do_not_block_each_other() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("graph.previous.1000"), b"retired").unwrap();
        assert_eq!(
            next_free_aside_ts(dir.path(), "graph", "corrupt", 1000),
            1000
        );
    }

    /// A graph that opens fine AS IT STANDS must be left completely alone --
    /// its WAL included.
    ///
    /// Setting a WAL aside is only a recovery when the WAL is what blocks the
    /// open. Do it to a healthy graph and it is data loss: everything
    /// committed since the last checkpoint lives in that file and nowhere
    /// else. Seven of the fourteen quarantined images found on one machine
    /// opened cleanly WITH their WAL attached, so this is not a hypothetical
    /// input -- it is the single most common shape reaching this function.
    #[test]
    fn a_graph_that_already_opens_keeps_its_wal_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph");
        {
            let store = crate::graph::GraphStore::open(&graph).unwrap();
            let conn = store.connection().unwrap();
            conn.query(
                "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
                 language: 'rust', symbol_count: 0})",
            )
            .unwrap();
        }
        // A WAL that is present and perfectly readable -- the state a graph is
        // in whenever a writer checkpointed and left the file behind.
        let wal = dir.path().join("graph.wal");
        std::fs::write(&wal, b"").unwrap();
        assert!(
            in_process_probe(&graph),
            "precondition: this graph opens with its WAL in place, or the test \
             proves nothing"
        );

        assert_eq!(
            try_recover_by_setting_wal_aside_with(dir.path(), "graph", in_process_probe),
            WalRecovery::AlreadyHealthy,
            "a graph that already opens needs no recovery, so this must decline -- \
             and must say WHY it declined, or the caller cannot tell this apart \
             from an image that is beyond saving"
        );
        assert!(
            wal.exists(),
            "the WAL of a healthy graph must still be there -- moving it aside \
             discards every change committed since the last checkpoint"
        );
        let filed: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("torn-wal"))
            .collect();
        assert!(
            filed.is_empty(),
            "nothing should have been filed: {filed:?}"
        );
    }
}
