use std::path::Path;

use anyhow::Result;
use kuzu::Connection;

use crate::graph::parquet_loader;
use crate::graph::store::GraphStore;

/// How many bad-record-drop-and-retry cycles a bulk COPY gets before giving
/// up and falling back to the slow per-row UNWIND path for whatever remains.
/// Bounded so a batch with many genuinely-bad rows still terminates instead
/// of looping indefinitely.
const MAX_BAD_RECORD_RETRIES: usize = 20;

/// Absolute floor of free space required before attempting any bulk write,
/// regardless of the projected write size -- Kuzu needs headroom for WAL and
/// temp buffers even for a small COPY.
const MIN_DISK_HEADROOM_BYTES: u64 = 200 * 1024 * 1024;

/// How many bytes of free space to require per byte of projected write size
/// (source data read, or an existing on-disk estimate). Conservative
/// multiplier for WAL/temp-buffer write amplification, not a precise model.
const DISK_HEADROOM_FACTOR: u64 = 3;

/// Check that the volume holding `dir` has enough free space to safely
/// attempt a write of roughly `projected_write_bytes`. Returns `Err` with a
/// human-readable shortfall description if not -- callers should abort the
/// write entirely rather than let Kuzu run out of disk mid-transaction,
/// which crashes the whole process with an uncaught C++ exception instead of
/// surfacing a `Result` (observed on sittir: SCIP enrichment's COPY ran the
/// volume out of space and crashed with `TransactionManagerException`).
///
/// If free space can't be determined (e.g. `dir` doesn't exist, or the
/// platform call fails), the check passes -- an unrelated I/O error here
/// shouldn't block a write; let the write itself surface any real problem.
pub(crate) fn check_disk_headroom(dir: &Path, projected_write_bytes: u64) -> Result<(), String> {
    let needed = projected_write_bytes
        .saturating_mul(DISK_HEADROOM_FACTOR)
        .max(MIN_DISK_HEADROOM_BYTES);
    match fs2::available_space(dir) {
        Ok(avail) if avail >= needed => Ok(()),
        Ok(avail) => Err(format!(
            "only {} MB free on the volume holding {}, need ~{} MB headroom for a ~{} MB write",
            avail / (1024 * 1024),
            dir.display(),
            needed / (1024 * 1024),
            projected_write_bytes / (1024 * 1024),
        )),
        Err(_) => Ok(()),
    }
}

/// Kept for `check_graph_growth_ratio`'s user-facing "override with ..."
/// hint; the value itself resolves through the `graph` settings group.
const GRAPH_GROWTH_MAX_RATIO_ENV: &str = "INFIGRAPH_GRAPH_GROWTH_MAX_RATIO";

/// Observed pathological incidents (github.com/pradeepmouli/infigraph#100)
/// were 40-70x a healthy graph's size; the default 10x gives wide headroom
/// for legitimate growth (large refactors, new language support landing)
/// while still catching the actual pattern well before it reaches
/// disk-filling scale. Resolved via the `graph` settings group
/// (`INFIGRAPH_GRAPH_GROWTH_MAX_RATIO`).
fn graph_growth_max_ratio() -> u64 {
    crate::graph::Graph::resolve(crate::graph::RawGraph::default(), None).growth_max_ratio
}

fn graph_health_path(infigraph_dir: &Path) -> std::path::PathBuf {
    infigraph_dir.join("graph.health.json")
}

