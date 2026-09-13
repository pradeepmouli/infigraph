//! Scheduled compaction (#183): deciding when a graph holds enough dead
//! space to be worth rebuilding.
//!
//! lbug v0.20.4 exposes no compaction statement (pinned by
//! `does_the_engine_accept_a_compaction_statement`), so rebuild-and-swap is
//! the only reclamation route and this module only decides *when*.

use std::path::Path;

use super::GraphStore;
use kuzu::Connection;

/// Tables whose page accounting is tracked. Node tables, every one, so a
/// plain `MATCH (n:Label)` row count is valid for each.
///
/// `Module` and `Statement` stay in the list even where a write path carries
/// nothing for them: an empty table still reports its pages, so a regression
/// that starts writing them surfaces here rather than hiding inside a byte
/// total.
pub const ACCOUNTED_TABLES: &[&str] = &["Symbol", "File", "Module", "Statement"];

/// Pages a table occupies, against the rows actually live in it.
///
/// Their ratio separates the two causes of growth a byte count conflates:
/// genuine new code raises pages and rows together, while churn leaves
/// deleted row versions in table-owned pages and raises pages alone.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TableStats {
    pub pages: u64,
    pub rows: u64,
}

/// One aggregate, read out of a single-row result as a `u64`.
///
/// Aggregating inside Cypher, by column *name*, is the whole point. The first
/// cut of this read `storage_info`'s columns positionally and summed index 2
/// -- which holds a legitimate `0` -- so it reported "0 pages" for a 21MB
/// graph holding 2000 symbols, took none of its three error paths, and looked
/// exactly like confirmation of the hypothesis it was built to test.
/// `RETURN sum(num_pages)` names the column instead, so a reshaped output
/// fails loudly rather than quietly summing the wrong integer. lbug's own
/// `storage_info`/`fsm_info` tests query these results the same way, so this
/// is the engine's intended access path, not a trick.
///
/// Callers must wrap the aggregate in `CAST(... AS INT64)`. lbug sums a
/// UINT64 column into a wider logical type its own Rust binding cannot
/// convert, and the binding *panics* (`Unsupported type LogicalTypeID(43)`,
/// `logical_type.rs`) while materialising the row -- so no `map_err` here can
/// catch it. lbug's FSM test casts for the same reason.
///
/// Returns the engine's error text rather than a bare `None`: the test
/// instrument in `store.rs` panics with it, and "the query was malformed"
/// and "the engine reshaped its output" are different diagnoses. Callers that
/// only need a yes/no -- the policy path below -- discard it with `.ok()`.
pub(crate) fn scalar_u64(conn: &Connection<'_>, cypher: &str) -> Result<u64, String> {
    let mut res = conn.query(cypher).map_err(|e| e.to_string())?;
    let row = res.next().ok_or_else(|| "no rows".to_string())?;
    let raw = row
        .first()
        .ok_or_else(|| "no columns".to_string())?
        .to_string();
    // `sum()` over an empty table is NULL, and that is a real answer here:
    // no pages. Anything else that fails to parse is not.
    if raw.is_empty() || raw.eq_ignore_ascii_case("null") {
        return Ok(0);
    }
    raw.parse::<u64>()
        .map_err(|_| format!("expected an integer, got {raw:?}"))
}

/// `(table, stats)` per requested table, or `None` if anything failed.
///
/// Deliberately the inverse of the test instrument this productionises, which
/// panics rather than degrade: there, a measurement whose failure mode mimics
/// its hoped-for result is worse than none at all. Here it drives a policy,
/// and acting on unknown state is worse than missing a cycle -- so
/// unmeasurable resolves to "not due" at the call site.
///
/// Read through `GraphStore::connection` rather than a second `Database`,
/// because there is exactly one `Database` per graph file per process: a
/// second handle cannot see the first's uncommitted WAL and silently serves
/// stale or empty rows (#149). Here that would read as "pages never grew".
pub fn table_page_stats(store: &GraphStore, tables: &[&str]) -> Option<Vec<(String, TableStats)>> {
    let conn = store.connection().ok()?;
    tables
        .iter()
        .map(|t| {
            let pages = scalar_u64(
                &conn,
                &format!("CALL storage_info('{t}') RETURN CAST(sum(num_pages) AS INT64)"),
            )
            .ok()?;
            let rows = scalar_u64(
                &conn,
                &format!("MATCH (n:{t}) RETURN CAST(count(*) AS INT64)"),
            )
            .ok()?;
            Some(((*t).to_string(), TableStats { pages, rows }))
        })
        .collect()
}

