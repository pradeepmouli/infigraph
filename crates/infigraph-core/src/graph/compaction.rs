//! Scheduled compaction (#183): deciding when a graph holds enough dead
//! space to be worth rebuilding.
//!
//! lbug v0.20.4 exposes no compaction statement (pinned by
//! `does_the_engine_accept_a_compaction_statement`), so rebuild-and-swap is
//! the only reclamation route and this module only decides *when*.

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