fn read_healthy_size(infigraph_dir: &Path) -> Option<u64> {
    let content = std::fs::read_to_string(graph_health_path(infigraph_dir)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("healthy_size_bytes")?.as_u64()
}

/// Refreshes the recorded "last known healthy size" baseline. Call this
/// only after a *verified* healthy checkpoint -- a completed full rebuild
/// (build-fresh-then-swap succeeded and the swapped-in graph reopened), not
/// after an ordinary incremental write. Stamping on every incremental
/// success would let the baseline ratchet forward with the same
/// sub-threshold growth `check_graph_growth_ratio` exists to catch --
/// successive writes each just under the cap could grow the graph without
/// bound while every individual preflight passes (adversarial review
/// finding on R3.1.4). `pub` (not `pub(crate)`): the CLI's non-daemon full
/// reindex is the one call site outside this crate.
pub fn stamp_healthy_graph_size(infigraph_dir: &Path, graph_path: &Path) {
    let Ok(meta) = std::fs::metadata(graph_path) else {
        return; // nothing written yet -- nothing to stamp
    };
    let payload = serde_json::json!({ "healthy_size_bytes": meta.len() });
    let _ = crate::daemon_protocol::write_atomic(
        &graph_health_path(infigraph_dir),
        &serde_json::to_string_pretty(&payload).unwrap_or_default(),
    );
}

/// Bootstraps the baseline exactly once, after a project's very first
/// successful write -- a no-op once any baseline already exists. Call this
/// (unconditionally, it's cheap) after every ordinary write, alongside the
/// preflight `check_graph_growth_ratio` call before it.
///
/// This exists because `check_graph_growth_ratio`'s own "no baseline yet"
/// branch runs as a *preflight*, before the write it's guarding -- for a
/// project's first-ever write, that captures the graph's pre-write (bare
/// schema, no data) size, not what the graph actually looks like once
/// healthy. Left uncorrected, that undersized baseline makes the *next*
/// legitimate operation -- including a verified full rebuild -- look like
/// runaway growth and get spuriously refused. This helper lets the first
/// real post-write size win instead, without re-stamping (and so ratcheting)
/// on every write after that, the way the removed unconditional per-write
/// stamp used to.
pub(crate) fn stamp_healthy_graph_size_if_unset(infigraph_dir: &Path, graph_path: &Path) {
    if read_healthy_size(infigraph_dir).is_none() {
        stamp_healthy_graph_size(infigraph_dir, graph_path);
    }
}

/// Circuit breaker against the runaway-WAL-growth pattern from #100 (a live
/// graph observed growing 40-70x its healthy size before crashing). This is
/// NOT a fix for the underlying cause (why Kuzu's WAL isn't checkpointing
/// under the observed workloads) -- only a refusal before a write can push
/// the graph further into that pattern. Passes rather than refuses when no
/// baseline exists yet -- there's nothing to compare against -- but
/// deliberately does NOT establish one itself: this runs as a *preflight*,
/// before the write it guards, so stamping here would capture the graph's
/// pre-write size. `stamp_healthy_graph_size_if_unset`, called by the same
/// write paths *after* their write completes, is what actually bootstraps
/// the first real baseline.
pub(crate) fn check_graph_growth_ratio(
    infigraph_dir: &Path,
    graph_path: &Path,
) -> Result<(), String> {
    let Some(healthy) = read_healthy_size(infigraph_dir) else {
        return Ok(());
    };
    let graph_size = std::fs::metadata(graph_path).map(|m| m.len()).unwrap_or(0);
    // The checkpointed `graph` file alone isn't the whole story -- a
    // sittir incident (2026-08-31) crashed with `graph.wal` grown to ~97GB
    // while `graph` itself stayed small, which this check would have missed
    // entirely before this fix (it only ever stat'd `graph_path`). Sum in
    // every WAL-family sibling too, so uncommitted growth is caught, not
    // just checkpointed growth.
    let wal_size: u64 = crate::graph::store::wal_family_paths(graph_path)
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    if graph_size == 0 && wal_size == 0 {
        return Ok(()); // fresh/missing graph -- nothing to compare
    }
    let current = graph_size + wal_size;
    let max_allowed = healthy.saturating_mul(graph_growth_max_ratio());
    if current > max_allowed {
        return Err(format!(
            "graph at {} is {} MB ({} MB graph + {} MB WAL), {}x its recorded healthy size \
             ({} MB) -- refusing further growth (cap: {}x, override with \
             {GRAPH_GROWTH_MAX_RATIO_ENV}); this guards against the runaway-WAL-growth pattern \
             from github.com/pradeepmouli/infigraph#100 -- if this growth is legitimate, delete \
             {} to reset the baseline",
            graph_path.display(),
            current / (1024 * 1024),
            graph_size / (1024 * 1024),
            wal_size / (1024 * 1024),
            current / healthy.max(1),
            healthy / (1024 * 1024),
            graph_growth_max_ratio(),
            graph_health_path(infigraph_dir).display(),
        ));
    }
    Ok(())
}

/// Serialized-size proxy for a batch of file extractions, used as the
/// `check_disk_headroom` write estimate by bulk upsert/resolve paths that
/// have no raw source-file byte count on hand (unlike SCIP import, which
/// reads the whole `.scip` file into memory up front). Not exact -- the
/// actual on-disk write is Parquet/CSV, not JSON -- but proportional to
/// what's about to be written, which is what the headroom check needs.
pub(crate) fn estimate_extractions_write_bytes(
    extractions: &[crate::model::FileExtraction],
) -> u64 {
    serde_json::to_vec(extractions)
        .map(|v| v.len() as u64)
        .unwrap_or(0)
}

/// Escape single quotes and control characters for Kuzu string literals.
pub(crate) fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', " ")
        .replace('\r', "")
        .replace('\t', " ")
}