/// Per-table stats as of the last verified rebuild, plus when they were
/// recorded.
///
/// `stamped_at` is load-bearing beyond documentation: it is how the policy
/// rate-limits itself, deliberately *not* by consuming `recovery.rs`'s
/// crash-loop budget. That budget protects corruption recovery, which is not
/// discretionary -- a compaction rebuild that exhausted it could leave a
/// genuinely corrupt graph unrecovered.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompactionBaseline {
    #[serde(default)]
    pub stamped_at: u64,
    #[serde(default)]
    pub tables: std::collections::BTreeMap<String, TableStats>,
}

fn compaction_baseline_path(infigraph_dir: &Path) -> std::path::PathBuf {
    infigraph_dir.join("graph.compaction.json")
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The recorded baseline, or an empty one.
///
/// Empty is a real answer meaning "no comparison is possible";
/// `compaction_due` returns false on it rather than treating absence as
/// drift. Missing, unreadable and corrupt all collapse here deliberately:
/// each means the same thing to the policy, and the worst outcome is a
/// missed cycle rather than a rebuild fired on a number nobody recorded.
pub fn read_compaction_baseline(infigraph_dir: &Path) -> CompactionBaseline {
    std::fs::read_to_string(compaction_baseline_path(infigraph_dir))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// Record a new baseline, replacing any existing one.
///
/// Call only after a *verified* rebuild -- build-fresh-then-swap succeeded
/// and the swapped-in graph reopened -- never after an ordinary write. See
/// `store_util::stamp_healthy_graph_size`: stamping on every write lets the
/// baseline ratchet forward with the very growth it exists to catch.
///
/// Failures are swallowed rather than propagated. A baseline that does not
/// persist costs one re-measured cycle; returning an error here would let a
/// full disk abort a rebuild that has already succeeded.
pub fn stamp_compaction_baseline(infigraph_dir: &Path, stats: &[(String, TableStats)]) {
    let baseline = CompactionBaseline {
        stamped_at: now_epoch_secs(),
        tables: stats.iter().cloned().collect(),
    };
    let Ok(json) = serde_json::to_string_pretty(&baseline) else {
        return;
    };
    let _ = crate::daemon_protocol::write_atomic(&compaction_baseline_path(infigraph_dir), &json);
}

/// Minimum wall-clock gap between automatic rebuilds.
///
/// One hour, matching `recovery::CRASH_LOOP_WINDOW` in spirit while
/// deliberately keeping a separate budget: compaction is discretionary and
/// must never be able to starve corruption recovery, which is not.
pub const MIN_REBUILD_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Whether any table's pages-per-row has drifted `drift_ratio`x above its
/// post-rebuild baseline, and enough time has passed since the last rebuild.
///
/// Pure, like `daemon::scip_enrichment_due` -- the caller owns the I/O, so
/// the decision itself is table-testable. `now_secs` is injected for the
/// same reason.
///
/// Compared by cross-multiplication rather than division: settings fields
/// cannot be floats, and integer division would round a genuine 2.9x down.
/// The products are widened to `u128` because `pages * rows * ratio`
/// overflows `u64` at realistic page counts.
///
/// The retry gate is checked before the per-table loop, so "a rebuild just
/// happened" is an unconditional veto no amount of drift can override --
/// otherwise a rebuild that failed to reclaim would re-trigger itself
/// immediately on the very same numbers.
pub fn compaction_due(
    now: &[(String, TableStats)],
    baseline: &CompactionBaseline,
    drift_ratio: u64,
    now_secs: u64,
) -> bool {
    if baseline.tables.is_empty() || drift_ratio == 0 {
        return false;
    }
    // Saturating, not plain subtraction: a baseline stamped in the future
    // (clock skew, a restored backup) would underflow into a huge elapsed
    // time and make compaction permanently due. Zero elapsed defers instead.
    if now_secs.saturating_sub(baseline.stamped_at) < MIN_REBUILD_INTERVAL.as_secs() {
        return false;
    }
    now.iter().any(|(table, cur)| {
        let Some(base) = baseline.tables.get(table) else {
            return false; // absent from the baseline -- not measured then
        };
        // A zero-row table has no meaningful pages-per-row, and a zero-page
        // baseline would make every later reading infinite drift.
        if cur.rows == 0 || base.rows == 0 || base.pages == 0 {
            return false;
        }
        // cur.pages/cur.rows > drift_ratio * base.pages/base.rows
        let lhs = (cur.pages as u128) * (base.rows as u128);
        let rhs = (drift_ratio as u128) * (base.pages as u128) * (cur.rows as u128);
        lhs > rhs
    })
}

#[cfg(test)]
mod measurement_tests {
    use super::table_page_stats;

    /// A fresh graph has schema but no rows. This must return a reading, not
    /// a failure -- "no rows yet" is a real answer the predicate handles by
    /// skipping the table.
    #[test]
    fn an_empty_graph_measures_zero_rows_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::graph::GraphStore::open(&dir.path().join("graph")).unwrap();
        let stats = table_page_stats(&store, &["Symbol"]).expect("measurable");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].0, "Symbol");
        assert_eq!(stats[0].1.rows, 0);
    }
}

