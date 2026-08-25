# GraphStore Connection/Transaction Ownership Design

## Summary

Two real, currently-live bugs in the Kùzu local backend trace back to the same
root cause: `GraphStore` hands out bare `Connection` values and leaves every
caller to manage connection lifetime and transaction boundaries by hand. Some
callers reuse one connection across independent statements that never needed
to share state (causing a wedged-transaction failure); others believe they're
inside a real transaction when `raw_query`'s Kùzu implementation silently
no-ops `BEGIN`/`COMMIT`/`ROLLBACK` (causing real, undetected atomicity loss).
This is upstream-owned code (`intuit/infigraph`), not a fork invention — the
fix should land there first and be ported back.

## Bug 1: connection reuse wedges a later, unrelated COPY

**Evidence** (sittir's `daemon.log`, this session): a `COPY Symbol` bulk load
retried 3 times after bad-primary-key errors (dropping the offending rows and
retrying — normal, expected behavior). Immediately after, on the *same*
connection, a `COPY CALLS` bulk load failed on its first attempt with Kùzu's
internal `Invalid transaction type to rollback.` and fell back to the slower
per-row `UNWIND` path. Confirmed self-healing (UNWIND produces correct data,
per `store_bench::test_parquet_quality`) — low severity, but real and
reproducible in principle.

**Root cause:** `import_scip_index` (`crates/infigraph-core/src/scip/mod.rs`)
opens one `Connection` at the top of the function and reuses it across an
inline Symbol-node `COPY` block, then `copy_edges_with_bad_record_retry`
(`crates/infigraph-core/src/graph/store_util.rs:310`) for `CALLS` edges, then
again for `INHERITS` edges. None of these three bulk loads are wrapped in an
explicit `BEGIN TRANSACTION` — each `COPY` already auto-commits independently
regardless of connection sharing. So sharing the connection buys zero
atomicity here; it only creates exposure to whatever internal state a caught
COPY failure leaves behind. Two more call sites hit the same helper the same
way: `resolve/calls.rs:633` (single call, incremental resolution) and
`store_parquet.rs:556,576` (full-reindex bulk path).

**Fix:** `copy_edges_with_bad_record_retry` takes `store: &GraphStore`
instead of `conn: &Connection`, and calls `store.connection()?` fresh and
unconditionally at the top of every retry-loop iteration and before the
`UNWIND` fallback — no "is this a retry" bookkeeping, since
`GraphStore::connection()` (`store.rs:554`) already does
`Connection::new(&self.db)` fresh and cheaply every call. The inline
Symbol-`COPY` block in `import_scip_index` gets the same treatment (fetch a
fresh connection for its own retry loop, and again before handing off to the
edge-copy calls that follow it).

## Bug 2: `raw_query`'s transaction no-op silently breaks real atomicity elsewhere

**Evidence:** `KuzuBackend::raw_query` (`kuzu_backend.rs:173`) opens a fresh
connection per call, so it deliberately no-ops `BEGIN TRANSACTION` / `BEGIN` /
`COMMIT` / `ROLLBACK` when passed as a query string — documented as
intentional, to fail safe rather than let a stray `COMMIT` error with "No
active transaction." But two call sites don't know this and rely on it for
real atomicity:

- `concerns/mod.rs::write_concerns` (line 308): `BEGIN TRANSACTION`, then
  `DETACH DELETE` all existing `Concern` nodes, then loops `CREATE`-ing new
  ones, then `COMMIT` — believing this is one atomic unit.
- `reflection/mod.rs` (line 344): same shape.

Because `raw_query`'s `BEGIN`/`COMMIT` are no-ops, every statement in these
loops auto-commits independently and immediately. **A crash mid-loop today
already means the old data is gone and only part of the new data landed** —
live data-loss exposure, not hypothetical, currently masked because the
no-op returns `Ok` instead of erroring.

**Fix:** introduce `GraphStore::transaction<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T>` —
opens one connection, issues a real `BEGIN TRANSACTION`, runs the closure
against that connection, commits on `Ok`, rolls back on `Err`. Migrate every
real multi-statement-atomic call site onto it:

- `write_concerns` (fixes the live bug)
- `reflection/mod.rs`'s equivalent (fixes the live bug)
- `KuzuBackend::write_calls_service_edges` (already correct today — hand-rolled
  BEGIN/loop/COMMIT/ROLLBACK on one held connection — but duplicates the
  boilerplate `transaction()` centralizes)
- The two hand-rolled transaction blocks inside `KuzuBackend`'s bulk-index
  path (`kuzu_backend.rs:465,477` — delete-then-`COPY`, delete-then-`UNWIND`)

## Non-goals

- Not attempting the fully-encapsulated end state (`GraphStore` forwarding
  every `Connection` method so no caller ever imports the `Connection` type)
  discussed but explicitly deferred — the `_conn`-suffixed helper family is
  9 functions across `store.rs`/`store_parquet.rs`/`store_write.rs`, 7 of
  them upstream-shared; reworking all of them is a separate, larger project
  and a separate ask about whether it should go upstream.
- Not fixing the disk-growth circuit breaker's bare-refuse behavior (a
  separate, already-scoped follow-up: quarantine-and-auto-rebuild on a
  growth-ratio trip, gated to the reindex/full-rebuild write path only).