/// Convert a path to forward-slash form (needed on Windows for Kuzu COPY FROM).
pub(crate) fn fwd_slash_path(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Lookup stem for a Module file id (a path): final path segment with its file
/// extension stripped, lower-cased. "pkg/helpers.py" -> "helpers".
pub(crate) fn file_stem(path: &str) -> String {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    stem.to_lowercase()
}

/// Lookup stem for an import target module name: the last dotted/slashed
/// segment, lower-cased. Python "src.lib" -> "lib"; "pkg/mod" -> "mod";
/// bare "helpers" -> "helpers". Matches `file_stem` so an import resolves to
/// the imported file's Module node (whose stem is computed via `file_stem`).
pub(crate) fn import_stem(module_name: &str) -> String {
    module_name
        .rsplit(['.', '/', '\\'])
        .next()
        .unwrap_or(module_name)
        .to_lowercase()
}

/// Resolve an import's bare module name to a Module file path when the
/// basename stem has more than one candidate (e.g. both `app/service/constants.py`
/// and `app/test/e2e/constants.py` exist). Picks the candidate whose path,
/// normalized to `/`-separated lowercase segments, ends with `module_name`'s
/// own dotted/slashed segments -- so `app.service.constants` matches
/// `app/service/constants.py` but not `app/test/e2e/constants.py`. Returns
/// `None` (skip the edge) rather than guessing when no candidate matches or
/// more than one does -- a wrong IMPORTS edge is worse than a missing one.
pub(crate) fn resolve_import_candidate<'a>(
    module_name: &str,
    candidates: &[&'a str],
) -> Option<&'a str> {
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }
    let wanted: Vec<String> = module_name
        .split(['.', '/', '\\'])
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect();
    if wanted.len() < 2 {
        // Bare single-segment import name (e.g. "constants") carries no
        // disambiguating path info -- can't tell candidates apart.
        return None;
    }
    let mut matches = candidates.iter().filter(|file| {
        // Strip the extension from the basename only (matches `file_stem`),
        // not the whole path -- a dot in a directory name (e.g. "app/v1.2/x.py")
        // must not be mistaken for the extension separator.
        let (dir, base) = file.rsplit_once(['/', '\\']).unwrap_or(("", file));
        let base_no_ext = base.rsplit_once('.').map(|(b, _)| b).unwrap_or(base);
        let path_no_ext = if dir.is_empty() {
            base_no_ext.to_string()
        } else {
            format!("{dir}/{base_no_ext}")
        };
        let segments: Vec<String> = path_no_ext
            .split(['/', '\\'])
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
            .collect();
        segments.ends_with(&wanted)
    });
    let first = matches.next()?;
    if matches.next().is_some() {
        None
    } else {
        Some(first)
    }
}