#[cfg(test)]
mod baseline_tests {
    use super::{read_compaction_baseline, stamp_compaction_baseline, TableStats};

    fn t(name: &str, pages: u64, rows: u64) -> (String, TableStats) {
        (name.to_string(), TableStats { pages, rows })
    }

    #[test]
    fn a_stamped_baseline_reads_back_with_a_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        stamp_compaction_baseline(dir.path(), &[t("Symbol", 100, 2000)]);
        let back = read_compaction_baseline(dir.path());
        assert_eq!(
            back.tables.get("Symbol"),
            Some(&TableStats {
                pages: 100,
                rows: 2000
            })
        );
        assert!(back.stamped_at > 0, "must record when it was stamped");
    }

    #[test]
    fn no_sidecar_reads_as_empty_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let back = read_compaction_baseline(dir.path());
        assert!(back.tables.is_empty());
        assert_eq!(back.stamped_at, 0);
    }

    #[test]
    fn stamping_replaces_the_previous_baseline() {
        let dir = tempfile::tempdir().unwrap();
        stamp_compaction_baseline(dir.path(), &[t("Symbol", 100, 2000)]);
        stamp_compaction_baseline(dir.path(), &[t("Symbol", 120, 2400)]);
        let back = read_compaction_baseline(dir.path());
        assert_eq!(
            back.tables.get("Symbol"),
            Some(&TableStats {
                pages: 120,
                rows: 2400
            })
        );
    }
}

#[cfg(test)]
mod predicate_tests {
    use super::{compaction_due, CompactionBaseline, TableStats};

    fn base(stamped_at: u64, entries: &[(&str, u64, u64)]) -> CompactionBaseline {
        CompactionBaseline {
            stamped_at,
            tables: entries
                .iter()
                .map(|(n, pages, rows)| {
                    (
                        (*n).to_string(),
                        TableStats {
                            pages: *pages,
                            rows: *rows,
                        },
                    )
                })
                .collect(),
        }
    }

    fn now(entries: &[(&str, u64, u64)]) -> Vec<(String, TableStats)> {
        entries
            .iter()
            .map(|(n, pages, rows)| {
                (
                    (*n).to_string(),
                    TableStats {
                        pages: *pages,
                        rows: *rows,
                    },
                )
            })
            .collect()
    }

    const LATER: u64 = 100_000;

    /// The measured shape of #183: rows flat, pages climbing. 2000 rows in
    /// 100 pages became 2000 rows in 405 pages over 40 identical rounds.
    #[test]
    fn pages_climbing_on_flat_rows_is_due() {
        assert!(compaction_due(
            &now(&[("Symbol", 405, 2000)]),
            &base(1, &[("Symbol", 100, 2000)]),
            3,
            LATER,
        ));
    }

    /// Genuine new code raises pages AND rows, leaving the ratio flat. A
    /// rebuild reclaims nothing, so it must not fire however large the graph.
    #[test]
    fn proportional_growth_is_not_due() {
        assert!(!compaction_due(
            &now(&[("Symbol", 1000, 20_000)]),
            &base(1, &[("Symbol", 100, 2000)]),
            3,
            LATER,
        ));
    }

    /// Without a baseline there is nothing to compare against.
    #[test]
    fn an_absent_baseline_is_never_due() {
        assert!(!compaction_due(
            &now(&[("Symbol", 999, 1)]),
            &CompactionBaseline::default(),
            3,
            LATER,
        ));
    }

    /// A zero-row table makes pages-per-row infinite. This is the growth
    /// guard's near-zero-denominator bug one component over: skip the table
    /// rather than divide by it.
    #[test]
    fn a_zero_row_table_is_skipped_not_treated_as_infinite_drift() {
        assert!(!compaction_due(
            &now(&[("Statement", 50, 0)]),
            &base(1, &[("Statement", 0, 0)]),
            3,
            LATER,
        ));
    }

    /// Per-table precisely so one bloated table is not averaged away.
    #[test]
    fn one_drifting_table_among_several_is_due() {
        assert!(compaction_due(
            &now(&[("Symbol", 110, 1000), ("File", 40, 100)]),
            &base(1, &[("Symbol", 100, 1000), ("File", 10, 100)]),
            3,
            LATER,
        ));
    }

    /// The retry gate. Rebuilding again immediately after the last one would
    /// loop on a graph whose drift a rebuild cannot fix, so a rebuild inside
    /// MIN_REBUILD_INTERVAL is never due -- however bad the drift looks.
    #[test]
    fn a_recent_rebuild_defers_however_bad_the_drift() {
        assert!(!compaction_due(
            &now(&[("Symbol", 9999, 2000)]),
            &base(LATER - 60, &[("Symbol", 100, 2000)]),
            3,
            LATER,
        ));
    }
}
