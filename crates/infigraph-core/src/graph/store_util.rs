use std::collections::HashSet;
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

/// Batch-insert edges via UNWIND in chunks of 500, each chunk built by
/// `pair_edge_statement` so it plans as primary-key lookups. This is the
/// terminal fallback of `copy_edges_with_bad_record_retry`, so it runs on
/// exactly the batches COPY could not take -- the largest ones. It kept the
/// pre-2026-09-02 cross-product shape after the other write paths moved off
/// it, which wedged a sittir SCIP import at 100% CPU for 15+ minutes on a
/// 130k-symbol graph (500 pairs x |A| x |B| per chunk).
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
        let _ = conn.query(&pair_edge_statement(
            src_label,
            dst_label,
            rel_type,
            &pair_list.join(", "),
        ));
    }
}

/// The COPY error Kuzu reports when an edge endpoint names no row in the
/// node table. Shared by `extract_bad_copy_value` and the endpoint
/// pre-filter so the two can never drift apart.
const MISSING_PK_MARKER: &str = "Unable to find primary key value ";

/// Every primary key currently present in `label`'s node table.
///
/// One scan of the id column. Only ever called after a COPY has already
/// failed with a missing endpoint, so the happy path never pays for it.
fn existing_ids(conn: &Connection, label: &str) -> Result<HashSet<String>> {
    let result = conn
        .query(&format!("MATCH (n:{label}) RETURN n.id"))
        .map_err(|e| anyhow::anyhow!("failed to read {label} ids: {e}"))?;
    let mut ids = HashSet::new();
    for row in result {
        if let Some(v) = row.first() {
            ids.insert(unquote(v.to_string()));
        }
    }
    Ok(ids)
}

/// Kuzu renders a STRING value with surrounding double quotes; ids come back
/// through `Value::to_string` so strip one matched pair, and only a matched
/// pair -- an id that genuinely starts or ends with a quote must survive.
fn unquote(s: String) -> String {
    match s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(inner) => inner.to_string(),
        None => s,
    }
}

/// Drop every pair whose source or target names no existing node, in a
/// single pass over both id sets.
///
/// `extract_bad_copy_value` names only *one* offending value per COPY
/// failure, so draining a batch that way costs one whole re-COPY per bad
/// endpoint and cannot converge within `MAX_BAD_RECORD_RETRIES` when there
/// are more bad endpoints than retries. sittir's SCIP import hit exactly
/// that: attempts 17-20 dropped 254, 1, 2 and 253 records and still ran out,
/// dumping a 113k-edge batch into the slow fallback. Two id-column scans
/// settle it in one retry instead.
fn prefilter_pairs_against_existing(
    store: &GraphStore,
    pairs: &mut Vec<(String, String)>,
    src_label: &str,
    dst_label: &str,
) -> Result<()> {
    // A fresh connection, not the one that just failed: a caught COPY
    // failure can leave that connection's transaction bookkeeping wedged
    // (see this function's caller).
    let conn = store.connection()?;
    let src_ids = existing_ids(&conn, src_label)?;
    let dst_ids = if src_label == dst_label {
        None
    } else {
        Some(existing_ids(&conn, dst_label)?)
    };
    let dst_ids = dst_ids.as_ref().unwrap_or(&src_ids);
    pairs.retain(|(a, b)| src_ids.contains(a) && dst_ids.contains(b));
    Ok(())
}

