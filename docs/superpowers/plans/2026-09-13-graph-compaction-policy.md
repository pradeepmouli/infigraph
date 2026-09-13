# Graph Compaction Policy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the daemon rebuild a graph on its own once dead space has accumulated, instead of a user discovering the need after indexing has already stopped.

**Architecture:** A pure predicate compares per-table pages-per-row against a baseline recorded at the last rebuild. The write coordinator samples it only while idle; the existing whole-graph growth ratio forces a rebuild regardless of idleness when the graph nears refusal. Both paths submit the existing `WriteRequest::FullReindex` by writing a request file, exactly as `drain_recovery_sentinel` does.

**Tech Stack:** Rust, lbug/Kuzu via `GraphStore`, the `settings!` macro, `serde_json`.

**Spec:** `docs/superpowers/specs/2026-09-13-graph-compaction-policy-design.md`

## Global Constraints

- `ACCOUNTED_TABLES = ["Symbol", "File", "Module", "Statement"]` (`graph/store.rs:1706`). All four are **node** tables (`graph/schema.rs:77,96,111,133`), so `MATCH (n:Label) RETURN count(*)` is valid for each.
- `COORDINATOR_TICK = Duration::from_millis(200)` (`daemon/mod.rs:31`).
- Sampling interval **10 minutes**; minimum interval between automatic rebuilds **1 hour**. Both constants, not settings.
- Defaults: `compaction` **off**, `compaction_drift_ratio` **3**, `compaction_escalate_pct` **50**.
- `settings!` fields must implement `FromStr + FromTomlItem` — only `u64`, `String`, `Toggle` do. **No floats in settings.**
- The `settings!` macro has **no attribute capture**; per-field docs go in the comment block above it (`graph/mod.rs:54-76`), one bullet each.
- All graph reads go through `GraphStore::connection` or `GraphStore::open_read_only` — never a second `Database` on a path already open in this process (#149).
- Aggregates: `CAST(... AS INT64)`, selected **by column name**, never positionally.
- **Compaction must not consume `recovery.rs`'s crash-loop budget.** That budget protects corruption recovery, which is not discretionary; a compaction rebuild that exhausted it could leave a genuinely corrupt graph unrecovered. Compaction rate-limits itself via `stamped_at` in its own sidecar.
- Run tests per-crate (`-p infigraph-core`), never `--all` — this machine is disk-constrained. Confirm any suspected regression with `--test-threads=1` first. Pin `INFIGRAPH_BACKEND=kuzu`.

---

### Task 1: Settings — three new `[graph]` fields

**Files:**
- Modify: `crates/infigraph-core/src/graph/mod.rs:54-76` (comment block), `:77-87` (macro)
- Test: `crates/infigraph-core/tests/graph_settings.rs`

**Interfaces:**
- Produces: `Graph { compaction: Toggle, compaction_drift_ratio: u64, compaction_escalate_pct: u64 }`, read via `crate::graph::Graph::resolve(crate::graph::RawGraph::default(), scope)`. Tasks 5, 6, 7 consume these.

- [ ] **Step 1: Write the failing test**

Append to `crates/infigraph-core/tests/graph_settings.rs`:

```rust
#[test]
fn compaction_defaults_are_off_with_conservative_thresholds() {
    let g = resolve_graph();
    assert!(!g.compaction.0, "compaction must ship opt-in, off by default");
    assert_eq!(g.compaction_drift_ratio, 3);
    assert_eq!(g.compaction_escalate_pct, 50);
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test graph_settings compaction_defaults`
Expected: FAIL to compile — `no field 'compaction' on type 'Graph'`.

- [ ] **Step 3: Add the fields**

In `crates/infigraph-core/src/graph/mod.rs`, inside `crate::settings! { graph { ... } }`, after `doc_hnsw_threshold: u64 = 200_000,`:

```rust
        compaction: crate::settings::Toggle = crate::settings::Toggle(false),
        compaction_drift_ratio: u64 = 3,
        compaction_escalate_pct: u64 = 50,
```

- [ ] **Step 4: Document them in the comment block**

The macro captures no attributes, so append to the comment block above it:

```rust
// - compaction: opt-in automatic rebuild once dead space accumulates
//   (`graph::compaction`, #183). Off by default.
// - compaction_drift_ratio: how far a table's pages-per-row may drift above
//   its post-rebuild baseline before a rebuild is due. Measured churn
//   reached 4.05x with rows flat, so 3 fires while churn is still climbing.
// - compaction_escalate_pct: percent of `growth_max_ratio` at which a
//   rebuild is forced regardless of idleness. A percentage, not an absolute,
//   so a raised cap still escalates at the same relative point.
```

- [ ] **Step 5: Run the settings suite**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test graph_settings`
Expected: PASS, including `graph_group_defaults_match_the_pre_migration_values`.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/graph/mod.rs crates/infigraph-core/tests/graph_settings.rs
git commit -m "feat(core): add compaction settings to the graph group (#183)"
```

---

### Task 2: Measurement — `table_page_stats`

**Files:**
- Create: `crates/infigraph-core/src/graph/compaction.rs`
- Modify: `crates/infigraph-core/src/graph/mod.rs` (add `pub mod compaction;`)

**Interfaces:**
- Produces:
  - `pub const ACCOUNTED_TABLES: &[&str] = &["Symbol", "File", "Module", "Statement"];`
  - `pub struct TableStats { pub pages: u64, pub rows: u64 }` — derives `Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize`
  - `pub fn table_page_stats(store: &GraphStore, tables: &[&str]) -> Option<Vec<(String, TableStats)>>`

Returning **named** stats, not a bare `Vec<TableStats>`: every consumer needs the names, and zipping them back on at each call site is duplication waiting to drift.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/src/graph/compaction.rs` with only:

```rust
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
```

Add `pub mod compaction;` to `crates/infigraph-core/src/graph/mod.rs`.

- [ ] **Step 2: Run it and watch it fail**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib compaction::measurement`
Expected: FAIL to compile — `unresolved import super::table_page_stats`.

- [ ] **Step 3: Implement**

At the top of `crates/infigraph-core/src/graph/compaction.rs`:

```rust
//! Scheduled compaction (#183): deciding when a graph holds enough dead
//! space to be worth rebuilding.
//!
//! lbug v0.20.4 exposes no compaction statement (pinned by
//! `does_the_engine_accept_a_compaction_statement`), so rebuild-and-swap is
//! the only reclamation route and this module only decides *when*.

use super::GraphStore;
use std::path::Path;

/// Tables whose page accounting is tracked. Node tables, every one, so a
/// plain `MATCH (n:Label)` row count is valid for each.
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

/// One aggregate, by column name, or `None`.
///
/// Named rather than positional: a positional read of `storage_info` once
/// summed a legitimate `0` and reported "0 pages" for a 21MB graph, taking
/// none of its error paths. `CAST(... AS INT64)` because lbug sums UINT64
/// into a type its own binding panics on while materialising.
fn scalar_u64(conn: &kuzu::Connection<'_>, cypher: &str) -> Option<u64> {
    let mut res = conn.query(cypher).ok()?;
    let raw = res.next()?.first()?.to_string();
    if raw.is_empty() || raw.eq_ignore_ascii_case("null") {
        return Some(0); // sum() over an empty table is NULL -- a real zero
    }
    raw.parse::<u64>().ok()
}

/// `(table, stats)` per requested table, or `None` if anything failed.
///
/// Deliberately the inverse of the test instrument this productionises,
/// which panics rather than degrade: there, a measurement whose failure
/// mimics its hoped-for result is worse than none. Here it drives a policy,
/// and acting on unknown state is worse than missing a cycle -- so
/// unmeasurable resolves to "not due" at the call site.
pub fn table_page_stats(
    store: &GraphStore,
    tables: &[&str],
) -> Option<Vec<(String, TableStats)>> {
    let conn = store.connection().ok()?;
    tables
        .iter()
        .map(|t| {
            let pages = scalar_u64(
                &conn,
                &format!("CALL storage_info('{t}') RETURN CAST(sum(num_pages) AS INT64)"),
            )?;
            let rows = scalar_u64(
                &conn,
                &format!("MATCH (n:{t}) RETURN CAST(count(*) AS INT64)"),
            )?;
            Some(((*t).to_string(), TableStats { pages, rows }))
        })
        .collect()
}
```

- [ ] **Step 4: Run the test**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib compaction::measurement`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/graph/compaction.rs crates/infigraph-core/src/graph/mod.rs
git commit -m "feat(core): measure per-table pages and live rows (#183)"
```

---

### Task 3: The baseline sidecar

**Files:**
- Modify: `crates/infigraph-core/src/graph/compaction.rs`

**Interfaces:**
- Consumes: `TableStats` (Task 2).
- Produces:
  - `pub struct CompactionBaseline { pub stamped_at: u64, pub tables: BTreeMap<String, TableStats> }` — derives `Debug, Clone, Default, PartialEq, Serialize, Deserialize`
  - `pub fn read_compaction_baseline(infigraph_dir: &Path) -> CompactionBaseline`
  - `pub fn stamp_compaction_baseline(infigraph_dir: &Path, stats: &[(String, TableStats)])`

`stamped_at` is the epoch second the baseline was written. Because the baseline is re-stamped after every rebuild, **it doubles as the rate limiter** — no second log, and no reliance on file mtime.

- [ ] **Step 1: Write the failing tests**

Append to `crates/infigraph-core/src/graph/compaction.rs`:

```rust
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
        assert_eq!(back.tables.get("Symbol"), Some(&TableStats { pages: 100, rows: 2000 }));
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
        assert_eq!(back.tables.get("Symbol"), Some(&TableStats { pages: 120, rows: 2400 }));
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib compaction::baseline`
Expected: FAIL to compile — unresolved imports.

- [ ] **Step 3: Implement**

Append to the non-test portion:

```rust
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
/// drift.
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
```

- [ ] **Step 4: Run the tests**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib compaction::baseline`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/graph/compaction.rs
git commit -m "feat(core): persist a per-table compaction baseline (#183)"
```

---

### Task 4: The predicate — `compaction_due`

**Files:**
- Modify: `crates/infigraph-core/src/graph/compaction.rs`

**Interfaces:**
- Consumes: `TableStats`, `CompactionBaseline` (Tasks 2-3).
- Produces:
  - `pub const MIN_REBUILD_INTERVAL: Duration = Duration::from_secs(3600);`
  - `pub fn compaction_due(now: &[(String, TableStats)], baseline: &CompactionBaseline, drift_ratio: u64, now_secs: u64) -> bool`

`now_secs` is injected rather than read inside, so the retry gate is testable without sleeping.

- [ ] **Step 1: Write the failing tests**

Append to `crates/infigraph-core/src/graph/compaction.rs`:

```rust
#[cfg(test)]
mod predicate_tests {
    use super::{compaction_due, CompactionBaseline, TableStats};

    fn base(stamped_at: u64, entries: &[(&str, u64, u64)]) -> CompactionBaseline {
        CompactionBaseline {
            stamped_at,
            tables: entries
                .iter()
                .map(|(n, pages, rows)| {
                    ((*n).to_string(), TableStats { pages: *pages, rows: *rows })
                })
                .collect(),
        }
    }

    fn now(entries: &[(&str, u64, u64)]) -> Vec<(String, TableStats)> {
        entries
            .iter()
            .map(|(n, pages, rows)| ((*n).to_string(), TableStats { pages: *pages, rows: *rows }))
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
```

- [ ] **Step 2: Run them and watch them fail**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib compaction::predicate`
Expected: FAIL to compile — `unresolved import super::compaction_due`.

- [ ] **Step 3: Implement**

Append to the non-test portion:

```rust
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
pub fn compaction_due(
    now: &[(String, TableStats)],
    baseline: &CompactionBaseline,
    drift_ratio: u64,
    now_secs: u64,
) -> bool {
    if baseline.tables.is_empty() || drift_ratio == 0 {
        return false;
    }
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
```

- [ ] **Step 4: Run the tests**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib compaction::predicate`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/graph/compaction.rs
git commit -m "feat(core): a pure pages-per-row drift predicate (#183)"
```

---

### Task 5: The escalation accessor — `graph_growth_ratio`

**Files:**
- Modify: `crates/infigraph-core/src/graph/store_util.rs` (after `healthy_baseline_recorded`, `:160-162`)

**Interfaces:**
- Produces: `pub(crate) fn graph_growth_ratio(infigraph_dir: &Path, graph_path: &Path) -> Option<u64>`. Tasks 6 and 7 consume it.

- [ ] **Step 1: Write the failing tests**

Append inside `store_util.rs`'s existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn growth_ratio_is_none_without_a_baseline() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph_path = tmp.path().join("graph");
    std::fs::write(&graph_path, vec![0u8; 4096]).unwrap();
    assert_eq!(graph_growth_ratio(tmp.path(), &graph_path), None);
}

#[test]
fn growth_ratio_reports_how_far_past_the_baseline_the_graph_is() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph_path = tmp.path().join("graph");
    std::fs::write(&graph_path, vec![0u8; 4096]).unwrap();
    stamp_healthy_graph_size(tmp.path(), &graph_path);
    std::fs::write(&graph_path, vec![0u8; 4096 * 5]).unwrap();
    assert_eq!(graph_growth_ratio(tmp.path(), &graph_path), Some(5));
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib store_util::tests::growth_ratio`
Expected: FAIL to compile — `cannot find function graph_growth_ratio`.

- [ ] **Step 3: Implement**

```rust
/// How many times its recorded healthy size the graph currently is, or
/// `None` when no baseline exists.
///
/// `check_graph_growth_ratio` answers *pass or refuse*, not *how close* --
/// and compaction needs the distance so it can act before the refusal rather
/// than after. A named accessor rather than a richer return type on that
/// function, which all eight of its write-path call sites would only have to
/// ignore: the same reasoning as `healthy_baseline_recorded` (#180 ask 5).
///
/// Costs no new measurement -- these bytes are already stat'd on every write.
pub(crate) fn graph_growth_ratio(infigraph_dir: &Path, graph_path: &Path) -> Option<u64> {
    let healthy = read_healthy_size(infigraph_dir)?;
    if healthy == 0 {
        return None; // a zero baseline makes every ratio meaningless
    }
    Some(graph_family_bytes(graph_path) / healthy)
}
```

- [ ] **Step 4: Run the tests**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib store_util::tests::growth_ratio`
Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/graph/store_util.rs
git commit -m "feat(core): expose how far past its baseline a graph has grown (#183)"
```

---

### Task 6: `doctor` observability

**Files:**
- Modify: `crates/infigraph-core/src/doctor.rs` (after `check_growth_breaker`, `:548-553`)
- Test: `crates/infigraph-core/tests/doctor.rs`

**Interfaces:**
- Consumes: `compaction::{read_compaction_baseline, table_page_stats, compaction_due, TableStats, ACCOUNTED_TABLES}`, `Graph::resolve` (Tasks 1-4).
- Produces: `pub fn check_one_compaction_drift(project_path: &Path) -> Option<CheckResult>`, `pub fn check_compaction_drift(ctx: &DoctorContext) -> Vec<CheckResult>`.

Ships **regardless of the toggle**. Opening the graph read-only is precedented here — `check_one_project_scip_staleness` (`doctor.rs:983`) already does `GraphStore::open_read_only(&graph_path).ok()?`.

- [ ] **Step 1: Write the failing test**

Append to `crates/infigraph-core/tests/doctor.rs`:

```rust
#[test]
fn compaction_drift_is_not_reported_for_an_unindexed_project() {
    let dir = tempfile::tempdir().unwrap();
    assert!(infigraph_core::doctor::check_one_compaction_drift(dir.path()).is_none());
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test doctor compaction_drift`
Expected: FAIL to compile — `cannot find function check_one_compaction_drift`.

- [ ] **Step 3: Implement**

```rust
const COMPACTION_CATEGORY: &str = "graph compaction";

/// Per-table pages-per-row against the last rebuild's baseline (#183).
///
/// Observe-only: reads the sidecar, opens the graph read-only, changes
/// nothing. Reported whether or not automatic compaction is enabled --
/// naming the table that is bloating is useful either way.
pub fn check_one_compaction_drift(project_path: &Path) -> Option<CheckResult> {
    let infigraph_dir = project_path.join(".infigraph");
    let graph_path = infigraph_dir.join("graph");
    if !graph_path.exists() {
        return None; // nothing indexed here -- not this check's business
    }
    let label = format!("{}: graph compaction drift", project_path.display());

    let baseline = crate::graph::compaction::read_compaction_baseline(&infigraph_dir);
    if baseline.tables.is_empty() {
        return Some(CheckResult::warn(
            COMPACTION_CATEGORY,
            label,
            "no compaction baseline is recorded, so drift cannot be measured",
            "run `infigraph rebuild`, which records one after rebuilding compactly",
        ));
    }

    let store = crate::graph::GraphStore::open_read_only(&graph_path).ok()?;
    let tables: Vec<&str> = crate::graph::compaction::ACCOUNTED_TABLES.to_vec();
    let now = crate::graph::compaction::table_page_stats(&store, &tables)?;

    let scope = crate::settings_file::ConfigScope::of_infigraph_dir(Some(&infigraph_dir));
    let cfg = crate::graph::Graph::resolve(crate::graph::RawGraph::default(), scope);

    let worst = now
        .iter()
        .filter_map(|(name, cur)| {
            let base = baseline.tables.get(name)?;
            if cur.rows == 0 || base.rows == 0 || base.pages == 0 {
                return None;
            }
            let drift = (cur.pages as f64 / cur.rows as f64)
                / (base.pages as f64 / base.rows as f64);
            Some((name.clone(), drift))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1));

    let due = crate::graph::compaction::compaction_due(
        &now,
        &baseline,
        cfg.compaction_drift_ratio,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );

    match worst {
        Some((table, drift)) if due => Some(CheckResult::warn(
            COMPACTION_CATEGORY,
            label,
            format!(
                "`{table}` holds {drift:.2}x the pages per row it did after the last \
                 rebuild -- that space is dead row versions nothing reclaims"
            ),
            if cfg.compaction.0 {
                "automatic compaction is enabled; the daemon will rebuild when idle"
            } else {
                "run `infigraph rebuild`, or enable `[graph] compaction` to have the \
                 daemon do it when idle"
            },
        )),
        Some((table, drift)) => Some(CheckResult::pass(
            COMPACTION_CATEGORY,
            label,
            format!("worst drift is `{table}` at {drift:.2}x its post-rebuild baseline"),
        )),
        None => Some(CheckResult::pass(
            COMPACTION_CATEGORY,
            label,
            "no table has enough rows to measure drift yet",
        )),
    }
}

pub fn check_compaction_drift(ctx: &DoctorContext) -> Vec<CheckResult> {
    projects_in_scope(ctx)
        .iter()
        .filter_map(|p| check_one_compaction_drift(p))
        .collect()
}
```

- [ ] **Step 4: Register it**

Run `rg 'check_growth_breaker' --type rust` and add a `check_compaction_drift` call beside every registration site outside `doctor.rs`.

- [ ] **Step 5: Run the doctor suite**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test doctor`
Expected: PASS, including the pre-existing growth-breaker tests.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/doctor.rs crates/infigraph-core/tests/doctor.rs
git commit -m "feat(core): report per-table compaction drift in doctor (#183)"
```

---

### Task 7: Coordinator wiring

**Files:**
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (near `:1234`, the `drain_recovery_sentinel` call in `run_write_coordinator`)

**Interfaces:**
- Consumes: everything from Tasks 1-5.
- Produces: `fn escalation_due(infigraph_dir: &Path, graph_path: &Path) -> bool`, `fn submit_compaction_rebuild(infigraph_dir: &Path) -> std::io::Result<()>`, and a `last_compaction_sample: Option<Instant>` local.

Submits no new kind of write: it writes a `WriteRequest::FullReindex` request file exactly as `drain_recovery_sentinel` does, so only the coordinator ever submits and single-writer holds by construction.

- [ ] **Step 1: Write the failing test**

Append to `daemon/mod.rs`'s test module:

```rust
/// Escalation must fire on the byte ratio alone, with no per-table
/// measurement and no compaction baseline -- that is the whole point of
/// having a second, always-current signal.
#[test]
fn escalation_fires_on_the_byte_ratio_alone() {
    let tmp = tempfile::TempDir::new().unwrap();
    let infigraph_dir = tmp.path().to_path_buf();
    let graph_path = infigraph_dir.join("graph");
    std::fs::write(&graph_path, vec![0u8; 4096]).unwrap();
    crate::graph::stamp_healthy_graph_size(&infigraph_dir, &graph_path);
    std::fs::write(&graph_path, vec![0u8; 4096 * 6]).unwrap();

    assert!(
        escalation_due(&infigraph_dir, &graph_path),
        "6x against a 10x cap is past the 50% escalation point"
    );
}

#[test]
fn escalation_does_not_fire_below_the_threshold() {
    let tmp = tempfile::TempDir::new().unwrap();
    let infigraph_dir = tmp.path().to_path_buf();
    let graph_path = infigraph_dir.join("graph");
    std::fs::write(&graph_path, vec![0u8; 4096]).unwrap();
    crate::graph::stamp_healthy_graph_size(&infigraph_dir, &graph_path);
    std::fs::write(&graph_path, vec![0u8; 4096 * 3]).unwrap();

    assert!(!escalation_due(&infigraph_dir, &graph_path), "3x is below 5x");
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib daemon::tests::escalation`
Expected: FAIL to compile — `cannot find function escalation_due`.

- [ ] **Step 3: Implement the two helpers**

In `crates/infigraph-core/src/daemon/mod.rs`, beside the other free functions:

```rust
/// How long the coordinator waits between per-table compaction samples.
/// A constant, not a setting: nothing suggests a project-specific value, and
/// a graph cannot cross from below the drift ratio to past escalation inside
/// one interval.
const COMPACTION_SAMPLE_INTERVAL: Duration = Duration::from_secs(600);

/// Whether the graph is close enough to refusal that a rebuild should happen
/// regardless of idleness. Being refused blocks all indexing, so an
/// interruption is the lesser harm. Costs no new measurement.
fn escalation_due(infigraph_dir: &Path, graph_path: &Path) -> bool {
    let scope = crate::settings_file::ConfigScope::of_infigraph_dir(Some(infigraph_dir));
    let cfg = crate::graph::Graph::resolve(crate::graph::RawGraph::default(), scope);
    let Some(ratio) = crate::graph::store_util::graph_growth_ratio(infigraph_dir, graph_path)
    else {
        return false; // no baseline -- nothing to be close to
    };
    ratio.saturating_mul(100) >= cfg.growth_max_ratio.saturating_mul(cfg.compaction_escalate_pct)
}

/// Ask the coordinator to rebuild, the same way `drain_recovery_sentinel`
/// does: drop a `WriteRequest::FullReindex` into the requests directory.
///
/// Deliberately does NOT call `recovery::record_recovery_attempt` -- see
/// `compaction::MIN_REBUILD_INTERVAL`.
fn submit_compaction_rebuild(infigraph_dir: &Path) -> std::io::Result<()> {
    let request_path = infigraph_dir.join("requests").join("compaction.request");
    let serialized = serde_json::to_string(&crate::daemon_protocol::WriteRequest::FullReindex)
        .expect("WriteRequest::FullReindex always serializes");
    crate::daemon_protocol::write_atomic(&request_path, &serialized)
}
```

Match `write_atomic`'s real return type; if it returns `anyhow::Result<()>`, use that instead of `std::io::Result<()>`.

- [ ] **Step 4: Run the tests**

Run: `INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib daemon::tests::escalation`
Expected: PASS, 2 tests.

- [ ] **Step 5: Wire both paths into the tick**

Immediately after the `drain_recovery_sentinel` block at `:1234`:

```rust
            // #183: compaction. Escalation every tick -- it is two file
            // stats. The per-table measurement only while idle and at most
            // every COMPACTION_SAMPLE_INTERVAL, so the expensive read cannot
            // run at a bad time by construction.
            if compaction_enabled {
                let escalate = escalation_due(&infigraph_dir, &graph_path);
                let idle = !drain_in_flight
                    && !full_reindex_in_flight
                    && scip_import_in_flight.is_none();
                let sample_due = last_compaction_sample
                    .is_none_or(|t: Instant| t.elapsed() >= COMPACTION_SAMPLE_INTERVAL);

                let drift = if idle && sample_due && !escalate {
                    last_compaction_sample = Some(Instant::now());
                    held_prism
                        .as_ref()
                        .and_then(|p| p.graph_store())
                        .and_then(|store| {
                            let now = crate::graph::compaction::table_page_stats(
                                store,
                                crate::graph::compaction::ACCOUNTED_TABLES,
                            )?;
                            let baseline =
                                crate::graph::compaction::read_compaction_baseline(&infigraph_dir);
                            let scope = crate::settings_file::ConfigScope::of_infigraph_dir(
                                Some(&infigraph_dir),
                            );
                            let cfg = crate::graph::Graph::resolve(
                                crate::graph::RawGraph::default(),
                                scope,
                            );
                            Some(crate::graph::compaction::compaction_due(
                                &now,
                                &baseline,
                                cfg.compaction_drift_ratio,
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0),
                            ))
                        })
                        .unwrap_or(false)
                } else {
                    false
                };

                if escalate || drift {
                    let projected = crate::graph::store_util::graph_family_bytes(&graph_path);
                    match crate::graph::store_util::check_disk_headroom(&infigraph_dir, projected) {
                        Ok(()) => {
                            eprintln!(
                                "[daemon] compaction: requesting a rebuild ({})",
                                if escalate { "escalated" } else { "drift" }
                            );
                            if let Err(e) = submit_compaction_rebuild(&infigraph_dir) {
                                eprintln!("[daemon] compaction: could not submit rebuild: {e}");
                            }
                        }
                        Err(e) => eprintln!("[daemon] compaction: skipped, {e}"),
                    }
                }
            }
```

Declare `let mut last_compaction_sample: Option<Instant> = None;` beside the loop's other mutable state, and resolve `let compaction_enabled = Graph::resolve(RawGraph::default(), scope).compaction.0;` once before the loop.

Adapt `held_prism.as_ref().and_then(|p| p.graph_store())` to however the loop reaches its open store — `Infigraph::graph_store` (`lib.rs:1095`) returns `Option<&GraphStore>` and is `None` for non-embedded backends. Never open a second `Database`.

- [ ] **Step 6: Re-stamp the compaction baseline after a rebuild**

Find where a completed `FullReindex` re-stamps `graph.health.json` (`rg 'stamp_healthy_graph_size' crates/infigraph-core/src/daemon/`) and add beside it, **after the swap and reopen**:

```rust
    if let Some(store) = prism.graph_store() {
        if let Some(stats) = crate::graph::compaction::table_page_stats(
            store,
            crate::graph::compaction::ACCOUNTED_TABLES,
        ) {
            crate::graph::compaction::stamp_compaction_baseline(&infigraph_dir, &stats);
        }
    }
```

This is what makes `MIN_REBUILD_INTERVAL` work: `stamped_at` advances on every rebuild.

- [ ] **Step 7: Full verification**

```bash
INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib -- --test-threads=1
INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --bins
INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-mcp --test concurrent_writer_reader
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

Expected: all pass, clippy clean. Confirm any failure single-threaded before believing it.

- [ ] **Step 8: Commit**

```bash
git add crates/infigraph-core/src/daemon/mod.rs
git commit -m "feat(core): schedule compaction from the write coordinator (#183)"
```

---

## Notes for the implementer

**`ACCOUNTED_TABLES` now lives in `compaction.rs`.** `store.rs`'s test module has its own private copy at `:1706`. Point that one at `compaction::ACCOUNTED_TABLES` rather than leaving two definitions of the same list — the same consolidation this codebase applied to the SCIP scratch-name format.

**Do not reuse `measure_churn`'s guard-disabling trick outside tests.** It pre-writes `{"healthy_size_bytes": 999999999}` to keep the growth guard asleep while measuring. Tests that drive a real rebuild loop need the same escape hatch; production never writes it.

**`infigraph-mcp`'s `concurrent_writer_reader` can fail intermittently on macOS** from a baseline stamped while the graph was still sub-megabyte — known, analysed at `store_util.rs:176-185`, and fixed in principle by `41ad37a`. Re-run before investigating.

**Why compaction does not use `recovery.rs`'s crash-loop breaker,** despite it being `pub` and a close fit: that budget protects corruption recovery. If compaction spent it, a subsequent real corruption would trip the breaker, write a crash-loop marker, and leave the graph unrecovered pending a human. A discretionary trigger must not be able to starve an essential one.