/// Batch-insert edges via UNWIND in chunks of 500.
pub(crate) fn unwind_edges_from_pairs(
    conn: &Connection,
    pairs: &[(&str, &str)],
    rel_type: &str,
    src_label: &str,
    dst_label: &str,
) {
    const CHUNK: usize = 500;
    for chunk in pairs.chunks(CHUNK) {
        let pair_list: Vec<String> = chunk
            .iter()
            .map(|(a, b)| format!("{{a: '{}', b: '{}'}}", escape(a), escape(b)))
            .collect();
        let _ = conn.query(&format!(
            "UNWIND [{}] AS p MATCH (a:{src_label}), (b:{dst_label}) WHERE a.id = p.a AND b.id = p.b CREATE (a)-[:{rel_type}]->(b)",
            pair_list.join(", ")
        ));
    }
}

/// Extract the offending value from a Kuzu COPY-failure error message, if it
/// looks like a bad-primary-key error recoverable by dropping just that
/// record and retrying -- a duplicate id within the batch (or one that
/// already exists in the graph), or an edge endpoint id that doesn't match
/// any existing row. Returns `None` for errors that don't name a specific
/// value, so callers know to fall back rather than retry blindly.
pub(crate) fn extract_bad_copy_value(err: &str) -> Option<&str> {
    for marker in [
        "duplicated primary key value ",
        "Unable to find primary key value ",
    ] {
        if let Some(idx) = err.find(marker) {
            let rest = &err[idx + marker.len()..];
            let end = rest.find(", which").unwrap_or(rest.len());
            let val = rest[..end].trim_end_matches('.').trim();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

/// Bulk-COPY an edge table from `pairs`, retrying with the offending
/// endpoint's pairs dropped whenever the failure is a recoverable
/// bad-primary-key error (a duplicate edge, or an endpoint id that doesn't
/// exist in `src_label`/`dst_label`). Falls back to per-row UNWIND only once
/// retries are exhausted (`MAX_BAD_RECORD_RETRIES`) or the error isn't
/// recognized as recoverable -- avoids paying the slow UNWIND path for an
/// entire batch over a handful of bad rows.
///
/// Takes `store` rather than a borrowed `Connection`: a caught COPY failure
/// can leave Kùzu's internal transaction bookkeeping on that connection in a
/// state where the *next* statement fails immediately with `Invalid
/// transaction type to rollback.` (observed in production -- a Symbol-table
/// COPY's bad-PK retries left the connection wedged for the CALLS-table COPY
/// that followed on the same connection). None of these bulk loads are
/// wrapped in an explicit transaction, so sharing a connection across them
/// buys no atomicity -- asking `store` for a fresh one every attempt is
/// free of that risk and no more expensive (`GraphStore::connection` is a
/// cheap `Connection::new` per call).
pub(crate) fn copy_edges_with_bad_record_retry(
    store: &GraphStore,
    table: &str,
    mut pairs: Vec<(String, String)>,
    src_label: &str,
    dst_label: &str,
    edge_pq: &Path,
) -> Result<()> {
    // Every attempt is a whole new COPY -- re-check growth each time
    // (#132 gap 1) rather than trusting the caller's once-per-call preflight.
    let mut gate = store.growth_gate(1);
    for attempt in 0..MAX_BAD_RECORD_RETRIES {
        if pairs.is_empty() {
            return Ok(());
        }
        gate.tick()?;
        let conn = store.connection()?;
        let refs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        if parquet_loader::write_edge_parquet(edge_pq, &refs).is_err() {
            break;
        }
        match conn.query(&format!("COPY {table} FROM '{}'", fwd_slash_path(edge_pq))) {
            Ok(_) => {
                let _ = std::fs::remove_file(edge_pq);
                return Ok(());
            }
            Err(e) => {
                let msg = e.to_string();
                if let Some(bad) = extract_bad_copy_value(&msg) {
                    let before = pairs.len();
                    pairs.retain(|(a, b)| a != bad && b != bad);
                    if pairs.len() < before {
                        eprintln!(
                            "warn: COPY {table} dropped {} bad-PK record(s) (attempt {}/{MAX_BAD_RECORD_RETRIES}), retrying",
                            before - pairs.len(),
                            attempt + 1
                        );
                        continue;
                    }
                }
                eprintln!("warn: COPY {table} via parquet failed ({e}), falling back to UNWIND");
                break;
            }
        }
    }
    let conn = store.connection()?;
    let refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    unwind_edges_from_pairs(&conn, &refs, table, src_label, dst_label);
    let _ = std::fs::remove_file(edge_pq);
    Ok(())
}

pub fn classify_file(file: &str) -> &'static str {
    let fl = file.to_ascii_lowercase();
    if fl.ends_with("-lock.yaml")
        || fl.ends_with(".lock")
        || fl.contains("pnpm-lock")
        || fl.contains("package-lock")
        || fl.contains("yarn.lock")
    {
        return "config";
    }
    if fl.ends_with(".md") || fl.contains("/docs/") || fl.contains("/doc/") {
        return "docs";
    }
    if fl.ends_with(".yaml") || fl.ends_with(".yml") || fl.ends_with(".json") {
        if fl.contains("test")
            || fl.contains("eval")
            || fl.contains("golden")
            || fl.contains("dataset")
            || fl.contains("fixture")
        {
            return "test";
        }
        return "config";
    }
    if fl.contains("/test/")
        || fl.contains("/tests/")
        || fl.contains("/__tests__/")
        || fl.contains("/__mocks__/")
        || fl.starts_with("test_")
        || fl.contains("/test_")
        || fl.contains(".test.")
        || fl.contains(".spec.")
        || fl.contains("/e2e/")
        || fl.starts_with("e2e/")
        || fl.contains("/fixtures/")
        || fl.starts_with("fixtures/")
        || fl.contains("/testdata/")
        || fl.starts_with("testdata/")
        || fl.starts_with("__tests__/")
        || fl.starts_with("__mocks__/")
    {
        return "test";
    }
    "impl"
}

#[cfg(test)]
mod tests {
    use super::{
        check_disk_headroom, check_graph_growth_ratio, classify_file, extract_bad_copy_value,
        read_healthy_size, resolve_import_candidate, stamp_healthy_graph_size,
        stamp_healthy_graph_size_if_unset,
    };

    #[test]
    fn growth_check_passes_rather_than_refuses_with_no_baseline_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph");
        std::fs::write(&graph_path, vec![0u8; 1024]).unwrap();

        // Deliberately does NOT establish a baseline itself -- this is a
        // preflight, run before the write it guards, so stamping here would
        // capture the pre-write size. See `stamp_healthy_graph_size_if_unset`.
        assert!(check_graph_growth_ratio(tmp.path(), &graph_path).is_ok());
        assert!(!tmp.path().join("graph.health.json").exists());
    }

    #[test]
    fn stamp_if_unset_establishes_a_baseline_once_and_never_again() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph");
        std::fs::write(&graph_path, vec![0u8; 1024]).unwrap();

        stamp_healthy_graph_size_if_unset(tmp.path(), &graph_path);
        assert_eq!(read_healthy_size(tmp.path()), Some(1024));

        // A later call must not overwrite an already-established baseline --
        // that's the unconditional `stamp_healthy_graph_size`'s job, called
        // only at a verified full-rebuild checkpoint.
        std::fs::write(&graph_path, vec![0u8; 999_999]).unwrap();
        stamp_healthy_graph_size_if_unset(tmp.path(), &graph_path);
        assert_eq!(read_healthy_size(tmp.path()), Some(1024));
    }

    #[test]
    fn growth_check_refuses_once_current_size_exceeds_the_ratio() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph");
        std::fs::write(&graph_path, vec![0u8; 1_000_000]).unwrap();
        stamp_healthy_graph_size(tmp.path(), &graph_path); // baseline: ~1MB

        std::fs::write(&graph_path, vec![0u8; 20_000_000]).unwrap(); // 20x -- over the 10x default
        let err = check_graph_growth_ratio(tmp.path(), &graph_path)
            .expect_err("20x growth over a 1MB baseline must be refused at the 10x default");
        assert!(err.contains("healthy size"), "unexpected message: {err}");
    }

    #[test]
    fn growth_check_passes_for_ordinary_growth_under_the_ratio() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph");
        std::fs::write(&graph_path, vec![0u8; 1_000_000]).unwrap();
        stamp_healthy_graph_size(tmp.path(), &graph_path);

        std::fs::write(&graph_path, vec![0u8; 3_000_000]).unwrap(); // 3x -- under the 10x default
        assert!(check_graph_growth_ratio(tmp.path(), &graph_path).is_ok());
    }

    #[test]
    fn growth_check_catches_runaway_wal_even_when_the_checkpointed_graph_stays_small() {
        // Reproduces the sittir incident (2026-08-31): `graph` itself never
        // grew past a healthy size, but `graph.wal` (uncommitted) ballooned
        // to ~97GB. Before this fix, check_graph_growth_ratio only ever
        // stat'd `graph_path` and would have passed this case cleanly.
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph");
        std::fs::write(&graph_path, vec![0u8; 1_000_000]).unwrap();
        stamp_healthy_graph_size(tmp.path(), &graph_path); // baseline: ~1MB

        // graph stays small...
        std::fs::write(&graph_path, vec![0u8; 1_000_000]).unwrap();
        // ...but its WAL balloons to 20x the baseline.
        let wal_path = tmp.path().join("graph.wal");
        std::fs::write(&wal_path, vec![0u8; 20_000_000]).unwrap();

        let err = check_graph_growth_ratio(tmp.path(), &graph_path)
            .expect_err("a runaway WAL must be caught even when the checkpointed graph is small");
        assert!(err.contains("MB graph"), "unexpected message: {err}");
        assert!(err.contains("MB WAL"), "unexpected message: {err}");
    }

    #[test]
    fn disk_headroom_passes_for_tiny_projected_write_on_real_dir() {
        // Real syscall against a real directory -- any dev/CI machine has
        // far more than 200MB free on the temp volume, and a 1-byte
        // projected write floors to the minimum headroom, not scaled up.
        assert!(check_disk_headroom(&std::env::temp_dir(), 1).is_ok());
    }

    #[test]
    fn disk_headroom_does_not_block_on_unreadable_path() {
        // Can't determine free space for a path that doesn't exist -- must
        // not block the write over an unrelated lookup failure.
        let bogus = std::path::Path::new("/does/not/exist/anywhere/infigraph-test");
        assert!(check_disk_headroom(bogus, 1024 * 1024 * 1024 * 1024).is_ok());
    }

    #[test]
    fn extracts_duplicated_primary_key_value() {
        // Real Kuzu COPY-failure text observed on sittir's SCIP enrichment.
        let err = "Found duplicated primary key value packages/codegen/src/compiler/link.ts::\"'\"0, which violates the uniqueness constraint of the primary key column.";
        assert_eq!(
            extract_bad_copy_value(err),
            Some("packages/codegen/src/compiler/link.ts::\"'\"0")
        );
    }

    #[test]
    fn extracts_unable_to_find_primary_key_value() {
        let err = "Unable to find primary key value packages/codegen/src/compiler/collect-slots.ts::scip-typescript npm @sittir/codegen 0.1.0 src/compiler/`collect-slots.ts`/findNestedSeparator().(rule).";
        assert_eq!(
            extract_bad_copy_value(err),
            Some(
                "packages/codegen/src/compiler/collect-slots.ts::scip-typescript npm @sittir/codegen 0.1.0 src/compiler/`collect-slots.ts`/findNestedSeparator().(rule)"
            )
        );
    }

    #[test]
    fn returns_none_for_unrecognized_errors() {
        assert_eq!(
            extract_bad_copy_value("Invalid transaction type to rollback."),
            None
        );
        assert_eq!(extract_bad_copy_value("No space left on device"), None);
    }

    #[test]
    fn impl_files() {
        assert_eq!(classify_file("src/main.rs"), "impl");
        assert_eq!(classify_file("lib/parser.py"), "impl");
        assert_eq!(classify_file("cmd/server/handler.go"), "impl");
    }

    #[test]
    fn test_files() {
        assert_eq!(classify_file("src/tests/unit.rs"), "test");
        assert_eq!(classify_file("test_parser.py"), "test");
        assert_eq!(classify_file("src/__tests__/App.test.tsx"), "test");
        assert_eq!(classify_file("src/handler.spec.ts"), "test");
        assert_eq!(classify_file("e2e/login.test.js"), "test");
        assert_eq!(classify_file("src/__mocks__/db.ts"), "test");
        assert_eq!(classify_file("testdata/input.go"), "test");
        assert_eq!(classify_file("src/fixtures/sample.py"), "test");
    }

    #[test]
    fn test_yaml_json_files() {
        assert_eq!(classify_file("tests/golden/expected.json"), "test");
        assert_eq!(classify_file("eval/dataset.yaml"), "test");
        assert_eq!(classify_file("fixtures/data.yml"), "test");
        assert_eq!(classify_file("config/settings.yaml"), "config");
        assert_eq!(classify_file("package.json"), "config");
    }

    #[test]
    fn config_files() {
        assert_eq!(classify_file("Cargo.lock"), "config");
        assert_eq!(classify_file("pnpm-lock.yaml"), "config");
        assert_eq!(classify_file("package-lock.json"), "config");
        assert_eq!(classify_file("yarn.lock"), "config");
        assert_eq!(classify_file("docker-compose.yml"), "config");
    }

    #[test]
    fn docs_files() {
        assert_eq!(classify_file("README.md"), "docs");
        assert_eq!(classify_file("docs/api.md"), "docs");
        assert_eq!(classify_file("doc/architecture.md"), "docs");
    }

    #[test]
    fn resolve_import_candidate_single_candidate_always_resolves() {
        assert_eq!(
            resolve_import_candidate("constants", &["app/service/constants.py"]),
            Some("app/service/constants.py")
        );
    }

    #[test]
    fn resolve_import_candidate_disambiguates_by_dotted_path() {
        let candidates = ["app/service/constants.py", "app/test/e2e/constants.py"];
        assert_eq!(
            resolve_import_candidate("app.service.constants", &candidates),
            Some("app/service/constants.py")
        );
        assert_eq!(
            resolve_import_candidate("app.test.e2e.constants", &candidates),
            Some("app/test/e2e/constants.py")
        );
    }

    #[test]
    fn resolve_import_candidate_skips_ambiguous_bare_name() {
        let candidates = ["app/service/constants.py", "app/test/e2e/constants.py"];
        assert_eq!(resolve_import_candidate("constants", &candidates), None);
    }

    #[test]
    fn resolve_import_candidate_skips_when_no_suffix_matches() {
        let candidates = ["app/service/constants.py", "app/test/e2e/constants.py"];
        assert_eq!(
            resolve_import_candidate("other.pkg.constants", &candidates),
            None
        );
    }
}
