# R3.1.4 — WAL Auto-Recovery, Crash-Loop Breaker, and Quarantine/Disk Hardening

**Date:** 2026-08-24
**Status:** Draft, pending implementation plan
**Scope:** `docs/DESIGN-hardening.md` §3.1 R3.1.4, closing out GitHub issues [#115](https://github.com/pradeepmouli/infigraph/issues/115) and the still-open remainder of [#100](https://github.com/pradeepmouli/infigraph/issues/100)

## Motivation

`docs/DESIGN-hardening.md` R3.1.4 was filed after R3.1.1–R3.1.3 shipped, covering two related gaps hit for real on this machine: a WAL corruption from a dead process holder that refuses forever instead of self-healing (#115), and a quarantine pool with no size bound that once grew to 9.9G and filled the disk (#100).

**Scope correction, established before this design started:** the design doc's own "Not started" line for R3.1.4 is stale on the #100 side. Verified via `git log` against `feat/hardening` (not inferred from the doc): the quarantine byte-size cap (`quarantine_max_bytes`, commit `8f5aba8`, PR #102) and the `index --full`-under-daemon-backend race fix (commit `73ca85a`, PR #103) are both shipped and merged. The "inconsistent dead-pid" report in #100 was investigated and explained (successive dead holders reusing the lock, not a caching bug) — no code change needed. This design covers what is genuinely still open:

- #115(a) — auto-recovery: quarantine-then-refuse-forever has no self-healing path.
- #115(b) — degrade instead of going fully dark while recovery runs.
- #115's follow-up investigation comment: the "corruption" was actually a **cascade** (quarantine → rebuild → die → new corruption, twice within one hour) with no recorded cause for either death. This surfaces two new asks: per-instance daemon crash logs, and a crash-loop breaker — without the breaker, auto-recovery turns a one-time outage into an infinite auto-rebuild loop.
- #100 item 2 — the *live* graph (not just the quarantine pool) has been observed growing 40–70× its healthy size before crashing. Root cause (why Kuzu's WAL isn't checkpointing) is a separate, still-open instrumented investigation; this design only adds a circuit breaker against the symptom.
- #100's **second-incident comment** (2026-08-20, previously untriaged into any issue or design section): `infigraph doctor` mutates state (auto-spawns a watcher) while it's supposed to be observing it; a plain `infigraph index` after the graph file is deleted fails with a confusing Kuzu-internal error while `--full` silently works; failed recovery attempts leave lock-file remnants.

## Non-goals

- Root-causing *why* Kuzu's WAL fails to checkpoint/compact under the observed workloads (large codegen churn bursts, debug-profile daemon builds). That needs reproduction under instrumentation and stays its own follow-up.
- R6.1's full structured-tracing rollout. Component G below ships a minimal, standalone slice (stderr capture) — not the JSON `tracing` machinery.
- Any change to the daemon's single-writer architecture (R2.1.3/R2.3.8) itself. All new triggers route through the daemon's existing `WriteRequest`/`IndexWorkQueue` machinery, never bypass it.

## Architecture

Two decisions anchor every component below, both made explicitly during design review rather than defaulted:

1. **The read path never triggers a write directly.** When a read call (`open_read_only`) discovers a dead-holder WAL, it quarantines (a data-safety action, not a write to the live graph) and drops a sentinel file. It does not itself submit a `FullReindex` or spawn a daemon. The daemon's own write coordinator — which already polls `.infigraph/requests/` on a tick inside `run_write_coordinator` — is solely responsible for noticing the sentinel and acting on it. This preserves the R2.1.3 single-writer invariant: only the daemon's coordinator thread ever initiates a rebuild.
2. **Everything reuses existing machinery.** The `WriteRequest::FullReindex` build-fresh-then-swap-and-quarantine-the-old-one path (daemon-routed full reindex design, already shipped and tested) is the actual recovery mechanism — this design does not reinvent it, only decides when it fires automatically. Similarly, the crash-loop breaker and per-instance logging reuse the append-log convention already established by `dirty.rs`/`audit.rs`, and the disk-growth breaker reuses `check_disk_headroom`'s module and call sites rather than introducing a parallel preflight system.

## Components

### A — WAL auto-recovery (daemon-owned trigger)

**Current state, confirmed via code reading, not assumption:** `unclean_shutdown_wal_holder` firing on the *write* path (`GraphStore::open`) already funnels into `Infigraph::init`'s existing retry-then-quarantine logic (R3.1.1/R3.1.2) — that half works today. The gap is the *read-only* path: `GraphStore::open_read_only` detects the identical condition and constructs a `GraphCorruption`-typed error, but `get_doc_context` on `GraphCorruption` shows its only callers are tests and its own construction sites — **no production caller downcasts it**. A `search`/`get_code_snippet` call today just sees this bubble up as a generic tool error; nothing quarantines, nothing recovers.

**Fix:**
- `open_read_only`, on detecting a dead-holder WAL, calls the existing `quarantine_graph` immediately (instead of only constructing an error) and writes a sentinel file, `.infigraph/recovery-needed`, naming the quarantined path and timestamp.
- `run_write_coordinator`'s existing poll tick (alongside its `.infigraph/requests/` scan) checks for this sentinel. If present, and the crash-loop breaker (Component C) allows it, the coordinator submits its own internal `WriteRequest::FullReindex` — the same request type and code path `infigraph index --full` uses — and removes the sentinel once the request is accepted.
- **Scoped to daemon-backend mode.** Non-daemon/CLI mode has no live coordinator process to poll the sentinel asynchronously; it keeps today's behavior (quarantine happens, but a human runs `infigraph index --full` to rebuild). This is a deliberate scope line, not an oversight: automating this without a coordinator process would mean spawning a daemon purely to service a sentinel, which is a bigger behavior change than this design intends.

### B — Degrade instead of going dark

**Fix:** a new thin wrapper, `GraphStore::open_read_only_or_degrade()`, sits beside (does not replace) `open_read_only`. On the dead-holder-WAL path, after Component A's quarantine step, it looks for the most recent `graph.previous.<ts>` pool entry — the last **cleanly retired** healthy graph, already enumerable via `snapshot::list_restore_points` — and opens *that* path read-only instead of failing. It returns `(GraphStore, Option<DegradeReason>)`; callers that want auto-degrade (MCP `search`, `get_code_snippet`) call this wrapper and, when `DegradeReason` is `Some`, render a staleness banner reusing R3.3.6's existing banner mechanism and wording style — just triggered by "serving a pre-crash snapshot" instead of "files changed since last index." Internal/test call sites that need the strict behavior keep calling `open_read_only` directly; it is unchanged except for the quarantine-on-detection behavior from Component A, which is shared by both.

**No-fallback case:** if there is no `.previous.` entry (fresh project, first crash ever, or the pool was itself evicted), the wrapper returns the same hard refusal as today — but reworded from "run `infigraph index --full`" (implies manual action) to "auto-rebuild already in progress, retry shortly" (accurate, since Component A's daemon-side trigger already fired). No new fallback machinery is needed for this case.

### C — Crash-loop breaker

Directly answers the follow-up investigation on #115: the "corruption" was a cascade — quarantine → rebuild → die → new corruption, twice within roughly an hour, with the second death leaving no recorded cause anywhere (`~/Library/Logs/DiagnosticReports`, unified log, and `~/.infigraph/logs/` were all checked and came up empty). Component A without this would auto-retrigger every time, turning a one-time outage into an unbounded loop that burns a full rebuild each generation and never converges.

**Fix:** a small append-only log, `.infigraph/recovery-attempts.log`, one line per auto-triggered rebuild (timestamp + triggering pid), following the same shape as the existing `dirty.rs`/`audit.rs` append-logs rather than inventing a new persistence pattern. Before the coordinator (Component A) acts on a `recovery-needed` sentinel, it reads this log, filters to entries within the last hour, and:
- **< 2 entries in the window:** proceeds with `FullReindex`, appends a new entry.
- **≥ 2 entries in the window:** refuses to auto-rebuild. Removes the sentinel (so a future read doesn't re-check indefinitely) and instead leaves a "crash-loop detected" marker that the next read call surfaces as a hard, distinctly-worded error naming the prior attempt timestamps — not the generic quarantine-refusal wording, so a human immediately understands this is qualitatively different from a single crash.

**Window/threshold:** N=2 within 1 hour — matches the observed incident exactly (two generations within ~1 hour) and mirrors the existing `QUARANTINE_RETENTION = 2` convention already used elsewhere in this codebase.

### D — Disk-growth circuit breaker

Circuit breaker, not a root-cause fix — the *why* (Kuzu WAL not checkpointing under large-churn/debug-build conditions) stays a separate, explicitly-filed follow-up investigation. This only stops the symptom (a live graph ballooning to 40–70× its healthy size, observed twice, once filling the disk to 0 bytes free) from becoming an outage.

**Fix:** sibling to the existing `check_disk_headroom` (same module, `store_util.rs`, same call sites: `import_scip_index`, `upsert_files_bulk`, `GraphStore::upsert_file`). At successful full-reindex completion, stamp the graph's on-disk size as a "last healthy size" baseline (alongside the existing generation-ID bookkeeping in `GraphMeta`). Before a bulk write, compare the *current* on-disk graph size against that baseline × **10** (env-overridable via `INFIGRAPH_GRAPH_GROWTH_MAX_RATIO`, following the exact precedent set by `INFIGRAPH_QUARANTINE_MAX_BYTES`). Given observed incidents were 40–70×, 10× gives wide headroom for legitimate growth (large refactors, new language support landing, etc.) while still catching the actual pathological pattern well before it reaches disk-filling scale. Over threshold → refuse the write with a `Resource`-classed error (per the existing `4.1` error taxonomy: never a destructive recovery, just fail fast and actionably).

### E — Doctor becomes observe-only

**Fix:** `infigraph doctor` must never mutate the state it is inspecting — no check may trigger `ensure_daemon_running`, `ensure_daemon_watcher`, or any other side-effecting spawn. Confirmed clean already: `check_one_project_scip_staleness` uses `GraphStore::open_read_only` directly, no side effect. The offending path was not pinned down to an exact line in this design pass — the implementation plan should grep every check function in `doctor.rs` for any project-open that isn't already `_read_only`, and convert it. This keeps `doctor`'s original R6.4 contract ("PASS/WARN/FAIL with a remediation hint") honest — remediation is a hint for a human to act on, not something `doctor` does itself. A future `--repair` flag is explicitly out of scope here; this design only removes the unintended side effect.

### F — Plain `index` after corruption / lock remnants

Answers the second-incident comment's UX gap: after a corrupt graph file is deleted, plain `infigraph index` (non-`--full`) fails with Kuzu's own confusing "Cannot create an empty database under READ ONLY mode" error, while `--full` silently succeeds — a working recovery path that's undiscoverable from the error message. This is a distinct scenario from the already-fixed `plain_index_on_a_never_indexed_project_fails_fast_under_daemon_backend` test (which covers a project that was *never* indexed, not one whose graph was deleted post-corruption).

**Fix:** when a plain `index` is invoked against a project where `.infigraph/` exists (i.e., it has been indexed before) but the graph file itself is missing, auto-promote to the same full-rebuild path `--full` takes, rather than attempting — and failing — an incremental open. There is nothing incremental to protect in this state, so this is not a behavior change in outcome, only the removal of a confusing dead end.

This also subsumes the lock-remnant complaint (`graph.rebuilding.lock`/`index.lock` left behind by the failed attempt): removing the doomed code path removes the scenario that was leaking them. The existing RAII lock guards (`WriteLock`, the `IndexOpOutcome` guard) already release correctly on the paths that do complete; this needs a regression test proving the promoted-to-full-rebuild path leaves no stray locks, not new cleanup code.

### G — Per-instance daemon crash logs

Minimal, standalone slice of R6.1 (not blocked on the full structured-tracing rollout) — captures what a daemon already prints to stderr today, redirected to a discoverable, size-bounded file, reusing the existing `logrotate` module rather than a new one.

Confirmed: `build_daemon_command` (`watch/daemon.rs`) already opens a per-project `watch.log` in append mode across daemon generations — this path is not silently dropping output. #115's own investigation found `~/.infigraph/logs/` (the *global*, not per-project, log directory) contains only `audit.log`, and that the crashed instance's stderr was unrecoverable by any means checked. This strongly suggests the gap is in the **opportunistic auto-start path** (MCP's `ensure_daemon_watcher`), which may spawn through a different code path than `build_daemon_command` and not capture stderr at all — rather than a defect in the explicit-CLI-spawn path, which already appears to log correctly. The implementation plan should confirm which spawn path was actually in play for the crashed instance and ensure every detached-daemon spawn — explicit or opportunistic — routes stderr to a captured, `logrotate`-bounded destination.

## Data flow (Components A–C, the core recovery path)

```
MCP search / get_code_snippet
        │
        ▼
open_read_only_or_degrade()
        │  unclean_shutdown_wal_holder fires
        ▼
quarantine_graph()  ──────────────► graph.corrupt.<ts>  (unchanged mechanism)
        │
        ▼
write .infigraph/recovery-needed sentinel
        │
        ▼
look for graph.previous.<ts> (most recent)
   ├─ found  → open read-only, return with DegradeReason::PreCrashSnapshot
   └─ none   → refuse: "auto-rebuild in progress, retry shortly"

  ... meanwhile, asynchronously, in the daemon process ...

run_write_coordinator's poll tick
        │  sees recovery-needed sentinel
        ▼
check .infigraph/recovery-attempts.log (last 1h)
   ├─ < 2 entries → submit WriteRequest::FullReindex, append log entry, remove sentinel
   └─ ≥ 2 entries → remove sentinel, write crash-loop marker (no rebuild)
```

## Error handling

Follows the existing `4.1` error taxonomy throughout — no new classes introduced:
- Quarantine-triggered-but-recovering reads: not an error at all where a `.previous.` fallback exists (degraded success, banner-flagged); a `Corrupt`-classed refusal only where no fallback exists.
- Crash-loop detected: a distinct, clearly-worded `Corrupt`-classed refusal (never silently downgraded to the generic quarantine message — a human must be able to tell "this crashed once" from "this is looping").
- Disk-growth breaker: `Resource`-classed, matching `check_disk_headroom`'s existing classification — fail fast, actionable, never a destructive recovery attempt.
- Every quarantine, sentinel-driven rebuild, and crash-loop refusal writes an audit line via the existing `crate::audit::audit_log`, consistent with R6.3's existing coverage of quarantine moves.

## Testing shape

- **A/B:** real temp-dir integration tests in the style of `write_lock_edge_cases.rs`/`daemon_kuzu_e2e.rs` — seed a dead-holder WAL, drive a read call, assert quarantine + sentinel write, then assert correct behavior both with and without a `.previous.` entry present.
- **C:** unit tests on the attempts-log threshold logic in isolation, plus one integration test proving a simulated 3rd trigger inside the 1-hour window refuses instead of rebuilding, and a 3rd trigger *outside* the window rebuilds normally.
- **D:** unit tests on the ratio math (mirroring the existing `check_disk_headroom` test style), plus one integration test that seeds an oversized graph relative to its recorded healthy baseline and asserts the write is refused before it grows further.
- **E:** a `doctor` invocation (project or global scope) against a project with no watcher running, asserting no `watch.lock` (or any other side-effect file) is created as a result of running doctor.
- **F:** sibling scenario to the existing `plain_index_on_a_never_indexed_project_fails_fast_under_daemon_backend` test — a previously-indexed project with its graph file deleted; plain `index` succeeds via auto-promotion to full rebuild, and no stray `.lock` files remain afterward.
- **G:** spawn a daemon that panics deterministically (test-only trigger), assert its crash is captured in a discoverable log file rather than lost.

## Open items for the implementation plan (not blocking this design)

- Exact call site(s) in `doctor.rs` responsible for Component E's side effect — needs a grep pass, not guessed at here.
- Whether the opportunistic auto-start daemon spawn path (Component G) shares `build_daemon_command` or has a separate spawn path — needs tracing during implementation, not assumed here.
- Whether Components A–C ship as one PR or a stacked sequence (A+B together as the core recovery path, C as a near-immediate follow-up given it's a correctness requirement for A, not an enhancement) — left to the implementation plan, matching this codebase's existing stacked-branch convention (see PR #101–#103 for #100).
