# Graph compaction policy — design

Closes the remaining direction of [#183](https://github.com/pradeepmouli/infigraph/issues/183): make compaction a scheduled operation rather than something a user discovers the need for after indexing has already stopped.

## Problem

A graph under continuous churn grows without bound. `upsert_file` deletes and re-creates a file's `Symbol`/`Module`/`File` nodes, and the deleted row versions keep occupying pages the table itself owns. Nothing reclaims them.

The measurement is in `unchanged_content_churn_stays_within_its_measured_bound` (`crates/infigraph-core/src/graph/store.rs`), which drives `measure_churn` for 40 rounds of **byte-identical** content:

| series | result |
|---|---|
| `Symbol` row count | flat at exactly 2000 |
| `Symbol` `num_pages` | **4.05×** |
| `fsm_info` free list | oscillates, no trend |
| file bytes | **5.98×** |

Rows flat means `upsert_file` is logically correct and duplicates nothing. So the growth is dead row versions inside table-owned pages — which is also why the free list never moves.

In production this is not theoretical. One project reached 720.6 MB against an 81.2 MB healthy baseline, i.e. **9.30×** of a 10× guard that refuses *all* indexing when crossed. A rebuild returned it to 81.3 MB in 9 seconds — within 0.06% of the recorded baseline, so roughly **89% of the file was reclaimable dead space**.

Today the only remedy is a human noticing and running `infigraph rebuild`. The failure mode is silent until it is total.

## Constraints

**Rebuild-and-swap is the only reclamation route.** lbug v0.20.4 exposes no compaction or vacuum statement, pinned by `does_the_engine_accept_a_compaction_statement`. This design schedules an existing operation; it does not add an engine capability.

**The existing growth breaker cannot express this.** `check_graph_growth_ratio` is a ratio tripwire consulted *between* operations, so it cannot say "this round changed nothing and should therefore have cost nothing." That is how the production project above reported four consecutive clean rounds while going 77 MB → 388 MB.

**Single-writer.** Only the daemon's write coordinator may submit a rebuild, mirroring how `drain_recovery_sentinel` already turns a detected condition into a `WriteRequest::FullReindex`.

## The signal, and one rejected alternative

**Chosen: per-table pages per live row.** `storage_info(...)` `num_pages` against `count(*)`, per table over `ACCOUNTED_TABLES` (`Symbol`, `File`, `Module`, `Statement`). This is exactly the quantity that proved tombstoning, and it separates the two cases a byte-ratio conflates:

- genuine new code raises pages **and** rows together — ratio flat
- churn raises pages **alone** — ratio climbs

It is also per-table, so it names the write path at fault rather than reporting one aggregate.

**Rejected: `fsm_info` free pages.** This is the obvious idea and it does not work. lbug frees pages into the free-space manager, but #183's dead space never reaches the FSM — it stays in pages the table owns. The measured free list oscillates with no trend while `Symbol` pages quadruple. A policy triggered on free-page ratio would read approximately 0% dead space on a graph that is 89% reclaimable and would never fire. Recorded here because the idea is attractive enough to be re-proposed.

## Architecture

### A. Measurement — `table_page_stats`

Returns `Option<Vec<(pages, rows)>>` over `ACCOUNTED_TABLES`, read through `GraphStore::connection` — never a second `Database`, since there is exactly one per graph file per process (#149) and a second handle silently serves stale rows.

Productionises the private test helpers `live_pages` and `symbol_rows`, with **one deliberate inversion**. The test instrument panics rather than degrading to zero, on the reasoning that a measurement whose failure mode mimics its hoped-for result is worse than no measurement. In a policy that triggers expensive work the opposite holds: **unmeasurable means not due**. Acting on unknown state is worse than missing a cycle.

Aggregates must be `CAST(... AS INT64)` and named by column (`RETURN sum(num_pages)`), not read positionally — a positional read of `storage_info` once summed a legitimate `0` and reported "0 pages" for a 21 MB graph, taking none of its error paths and looking exactly like confirmation.

### B. Baseline — `graph.compaction.json`

Beside `graph.health.json`, not inside it. Holds per-table `{pages, rows}` as recorded immediately after a verified rebuild. Written with `daemon_protocol::write_atomic`.

Lifecycle mirrors `graph.health.json` exactly, and that lifecycle was arrived at the hard way:

- **bootstrap once** after the first successful write
- **never re-stamp on ordinary writes** — an unconditional per-write stamp lets the baseline ratchet forward with exactly the sub-threshold growth the guard exists to catch (adversarial-review finding on R3.1.4)
- **re-stamp only after a verified rebuild** whose swapped-in graph reopened

### C. Predicate — `compaction_due`

Pure, no I/O, mirroring `scip_enrichment_due`'s shape: inputs, a threshold, and a last-attempt gate against retry storms.

Fires when any table's pages-per-row exceeds `drift_ratio ×` its baseline pages-per-row. Three guards:

| condition | behaviour | why |
|---|---|---|
| no baseline | `false` | Never rebuild without a comparison. |
| `rows == 0` | skip that table | A near-zero denominator makes drift infinite. |
| recent attempt | `false` | Retry-storm gate, as `scip_enrichment_due` has. |

The `rows == 0` guard is not boilerplate. It is a live failure generalised: a baseline stamped while a graph was sub-megabyte made `0 * 10 == 0` and refused 3 MB of entirely legitimate data, surfacing as an intermittent CI failure. Pages-per-row has the identical shape one component over.

### D. Escalation — `graph_growth_ratio`

`check_graph_growth_ratio` returns `Result<(), String>` — pass or refuse, not *how close* — and `read_healthy_size` is private. A small `pub(crate) fn graph_growth_ratio(infigraph_dir, graph_path) -> Option<u64>` reuses `graph_family_bytes` and `read_healthy_size`.

Precedent is exact: `healthy_baseline_recorded` was added for #180 ask 5 rather than giving `check_graph_growth_ratio` a richer return type that all eight write-path call sites would have to ignore.

The bytes are already stat'd on every write path, so escalation costs no new measurement.

### E. Coordinator integration

In `run_write_coordinator`'s tick, where `drain_recovery_sentinel` is already consulted. Idle reuses the computation `try_start_scip_import` already performs: no drain, no full reindex, no SCIP import in flight. Only the coordinator submits.

### F. Settings — `[graph]` group

Field types must implement `FromStr + FromTomlItem`; today that is `u64`, `String`, and `Toggle` — so no floats.

| field | default |
|---|---|
| `compaction: Toggle` | `Toggle(false)` |
| `compaction_drift_ratio: u64` | `3` |
| `compaction_escalate_pct: u64` | `50` |

Env vars follow `INFIGRAPH_GRAPH_*`.

The sampling interval is a **constant of 10 minutes, not a setting**. Nothing suggests a project-specific value, and every knob is documentation, tests and support forever. Ten minutes is chosen against what the measurement costs and what it can miss: eight queries per sample is negligible at that spacing, and a graph cannot plausibly go from below the drift ratio to past the escalation point inside one interval — the fastest observed movement is tens of megabytes per import against hundreds of megabytes of headroom.

### G. Observability

`doctor` reports per-table pages-per-row against baseline and names the drifting table. **On regardless of the toggle** — this is what the original instrument was built for, and it is useful whether or not the automation is enabled.

## Data flow

```
coordinator tick (200ms)
├─ growth_ratio >= escalate_pct% of growth_max_ratio?
│    └─ yes -> submit FullReindex (ignores idle)
└─ else, if idle AND sample interval elapsed:
     table_page_stats -> compaction_due?
       └─ yes -> check_disk_headroom -> submit FullReindex

FullReindex (existing): build fresh -> swap -> retire_previous_graph
  -> re-stamp BOTH graph.health.json and graph.compaction.json, after the swap
```

The two triggers are **sequential, not competing**. Drift asks "is this worth doing now, while nobody is looking." Escalation asks "has waiting become unsafe." Without drift there is no idle path and the feature collapses into always-forced rebuilds.

## Thresholds and their justification

**`compaction_drift_ratio = 3`** — anchored to measurement, not invented. Sustained identical churn reached 4.05× on `Symbol` pages with rows flat, so 3× fires while churn is still climbing rather than after it plateaus, and sits far enough above 1× that ordinary variation will not trip it.

**`compaction_escalate_pct = 50`** — half of `growth_max_ratio`, so 5× at the default cap. Expressed as a percentage rather than an absolute so a user who raises their cap to 20× still escalates at halfway instead of at a hardcoded 5×, which would otherwise sit inside their normal operating range.

On the production project above, escalating at 5× leaves ~400 MB of headroom in hand; 8× would leave ~150 MB against observed increments of up to 49 MB. The earlier number costs an occasional unnecessary rebuild and buys never approaching the wall.

That project reached 5× a few hours after a rebuild, so it would fire several times a day — but that is one project's cadence on a codegen repo, deliberately not stated as a rate. Its per-import growth varied between +34 MB and +49 MB with no trend, and repeated attempts to read a trend out of that series were wrong every time. Step 2 of the rollout measures the real cadence instead of predicting it.

**Both numbers rest on one project plus one controlled instrument.** They ship as settings because revision is expected.

## Error handling

| condition | behaviour |
|---|---|
| measurement fails | not due — never act on unknown state |
| no baseline | bootstrap it; do not rebuild |
| `rows == 0` | skip that table |
| insufficient disk | skip and log; `check_disk_headroom` already refuses rather than filling the disk |
| repeated rebuilds | the baseline's own `stamped_at` gates a minimum of one hour between automatic rebuilds |

**Compaction keeps its own rate limit rather than using `recovery.rs`'s crash-loop breaker**, despite that breaker being `pub` and a close fit (`CRASH_LOOP_THRESHOLD = 2` within `CRASH_LOOP_WINDOW = 1h`). Its budget protects *corruption recovery*, which is not discretionary. If a compaction rebuild spent it, a subsequent real corruption would trip the breaker, write a crash-loop marker, and leave the graph unrecovered pending human intervention — a discretionary trigger starving an essential one.

Since the baseline is re-stamped after every rebuild, recording `stamped_at` inside it makes the sidecar its own rate limiter: no second log, and no dependence on file mtime.

Rebuild peak disk is old + new simultaneously (`PREVIOUS_RETENTION = 1`), so a rebuild needs free space roughly equal to the current graph.

## Testing

- **Predicate** — table-driven unit tests: drift below/above threshold, absent baseline, zero rows, retry gate. Mirrors `scip_enrichment_due`'s tests.
- **Sidecar lifecycle** — bootstrap-once, no-ratchet-on-ordinary-writes, re-stamp-after-verified-rebuild.
- **Escalation accessor** — ratio arithmetic including the absent-baseline case.
- **Integration** — reuse `measure_churn`'s shape to generate real drift and assert the predicate fires on real page counts rather than synthetic numbers.

Note `measure_churn` pre-writes `{"healthy_size_bytes": 999999999}` to keep the existing guard asleep during measurement. Any test driving the new baseline needs the same escape hatch.

## Rollout

1. Ship **off**. Observability on regardless.
2. Enable on one known project; observe which trigger fires and how often.
3. Revisit both defaults with that data. On-by-default is a separate decision, not implied by this one.

The sidecar is written from day one even with the toggle off, so step 3 has history rather than a cold start.

## Out of scope

- **On by default.** Its own decision, after step 3.
- **Auto re-stamping a baseline** when growth is legitimate rather than bloat. The refusal message deliberately no longer advises deleting `graph.health.json`, because bootstrapping then anchors at the bloated size — a one-way ratchet. Automating that is the same defect with better manners. A rebuild re-stamps through the one sanctioned path.
- **An engine-level compaction call.** None exists; see Constraints.
- **The remaining half of #180 ask 5** — distinguishing "never had a baseline" from "had one, deleted" — tracked separately as #185.
