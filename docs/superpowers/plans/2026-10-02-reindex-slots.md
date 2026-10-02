# A Machine-Wide Cap on Concurrent Full Reindexes (#150, part 2) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans, with superpowers:test-driven-development inside each task.

**Goal:** At most `[index] max_concurrent_reindexes` daemons (default 2) run a full reindex at the same time on one machine. One full reindex of a 1,750-file project holds about 1.9 GB and parses on every core; several at once is what exhausts a machine.

**Architecture:** The SCIP indexer slot pool becomes one generic `SlotPool` with two named constructors. A daemon claims a reindex slot at the single place every in-daemon rebuild starts, `try_start_full_reindex`, before it takes any lock; with no slot free the request stays deferred and is retried each tick, so reads keep being served. The wait is a degraded mode, which is also what the CLI polls to stop its own deadline while the daemon waits.

**No spec.** The user ruled "cap how many daemons run a full reindex at once"; the decisions below are brainstorm's.

## Rulings (brainstorm, 2026-10-02)

| # | Ruling |
|---|---|
| R1 | One generic `SlotPool`, two named constructors. The SCIP caller keeps its behavior. |
| R2 | The cap sits at `try_start_full_reindex` only. Paths that do not pass through it stay uncapped and are named in the docs: a daemon's start, whole-project drains, `INFIGRAPH_BACKEND=kuzu` `index --full`, a first routed `index`. |
| R3 | Wait by returning the request to `deferred` before `begin_index_op`. The slot travels with the task and is dropped after the swap. |
| R4 | **A wait is bounded.** One named constant, 10 minutes: after it the daemon starts the rebuild anyway, with one warning, and clears the waiting mode. That is the pre-cap behavior, so it is safe; it stops one wedged rebuild from blocking every other daemon, and bounds the client's extension by the same constant. |
| R5 | Slot-wait time does not count against the client's 600s. FullReindex only, as an option on `WriteOpts`; no other write's timeout changes. The CLI prints one waiting line. A client that goes away withdraws the request, as today. |
| R6 | Compaction waits (up to R4's bound). Recovery of a faulted graph skips the cap: a faulted project is unusable until rebuilt. |
| R7 | A slot error fails open, with one warning. |
| R8 | Default 2, user layer only: `[index] max_concurrent_reindexes`, a second settings group in the `index` category, as `scip` has in `scip_slots` beside `scip_switch`. |
| R9 | A waiting rebuild is a degraded mode, set when the claim fails and cleared when it succeeds, when the request is dropped, or when R4's bound is reached. `in_use` is what the pool observes cheaply; no holder counting. |
| R10 | The MCP `index_project` tool's outer timeout is left alone. The docs say what happens to a rebuild that waits longer than it. |
| R11 | The real-process test holds the slot lock itself; no debug hook in product code. The "never two `graph.rebuilding`" assertion stays only if the remove-the-claim mutation fails it reliably. |

## Global constraints

- A wait never holds a lock: not `index.lock`, not `graph.lock`, not the write lock.
- A daemon that dies holding a slot frees it at once: the slot is an OS file lock. There is no ordering between waiting daemons.
- Tests pin `HOME` and the backend. Unit tests inject the pool (a tempdir) and the clock.
- DRY: one pool type, one degraded-mode wording, one constant for the bound.

## Tasks

1. **One slot pool.** `scip_slots.rs` becomes `slots.rs`: `SlotPool`, `Slot`, `try_claim`; constructors `SlotPool::scip_indexers()` and `SlotPool::full_reindexes()`. First step: a test that `[index] include` and `[index] max_concurrent_reindexes` resolve side by side from one config file; stop and report if two groups cannot share the category. Tests: the existing pool tests; the two pools do not take each other's slots; the env name is `INFIGRAPH_INDEX_MAX_CONCURRENT_REINDEXES`.
2. **The cap.** Claim in `try_start_full_reindex` before `begin_index_op`; no slot returns the request to `deferred`. The slot rides in the pending task and is released after `finish_full_reindex`, and at once if `begin_index_op` fails. `request_internal_rebuild` carries why (recovery or compaction); recovery skips the cap. R4's bound, with an injected clock. Fail open with one warning. Unit tests for each of those.
3. **The degraded mode.** `DegradedMode::ReindexWaitingForSlot`; set and cleared per R9. Unit tests for the three clearing paths and for `StatusReport.degraded`.
4. **The client's wait.** `WriteOpts` option used by the FullReindex submit: time the daemon reports waiting is not counted, bounded by R4's constant; one line printed. Unit test with an injected clock and status source.
5. **Real-process test.** `crates/infigraph-cli/tests/reindex_slots.rs`: cap 1, the test holds `slot-0.lock`, `index --full` waits; `doctor` shows the mode, a routed read still answers, no `index.lock` is held; releasing the lock completes the rebuild and clears the mode. Mutation: removing the claim fails it.
6. **Docs and gates.** AGENTS.md bullet (R2's list, R6, R4); DESIGN-hardening; R10's sentence after reading the MCP timeout and checking whether the daemon completes or drops the rebuild. fmt, clippy, core/cli/mcp tests single-threaded as numbers, the full hook on the final commit.

## Stop rules

Stop and report without committing when: two settings groups cannot share the `index` category; the claim cannot be placed before every lock; the client's pause would need a change to the wire protocol's frames.