## Fork vs. upstream portability

Checked against `upstream/main` (`intuit/infigraph`, tip `1c3622f`, v3.2.16):

| Piece | Portability |
|---|---|
| `copy_edges_with_bad_record_retry` | **byte-identical** in both trees — same patch applies verbatim |
| `resolve/calls.rs::resolve_with_map` | 26 bytes different — trivial adaptation |
| `store_write.rs::upsert_file_conn` | 38 bytes different — trivial adaptation |
| `scip/mod.rs::import_scip_index` | ~2.2KB different — fork added the disk-growth-ratio preflight (`check_graph_growth_ratio`, fork-only) and SCIP-generation tracking; the fix must be layered around these, not replace them |
| `store_parquet.rs` two call sites | **signature differs**: upstream threads an extra `_witness: &WriteLock` param through `upsert_folders_bulk_conn`/`upsert_all_parquet_conn` that this fork dropped — port must preserve whichever convention each tree already uses |
| `raw_query`'s no-op guard, `write_concerns`, `reflection/mod.rs` equivalent | upstream-shared, not yet diffed line-by-line — assume near-identical pending the actual patch |
| `bump_ast_generation_conn`, `bump_scip_generation_conn` | fork-only (R3.3.3/R3.3.4 additions) — not part of this fix's scope, unaffected |

**Process:** implement and verify on a fresh `upstream/main` worktree first
(this repo's established convention — always cherry-pick onto fresh
`upstream/main` and independently verify before pushing, never trust
fork-approval alone). Open the upstream PR there. Once merged (or
independently, if upstream review stalls), port the same fix onto
`feat/hardening`, adapting for the fork-only disk-growth-ratio code in
`scip/mod.rs` and the dropped `WriteLock` witness parameter in
`store_parquet.rs`, and re-verify independently on the fork.

## Testing

- Regression test for Bug 1: force a bad-PK retry on one table's COPY
  followed immediately by a COPY on a different table, on what would
  previously have been the same connection — assert the second COPY
  succeeds cleanly rather than falling back to UNWIND.
- Regression test for Bug 2: inject a mid-loop failure into `write_concerns`
  (or a `store.transaction()`-based equivalent) and assert the old `Concern`
  nodes are still present afterward (proving the delete didn't commit ahead
  of the failed recreate) — this test should *fail* against today's code
  before the fix, proving the live bug.
- `store.transaction()` itself: commit-on-success and rollback-on-error unit
  tests, plus a test that a panic inside the closure doesn't leave a stuck
  `BEGIN` on the connection (connection is dropped, not reused, on panic).