/// Extract the offending value from a Kuzu COPY-failure error message, if it
/// looks like a bad-primary-key error recoverable by dropping just that
/// record and retrying -- a duplicate id within the batch (or one that
/// already exists in the graph), or an edge endpoint id that doesn't match
/// any existing row. Returns `None` for errors that don't name a specific
/// value, so callers know to fall back rather than retry blindly.
pub(crate) fn extract_bad_copy_value(err: &str) -> Option<&str> {
    for marker in ["duplicated primary key value ", MISSING_PK_MARKER] {
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
    // The endpoint pre-filter is a whole-batch fix, so it is worth at most
    // one attempt; if a missing endpoint is still reported afterwards the
    // cause is something the id sets cannot see, and the per-value drops
    // below take over.
    let mut prefiltered = false;
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
                if !prefiltered && msg.contains(MISSING_PK_MARKER) {
                    prefiltered = true;
                    let before = pairs.len();
                    match prefilter_pairs_against_existing(store, &mut pairs, src_label, dst_label)
                    {
                        Ok(()) if pairs.len() < before => {
                            eprintln!(
                                "warn: COPY {table} dropped {} pair(s) with a missing endpoint in one pass (attempt {}/{MAX_BAD_RECORD_RETRIES}), retrying",
                                before - pairs.len(),
                                attempt + 1
                            );
                            continue;
                        }
                        Ok(()) => {}
                        Err(pe) => eprintln!(
                            "warn: COPY {table} endpoint pre-filter failed ({pe}), falling back to per-value retries"
                        ),
                    }
                }
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

/// The statement that creates one edge per `{a: <src id>, b: <dst id>}`
/// row of `pairs_literal` (already escaped). Written as two `MATCH`
/// clauses on purpose: `MATCH (a:X), (b:Y) WHERE a.id = p.a AND b.id = p.b`
/// -- the shape every write path used until 2026-09-02 -- is planned by
/// lbug as CROSS_PRODUCT over full scans of both node tables, N x |X| x |Y|
/// per batch; sittir's 71-file incremental write spun a daemon at 100%
/// CPU for over 15 minutes on a 130k-symbol graph. This form plans as two
/// QUERY_PRIMARY_KEY_LOOKUPs (verified with EXPLAIN; see the test).
///
/// The split is needed *only* because `p.a` is a variable. A literal id
/// is pushed into a primary-key scan from either layout, so the
/// single-edge `MATCH (a:X), (b:Y) WHERE a.id = '..' AND b.id = '..'`
/// writes elsewhere in this crate are already optimal -- see
/// `only_variable_id_joins_need_the_split_match_form`.
pub(crate) fn pair_edge_statement(
    src_label: &str,
    dst_label: &str,
    rel: &str,
    pairs_literal: &str,
) -> String {
    format!(
        "UNWIND [{pairs_literal}] AS p MATCH (a:{src_label}) WHERE a.id = p.a \
         MATCH (b:{dst_label}) WHERE b.id = p.b CREATE (a)-[:{rel}]->(b)"
    )
}

/// One parent to many children (CONTAINS, DEFINES): the parent by primary
/// key, then each child id of `child_ids_literal` (already escaped, quoted)
/// by primary-key lookup -- not `s.id IN [...]`, which full-scans the
/// child table under a FILTER. See `pair_edge_statement`.
pub(crate) fn fanout_edge_statement(
    parent_label: &str,
    child_label: &str,
    rel: &str,
    parent_id_escaped: &str,
    child_ids_literal: &str,
) -> String {
    format!(
        "MATCH (m:{parent_label}) WHERE m.id = '{parent_id_escaped}' UNWIND [{child_ids_literal}] \
         AS cid MATCH (s:{child_label}) WHERE s.id = cid CREATE (m)-[:{rel}]->(s)"
    )
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
        check_disk_headroom, check_graph_growth_ratio, classify_file,
        copy_edges_with_bad_record_retry, extract_bad_copy_value, prefilter_pairs_against_existing,
        read_healthy_size, resolve_import_candidate, stamp_healthy_graph_size,
        stamp_healthy_graph_size_if_unset, MAX_BAD_RECORD_RETRIES,
    };

    /// A store holding Symbol nodes `s0..s{n}` and nothing else.
    fn store_with_symbols(dir: &std::path::Path, n: usize) -> super::super::GraphStore {
        let store = super::super::GraphStore::open(&dir.join("graph")).unwrap();
        {
            let conn = store.connection().unwrap();
            for i in 0..n {
                conn.query(&format!(
                    "CREATE (s:Symbol {{id: 'a.py::s{i}', name: 's{i}', kind: 'Function', file: 'a.py'}})"
                ))
                .unwrap();
            }
        }
        store
    }

    fn calls_edge_count(store: &super::super::GraphStore) -> usize {
        let conn = store.connection().unwrap();
        let mut r = conn
            .query("MATCH ()-[r:CALLS]->() RETURN count(r)")
            .unwrap();
        r.next()
            .and_then(|row| row.first().map(|v| v.to_string()))
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap()
    }

    #[test]
    fn prefilter_drops_every_pair_with_a_missing_endpoint_in_one_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_symbols(tmp.path(), 3);

        let mut pairs: Vec<(String, String)> = vec![
            ("a.py::s0".into(), "a.py::s1".into()),
            ("a.py::s0".into(), "a.py::ghost".into()),
            ("a.py::ghost".into(), "a.py::s2".into()),
            ("a.py::s1".into(), "a.py::s2".into()),
        ];
        prefilter_pairs_against_existing(&store, &mut pairs, "Symbol", "Symbol").unwrap();

        assert_eq!(
            pairs,
            vec![
                ("a.py::s0".to_string(), "a.py::s1".to_string()),
                ("a.py::s1".to_string(), "a.py::s2".to_string()),
            ],
            "both directions of a missing endpoint must go, and only those"
        );
    }

    /// The wedge behind sittir's 21-minute SCIP import (2026-09-02): a COPY
    /// error names one bad value, so dropping them one re-COPY at a time
    /// cannot converge once the batch holds more bad endpoints than
    /// `MAX_BAD_RECORD_RETRIES`. It then dumped the whole batch into the slow
    /// UNWIND fallback. The pre-filter must settle it and keep every good
    /// pair.
    #[test]
    fn copy_edges_converges_with_more_bad_endpoints_than_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_symbols(tmp.path(), 4);

        let good: Vec<(String, String)> = vec![
            ("a.py::s0".into(), "a.py::s1".into()),
            ("a.py::s1".into(), "a.py::s2".into()),
            ("a.py::s2".into(), "a.py::s3".into()),
        ];
        let bad_endpoints = MAX_BAD_RECORD_RETRIES * 3;
        let mut pairs = good.clone();
        for i in 0..bad_endpoints {
            pairs.push(("a.py::s0".into(), format!("a.py::ghost{i}")));
        }

        copy_edges_with_bad_record_retry(
            &store,
            "CALLS",
            pairs,
            "Symbol",
            "Symbol",
            &tmp.path().join("edges.parquet"),
        )
        .unwrap();

        assert_eq!(
            calls_edge_count(&store),
            good.len(),
            "every good pair must survive, and no ghost edge may be invented"
        );
    }

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

    /// The wedge behind sittir's incremental-write timeouts (2026-09-02): an
    /// edge batch written as `MATCH (a), (b) WHERE a.id = p.a AND b.id =
    /// p.b` plans as CROSS_PRODUCT over full scans of both node tables,
    /// N x |A| x |B| per batch; a 71-file write on a 130k-symbol graph spun
    /// for 15+ minutes. The helpers must plan as primary-key lookups.
    #[test]
    fn edge_statements_plan_as_primary_key_lookups_not_cross_products() {
        let tmp = tempfile::tempdir().unwrap();
        let store = super::super::GraphStore::open(&tmp.path().join("graph")).unwrap();
        let conn = store.connection().unwrap();
        let plan = |statement: &str| -> String {
            conn.query(&format!("EXPLAIN {statement}"))
                .unwrap()
                .map(|row| {
                    row.iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let pair = super::pair_edge_statement(
            "Symbol",
            "Symbol",
            "CALLS",
            "{a: 'a.py::f', b: 'a.py::g'}, {a: 'a.py::f', b: 'b.py::h'}",
        );
        let p = plan(&pair);
        assert!(!p.contains("CROSS_PRODUCT"), "pair edge plan:\n{p}");
        assert_eq!(
            p.matches("QUERY_PRIMARY_KEY_LOOKUP").count(),
            2,
            "pair edge plan:\n{p}"
        );

        let fanout = super::fanout_edge_statement(
            "File",
            "Symbol",
            "DEFINES",
            "a.py",
            "'a.py::f', 'a.py::g'",
        );
        let p = plan(&fanout);
        assert!(!p.contains("CROSS_PRODUCT"), "fan-out edge plan:\n{p}");
        assert!(
            p.contains("QUERY_PRIMARY_KEY_LOOKUP"),
            "fan-out edge plan:\n{p}"
        );
        assert!(
            p.contains("PRIMARY_KEY_SCAN_NODE_TABLE"),
            "fan-out edge plan:\n{p}"
        );
    }

    /// Why `Symbol.file` and `Symbol.name` do NOT carry ART secondary
    /// indexes, despite an index being worth 3.4x on the incremental
    /// reindex path (a 71-file cycle over a 130k-symbol graph: 2.88s ->
    /// 0.86s, ~20ms to build, ~2% write overhead).
    ///
    /// lbug's bulk `COPY` does not maintain ART indexes. The rows land and
    /// the primary key finds them, but an index-backed lookup on the copied
    /// column returns **zero rows, silently**. Every write path in this
    /// crate bulk-loads, so adding the index would make `WHERE s.file =
    /// '<literal>'` blind to every symbol in the graph -- and because
    /// `remove_file_conn` swallows its query errors, the per-file prune
    /// would simply stop deleting anything, with no error anywhere.
    /// Observed as three failures in `embed_skip.rs` ("symbols from the
    /// deleted file must be pruned") the moment the indexes were added on
    /// lbug 0.20.2.
    ///
    /// This test asserts the *bug*, so it starts failing once lbug fixes
    /// index maintenance under COPY -- at which point the indexes become
    /// safe to add and this test should be replaced by them.
    #[test]
    fn copy_does_not_maintain_art_indexes_so_they_cannot_be_added_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let store = super::super::GraphStore::open(&tmp.path().join("graph")).unwrap();
        let conn = store.connection().unwrap();
        conn.query("CREATE ART INDEX ix_probe_file FOR (n:Symbol) ON (n.file)")
            .unwrap();

        let csv = tmp.path().join("symbols.csv");
        std::fs::write(
            &csv,
            (0..3)
                .map(|i| format!("a.py::s{i},s{i},Function,a.py\n"))
                .collect::<String>(),
        )
        .unwrap();
        conn.query(&format!(
            "COPY Symbol (id, name, kind, file) FROM '{}'",
            csv.to_string_lossy()
        ))
        .expect("the COPY itself succeeds -- that is what makes this silent");

        let count = |q: &str| conn.query(q).unwrap().count();
        assert_eq!(
            count("MATCH (s:Symbol) RETURN s.id"),
            3,
            "the rows really are in the table"
        );
        assert_eq!(
            count("MATCH (s:Symbol) WHERE s.id = 'a.py::s1' RETURN s.id"),
            1,
            "and the primary-key index sees them"
        );
        assert_eq!(
            count("MATCH (s:Symbol) WHERE s.file = 'a.py' RETURN s.id"),
            0,
            "but the ART index does not -- if this now returns 3, lbug has fixed \
             index maintenance under COPY and Symbol.file/Symbol.name should be \
             indexed (see this test's doc comment for the measured win)"
        );
    }

    /// Which statement shapes lbug can plan as primary-key lookups, pinned
    /// against the planner itself.
    ///
    /// The rule is about the *id expression*, not the `MATCH` layout: a
    /// literal id is pushed into a PRIMARY_KEY_SCAN_NODE_TABLE whether it is
    /// written as one comma-separated `MATCH` or two, so the many
    /// single-edge `MATCH (a:X), (b:Y) WHERE a.id = '..' AND b.id = '..'`
    /// writes across this crate are already optimal and must not be churned
    /// "for consistency". A *variable* id (`a.id = p.a`, from `UNWIND`)
    /// cannot be pushed down: written as one `MATCH` it plans as
    /// CROSS_PRODUCT over full scans of both node tables and is what wedged
    /// sittir's SCIP import for 21 minutes. Split into two `MATCH` clauses
    /// (`pair_edge_statement`) the join disappears entirely.
    ///
    /// The CROSS_PRODUCT left in the constant-id plans joins two one-row
    /// inputs, so it is free -- counting full scans, not cross products, is
    /// what separates the two classes.
    #[test]
    fn only_variable_id_joins_need_the_split_match_form() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_symbols(tmp.path(), 200);
        let conn = store.connection().unwrap();
        let full_scans = |q: &str| -> usize {
            let p = conn
                .query(&format!("EXPLAIN {q}"))
                .unwrap()
                .map(|row| {
                    row.iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
                .join("\n");
            p.matches("SCAN_NODE_TABLE").count() - p.matches("PRIMARY_KEY_SCAN_NODE_TABLE").count()
        };

        // Literal ids: both layouts already plan as key lookups.
        assert_eq!(
            full_scans(
                "MATCH (a:Symbol), (b:Symbol) WHERE a.id = 'a.py::s1' AND b.id = 'a.py::s2' \
                 CREATE (a)-[:CALLS]->(b)"
            ),
            0
        );
        assert_eq!(
            full_scans(
                "MATCH (a:Symbol) WHERE a.id = 'a.py::s1' MATCH (b:Symbol) WHERE b.id = 'a.py::s2' \
                 CREATE (a)-[:CALLS]->(b)"
            ),
            0
        );

        // Variable ids: the layout is the whole difference.
        let lit = "{a: 'a.py::s1', b: 'a.py::s2'}, {a: 'a.py::s1', b: 'a.py::s3'}";
        assert_eq!(
            full_scans(&format!(
                "UNWIND [{lit}] AS p MATCH (a:Symbol), (b:Symbol) \
                 WHERE a.id = p.a AND b.id = p.b CREATE (a)-[:CALLS]->(b)"
            )),
            2,
            "if this stops full-scanning, lbug's planner improved and \
             `pair_edge_statement` may be redundant"
        );
        assert_eq!(
            full_scans(&super::pair_edge_statement(
                "Symbol", "Symbol", "CALLS", lit
            )),
            0
        );
    }
}
