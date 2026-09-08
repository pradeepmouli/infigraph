# Remaining Hardening Work — Status Audit & Design

**Date:** 2026-07-21
**Status:** Draft
**Parent:** `docs/DESIGN-hardening.md`
**Scope:** Every requirement in `DESIGN-hardening.md` §§1-10 not already closed by the shipped/merge-ready `write-safety-locks-design.md` PRs (PR1 `feat/lockfile-identity`, PR2 `feat/write-lock-enforcement`, PR3 `feat/index-operation-lock`, PR6 `feat/health-beacons`), plus full orphan/instance-lifecycle management (§2.2). **Explicitly excludes PR 5** (`docs/worker-architecture-rfc` — worker restructuring + Kuzu-vs-Cozo backend decision) and everything downstream of it (R2.1.3 "MCP as write coordinator" target architecture, R2.3.8 "real write coordination") — that's a separate, already-scoped, spec-only initiative with its own decision gate (backend choice), tracked independently. PR4 (`feat/shared-state-write-safety` — sessions + *project* registry `registry.json` write locks) is also pre-existing/separately planned and not duplicated here.

**Terminology note:** this doc introduces an **instance registry** (`~/.infigraph/instances/<pid>.json`, tracks live/dead MCP processes for orphan reaping, R2.2.1). This is a different thing from PR4's **project registry** (`~/.infigraph/registry.json`, the list of indexed project paths). Both are called "registry" in the parent spec; this doc always qualifies which one.

---

## 0. Method

Status below is graded by confidence:
- **Confirmed absent** — read the relevant source directly this session (or via `ls`, since Infigraph's own MCP tools were down for this audit) and verified the mechanism doesn't exist.
- **Confirmed partial** — some piece exists, verified directly; the rest doesn't.
- **Inferred absent** — no evidence found in extensive adjacent reading this session (e.g., zero `tracing::` call sites across every file read), but not exhaustively grep-verified (Infigraph's search was unavailable and the enforcement hook blocks `grep`/`rg`/`find` as a substitute — flag as a live instance of I-9/R4.2.3 below). Re-verify with `search`/`grep` before writing an implementation plan against it.
- **Done** — shipped in PR1/2/3/6, cited by commit.

---

## 1. Status Audit

### §2 Process & Lifecycle Management

| Req | Title | Status | Evidence |
|---|---|---|---|
| R2.1.1 | Writer lock file (`graph.lock`, flock + identity) | **Done** | PR1: `lockfile::LockInfo` (pid/role/build_hash/acquired_at) stamped on acquire; `WriteLock` delegates to it (`graph/store.rs`). |
| R2.1.2 | Stale-lock recovery (PID-liveness break, PID-reuse guard) | **Confirmed partial** | `lockfile::acquire()` only polls-and-waits-then-`Busy`; no PID-liveness check anywhere, no `sysinfo` dependency (explicitly still listed as a *needed* building block in §2.6). Flock's OS-level auto-release on process death covers the *safety* case (a truly-dead holder's lock releases itself); missing is the *precision* case — detecting a known-dead PID immediately instead of waiting the full timeout, and the PID-reuse guard. |
| R2.1.3 | MCP as write coordinator | **Excluded (PR5 territory)** | Target architecture, explicitly deferred by the parent doc itself. |
| R2.2.1 | Instance registry | **Confirmed absent** | No `~/.infigraph/instances/` directory exists; nothing in `main.rs`/`lib.rs` writes one. |
| R2.2.2 | Duplicate detection: peers vs. orphans | **Confirmed absent** | No startup scan of any registry exists (there's nothing to scan). |
| R2.2.3 | Orphan self-termination (idle grace after stdin close) | **Planned, not yet built** | Standalone plan already written: `docs/superpowers/plans/2026-07-21-mcp-idle-self-termination.md`. Confirmed live bug empirically this session: 2 real orphaned `infigraph-mcp` processes, PPID 1, alive 5+ hours, RSS unchanging — root cause is `main.rs::run()`'s unconditional `loop { sleep(3600) }` after stdin EOF when `--ui` is set. |
| R2.2.4 | `infigraph ps` / `infigraph kill` | **Confirmed absent** | No `ps.rs`/`kill.rs` in `crates/infigraph-cli/src/`; no matching MCP tool in the 91-tool `MCP_TOOL_NAMES` list. |
| R2.3.1 | Lock carries identity (`mcp.lock`) | **Confirmed absent — important nuance** | PR1 built the identity mechanism (`lockfile` module) and applied it to `graph.lock` (via `WriteLock`) and later `index.lock` (PR3). **`mcp.lock` — the specific lock R2.3 is about — was never migrated onto it.** `main.rs::acquire_instance_lock()` still uses raw `fs2::FileExt::try_lock_exclusive()` directly: no `LockInfo` stamping, no identity, no stale-detection. This is a real, previously-undiscovered gap distinct from R2.1.1/R3.1's `graph.lock`, found by direct code read this session while investigating the live orphan incident. |
| R2.3.2 / R2.3.2a | Takeover (build-hash handshake, not surrender) | **Confirmed absent** | No signal-incumbent-and-handover logic anywhere; a losing process today just becomes non-primary and keeps running its own worker forever (per PR1's build-hash field existing but unused for this purpose). |
| R2.3.3 | Wedged lock-holder degrades loudly | **Confirmed absent** | No health-check-based "holder unresponsive" detection. |
| R2.3.4 | Lock respawn | **Done by construction** | Trivially true today — a released flock is immediately race-able by the next starting process. Not a gap. |
| R2.3.5 | Lock-holder heartbeat | **Confirmed absent** | No periodic mtime/timestamp refresh on `mcp.lock`. |
| R2.3.6 | Sessions DB write lock | **Pre-existing plan (PR4)** | Covered by `write-safety-locks-design.md` PR4, not yet executed — not duplicated here. |
| R2.3.7 | Per-project `writer.lock` (I-4 fix) | **Confirmed absent** | The doc's own text states this is "currently unmitigated" and "not an extension of anything described above" — still true; no `writer.lock` mechanism exists distinct from `graph.lock`. **This is the doc's own stated live, unmitigated single-writer violation (I-4) — worth prioritizing.** |
| R2.3.8 | Real write coordination | **Excluded (PR5/Phase-3 territory)** | Explicitly deferred by the parent doc to Phase 3, depends on PR5's backend decision. |
| R2.4.1 | Watcher owned by writer; stops on lock loss | **Partial / mostly moot today** | "Only primary starts watchers" already true by construction (`is_primary` gates `auto_start_watch`). The *dynamic* "stops if lock is lost" half has nothing to trigger it — no preemption/takeover exists yet (depends on R2.3.2 landing first). |
| R2.4.2 | Watcher FD/watch-descriptor budget | **Inferred absent** | No evidence of an FD/watch-descriptor ceiling or degrade-to-polling path in the watcher code read this session. |
| R2.4.3 | Watcher leak regression suite (100x start/stop/restart) | **Inferred absent** | No such test file found among the watcher test suites read this session (`watcher_reindex.rs`, `watcher_concurrency.rs` cover functional behavior, not FD/thread-count leak regression). |
| R2.5.1 | Spawned-child hygiene (timeout, kill-on-drop process-group, stderr capture) | **Inferred absent** | No evidence of a shared child-process wrapper with these properties. |
| R2.5.2 | SCIP import contract validation (exists/non-empty/parses before replacing enrichment) | **Confirmed partial** | A regression test (`scip_enrich_exit_message_warns_on_nonzero_exit`) shows *exit-status* validation exists; full output-contract validation (file exists, non-empty, parses) before replacing prior enrichment data is unconfirmed. |

### §3 Data Integrity

| Req | Title | Status | Evidence |
|---|---|---|---|
| R3.1.1 | No destructive recovery without a corruption verdict | **Confirmed partial** | The PR#24/I-3 fix exists for `init()`'s specific path (this campaign's own tests reference `init_recovers_from_transient_open_failure_without_wiping`). Whether *every* other delete-graph code path was audited and generalized is unconfirmed — the doc explicitly calls this out as a generalization task, not a one-site fix. |
| R3.1.2 | Quarantine, don't delete (rename to `graph.corrupt.<ts>/`, bounded N=2) | **Confirmed absent** | No `.infigraph/graph.corrupt.*` pattern or quarantine-eviction logic found; PR2/PR3's wipe-path work was about lock preservation during `--full` wipe, not quarantine-instead-of-delete. |
| R3.2.1 | Pre-write snapshot (clonefile, N=2, rollback) | **Confirmed absent** | No `.infigraph/snapshots/` directory exists for this actively-indexed project. |
| R3.2.2 | `infigraph restore` | **Confirmed absent** | No such CLI module/subcommand. |
| R3.3.1 | Sidecar atomic writes (temp + rename) | **Confirmed absent** | Read `embed::save_embeddings` directly this session: `std::fs::File::create(path)` writes straight to the target path — no temp-file-then-`rename(2)` swap. A crash mid-write can leave a truncated `embeddings.bin`. |
| R3.3.2 | Sidecar headers (magic/version/generation/checksum) | **Confirmed absent** | `save_embeddings`'s on-disk format (read directly): `[count:u32][id_len:u32][id_bytes][dim:u32][f32×dim]` — no magic bytes, no format version, no checksum. |
| R3.3.3 | Graph generation ID | **Confirmed absent** | No generation counter field found anywhere in the graph store or sidecar formats read this session. |
| R3.3.4 | SCIP-staleness tracking (`ast_generation` vs `scip_generation`) | **Confirmed absent — explicitly deferred by PR6** | PR6's health-beacon spec deliberately deferred this exact beacon "pending R3.3.4 generation counters" — this audit confirms those counters still don't exist, so PR6's deferral was correctly scoped. |
| R3.4.1 | `infigraph verify` | **Confirmed absent** | No such CLI module/subcommand. |

### §4 Error Handling

| Req | Title | Status | Evidence |
|---|---|---|---|
| R4.1 | Structured error taxonomy (`Busy`/`Corrupt`/`Resource`/`Config`/`Transient`/`Internal`) | **Confirmed partial** | Only `lockfile::Busy` exists (PR1), a single-purpose struct, not the full 6-variant taxonomy at the `infigraph-core` boundary. Elsewhere, errors are ad-hoc `anyhow::Error` with no class. |
| R4.2.1 | Explicit, validated backend selection | **Inferred absent** | No evidence of a startup-time explicit-resolve-and-log step found this session; not directly verified either way. |
| R4.2.2 | Visible fallbacks (one-time warn + `get_stats`/`doctor` surfacing) | **Confirmed partial — PR6 closed 2 of 3** | PR6's health beacons surface Model2Vec→trigram fallback and HNSW-missing as tool-response warnings. Directory-watch→poll fallback visibility is not covered. No `get_stats`/`doctor` surfacing exists (PR6 only appends to tool responses, not a dedicated stats surface). |
| R4.2.3 | Enforcement hook fails open (+ test fixtures) | **Confirmed live, reproduced this session** | While researching this very doc, the PreToolUse hook blocked `Read` on `Cargo.toml` and blocked `grep`/`find` entirely (redirecting to Infigraph MCP tools that were disconnected at the time), with no fallback — the exact "over-broad indexable-file check... blocks legitimate Read of markdown/config files; blocks grep filtering non-code stdout" failure mode named in I-9, caught live. |
| R4.3.1 | Partial index-failure reporting/aggregation | **Inferred partial** | Some "indexed X, skipped Y" style summary messaging exists in group-index paths (seen in prior PR3 research); whether it's universal across every index code path, and never silent, is unconfirmed. |

### §5 Reliability / Uptime

| Req | Title | Status | Evidence |
|---|---|---|---|
| R5.1 | Crash-safe startup (reap registry → lock → verify sidecars → serve, bounded per-step) | **Confirmed absent** | Depends on R2.2.1 (registry) which doesn't exist; no bounded-time-budget-per-phase found in `run()`. |
| R5.2 | Self-watchdog (RSS/FD/thread ceilings, self-restart) | **Confirmed absent** | No RSS/FD/thread monitoring in the worker loop read this session. |
| R5.3 | Ground-truth health endpoint (`/healthz` + `health` MCP tool) | **Confirmed partial — PR6 implements the philosophy, not the endpoint** | PR6's `health.rs` explicitly cites and follows R5.3's ground-truth rule ("every status surface must reconstruct state from durable/OS-level facts... never rely solely on an in-memory registry") and its `watcher_running()` probe is a direct instance of that principle. But PR6 only appends inline footers to *existing* tool responses — the dedicated `/healthz` HTTP endpoint and a standalone `health` MCP tool, with the full signal set (generation ID, RSS/FD, last index result), don't exist. |
| R5.4 | Graceful shutdown (SIGTERM/stdin-close → stop watcher, flush, release lock, deregister, bounded 5s) | **Distinct from and broader than the R2.2.3 plan** | The standalone idle-self-termination fix makes an orphaned process *eventually* exit; it does not implement an *active* clean-shutdown sequence (stop watcher, flush in-flight writes, deregister from a registry that doesn't exist yet). Confirmed absent as a distinct requirement. |
| R5.5 | Crash containment scoped to the faulting DB (`current-op` breadcrumb) | **Confirmed absent** | No breadcrumb-file mechanism found; I-14 (global wipe+reindex storm on any crash) is the doc's own description of current behavior and nothing in PR1-6 touched it. |
| R5.6 | No black-holed requests (outstanding-JSON-RPC-id tracking + hard per-call deadline) | **Confirmed absent** | No such tracking in `handle_tools_call`/the stdin loop. This is I-13's actual fix and is notably close to the class of MCP-disconnect behavior investigated earlier this session (though that specific incident was traced to a harness-side transport hiccup, not a worker crash — this requirement is still open regardless). |

### §6 Observability

| Req | Title | Status | Evidence |
|---|---|---|---|
| R6.1 | Structured logging (`tracing`, JSON, per-project rotation) | **Inferred absent** | Every logging call site read this session (`mcp_log()`, `watch_log()`, scattered `eprintln!`) is ad-hoc plain-text, not `tracing`; no per-project `.infigraph/logs/` directory exists on disk. |
| R6.2 | Metrics (index duration, files/s, resolution rate, search latency, FD/RSS, lock contention) | **Confirmed partial** | `get_compression_stats` + `compression_metrics.jsonl` exist (session_context.rs) but cover only compression, not the broader metric set. PR6's slow-wait tracking (`lockfile::take_slow_waits`) is a form of "lock contention" signal, but surfaced as tool-response footers, not as `get_stats`/`/healthz` metrics. |
| R6.3 | Audit trail (`~/.infigraph/logs/audit.log`) | **Confirmed absent** | No `audit*` file found anywhere in the repo/home `.infigraph` tree. |
| R6.4 | `infigraph doctor` | **Confirmed absent** | No such CLI module/subcommand. |

### §7 Scalability & Resource Management

| Req | Title | Status | Evidence |
|---|---|---|---|
| R7.1 | Registry GC (`infigraph gc`, N=90-day eviction) | **Confirmed absent** | No such CLI module/subcommand. (This is the *project* registry, `registry.json` — see terminology note.) |
| R7.2 | Disk accounting & preflight | **Confirmed absent — currently urgent** | No preflight check found. Live evidence this session: `/System/Volumes/Data` is at **99% capacity, 14 GB free**, on the exact machine that's had multiple ENOSPC incidents this campaign (documented in the session ledger: mixing `CARGO_PROFILE_DEV_DEBUG` settings has previously spawned duplicate ~3 GB lbug cmake trees). This requirement would have caught that class of incident before it happened. |
| R7.3 | Bounded derived data (snapshots/quarantines/logs/sessions all capped) | **Confirmed partial** | Session purge (30-day) exists per the parent doc's own parenthetical. Snapshots/quarantines/logs aren't bounded because they don't exist yet (R3.2, R3.1.2, R6.1) — this requirement is mostly "enforce caps on features built by other PRs," not independently actionable yet. |
| R7.4 | Reindex-storm coalescing (>K files in debounce window → single incremental pass) | **Confirmed partial** | The watcher's existing 500ms debounce (PR3 work) provides *some* batching at a small timescale, but not the specific "large mass-change → fall back to one full incremental pass instead of N per-file updates" mechanism described. |

### §8 Deployment & Upgrade

| Req | Title | Status | Evidence |
|---|---|---|---|
| R8.1 | Schema/format versioning | **Confirmed absent** | Same evidence as R3.3.2 — no version/magic-byte headers on any sidecar or the graph store itself. Likely the *same implementation* as R3.3.2, not a separate mechanism — see PR grouping note below. |
| R8.2 | Install integrity (launch-verify, codesign re-apply, rollback) | **Inferred absent** | No evidence of this in `install.rs`/`install.sh` from what's been read this session; not exhaustively verified. |
| R8.3 | Hook/binary version coupling | **Inferred absent** | No version-embedding found in the enforcement hook script. |
| R8.4 | Pinned toolchain (`rust-toolchain.toml`) | **Confirmed absent** | `ls rust-toolchain.toml` → no such file. Trivial, P0, and the one item on this entire list cheap enough to just fix directly rather than plan. |
| R8.5 | Upgrade smoke test in CI | **Confirmed absent** | No such CI job found. |

---

## 2. Orphan & Instance Lifecycle Management (§2.2, full)

The user specifically asked for full treatment here, beyond the narrow self-termination slice already planned.

**R2.2.1 — Instance registry.** Every `infigraph-mcp` process writes `~/.infigraph/instances/<pid>.json` on startup (pid, start-time, project path, transport [stdio/http], host-agent hint if determinable) and removes it on clean shutdown. This is the durable substrate every other §2.2/§2.3 requirement reads from.

**R2.2.2 — Duplicate detection: peers vs. orphans.** On startup, scan the instance registry for existing entries claiming the same project path. For each:
- **Live peer** (entry's PID alive, and — where checkable — its parent/session still alive): coexist. Never kill a live peer's front-end or in-use worker without a handover (that's R2.3.2 territory).
- **Orphan** (PID dead, or live-but-parent-gone/stdin-closed): reap — `SIGTERM` → grace → `SIGKILL` — and remove its registry entry. This is what makes the *already-empirically-confirmed* 2 orphaned processes this session detectable and cleanable by the *next* process that starts, not just by the orphan noticing its own stdin closed (which is all the standalone R2.2.3 plan does).
- Also runs on a periodic timer (spec says every 10 min) so orphans get reaped even with no new process starting.

**R2.2.3 — Self-termination.** Already fully planned: `docs/superpowers/plans/2026-07-21-mcp-idle-self-termination.md`. That plan is independent of R2.2.1/R2.2.2 (it's pure self-observation: "did *my own* stdin close") and can execute first, standalone, with zero registry dependency. R2.2.2's periodic-scan reaping is what additionally catches an orphan *from the outside* if self-termination somehow doesn't fire (e.g., a build that predates this fix, or a process that's stuck in a way that prevents its own idle-check loop from running).

**R2.2.4 — `infigraph ps` / `infigraph kill`.** CLI commands reading the instance registry (R2.2.1) to list all live Infigraph processes (MCP servers, watchers, SCIP children) with project/uptime/RSS/FD count, and to terminate them. Turns "found 7 orphaned processes with `ps aux`" (I-5, and this session's own 2-orphan discovery) into a supported, discoverable workflow instead of manual forensic `ps`/`kill`.

**Relationship to §2.3 (mcp.lock lifecycle).** R2.3.1-2.3.5 (identity, takeover, wedged-detection, heartbeat) apply to a *different* lock (`mcp.lock`, the watcher/UI-arbitration singleton) than the instance registry, but they're the same *kind* of problem (who's alive, who's dead, how do we discriminate and act) and share the same building blocks (`lockfile` module, `sysinfo` for PID-liveness). Building them together in one PR (below) avoids solving "is this PID alive" twice.

---

## 3. PR Decomposition

Continuing the existing numbering (PR1-3, PR6 shipped/merge-ready; PR4 pre-existing/separately planned; PR5 excluded per scope above).

| PR | Title | Covers | Depends on | Size |
|----|-------|--------|-----------|------|
| **PR7** | Instance registry, orphan reaping, `mcp.lock` identity & takeover | R2.2.1, R2.2.2, R2.3.1 (for `mcp.lock`), R2.3.2/2.3.2a, R2.3.3, R2.3.5 | R2.2.3 (standalone plan, should land first — no hard blocking dependency, but shares the same conceptual ground) | **XL** — likely worth splitting into 7a (registry + reaping) / 7b (mcp.lock takeover) at plan time |
| **PR8** | `infigraph ps`/`kill`/`doctor`/`verify`/`gc` CLI tooling | R2.2.4, R6.4, R3.4.1, R7.1 | PR7 (registry), PR9 (generations, for `verify`/`doctor` to check) | M-L |
| **PR9** | Data integrity: quarantine, snapshots, sidecar atomicity & generations | R3.1.1 (generalize), R3.1.2, R3.2.1, R3.2.2, R3.3.1, R3.3.2, R3.3.3, R3.3.4, R8.1 (likely same mechanism as R3.3.2 — merge if so) | none blocking | **L** — arguably highest-priority remaining PR: R3.1 is the doc's literal "Prime Directive," and R3.3.1's non-atomic sidecar write is a confirmed, live gap |
| **PR10** | Error taxonomy, no-silent-fallbacks, partial-failure reporting | R4.1, R4.2.1, R4.2.2 (extend PR6's partial coverage), R4.2.3 (+ hook test fixtures), R4.3.1 | none blocking | M |
| **PR11** | Reliability: watchdog, dedicated health endpoint, crash containment, no-black-holed-requests, graceful shutdown | R5.1, R5.2, R5.3 (extend PR6), R5.4, R5.5, R5.6 | PR7 (registry, for R5.1's reap step), PR9 (generation ID, for R5.3's full signal set) | L |
| **PR12** | Observability: structured logging, metrics, audit trail | R6.1, R6.2 (extend/unify), R6.3 | PR9 (quarantine/restore events to audit), PR7 (instance-kill events to audit) | M |
| **PR13** | Scalability: disk preflight, bounded derived data, reindex-storm coalescing | R7.2 (**recommend prioritizing given current 99%-full disk**), R7.3, R7.4 | PR9 (things to bound), PR12 (logs to bound) | S-M |
| **PR14** | Deployment: install integrity, hook/binary coupling, CI upgrade smoke test | R8.2, R8.3, R8.5 | PR9 (R8.1, if not merged into PR9) | M |
| *(standalone, no PR#)* | Pin `rust-toolchain.toml` (R8.4) | R8.4 | none | **trivial** — do directly, doesn't need a plan |

**Suggested priority order**, weighing "P0 in the spec" against "confirmed live/urgent this session": PR9 (Prime Directive + confirmed non-atomic writes) and PR7 (confirmed live orphans, confirmed unmigrated `mcp.lock`) first; R8.4 toolchain pin same-day (trivial); PR13's R7.2 disk preflight bumped up given the current 14 GB free; then PR10/PR11/PR8/PR12/PR14 in roughly spec-priority order.

---

## 4. Open Questions For The User

- **Snapshot storage cost policy (R3.2.1):** hardlink-clonefile snapshots are cheap on APFS but not free at scale — is N=2 the right retention, and should snapshot/quarantine directories count against the R7.2 disk-preflight budget?
- **Supervised mode (§2.6):** the parent doc floats an opt-in launchd/systemd socket-activation mode as an alternative to hand-rolled orphan reaping, trading zero-config for OS-level singleton guarantees. Worth deciding whether that's ever in scope, or purely `lockfile`-protocol forever.
- **PR7 sizing:** flagged XL above — confirm whether to split into 7a/7b before writing its implementation plan, or accept a larger single plan.

---

## 5. Newly Identified Gaps (2026-07-22)

Found while investigating a live symptom (stray tempdir entries polluting `list_projects` output across unrelated projects, since the project registry is machine-global). Both confirmed by reading the real source this session, not inferred.

### R-NEW.1 — Project registry (`registry.json`) has no test-isolation seam

**Confirmed absent.** `registry_path()`/`registry_lock_path()` (`crates/infigraph-core/src/multi/mod.rs:816-830`) resolve unconditionally to `$HOME/.infigraph/registry.json` — no env-var override, no test-mode gate. `tool_index_project` (`crates/infigraph-mcp/src/tools/index.rs:41-49`, `140-148`) unconditionally calls `registry.register_repo(...)` on that real file on both its CLI-subprocess and in-process-fallback code paths. This repo's own integration tests under `crates/infigraph-mcp/tests/` create `tempfile::tempdir()` fixtures and exercise `tool_index_project` in-process, so every full-suite run silently registers throwaway tempdir paths into the *real* `~/.infigraph/registry.json`; the tempdir is deleted at test teardown but the registry entry never is. Confirmed as the exact cause of a live incident: 48 leaked entries observed via `list_projects` from an unrelated project (sittir), pruned down to 1 real entry on 2026-07-22 (mechanical fix applied directly, not planned — see session log).

Note there's already a precedent for exactly this class of test-isolation problem in this codebase (`crates/infigraph-mcp/src/session_context.rs`'s `TestEnv`, which isolates `dedup_state.json` and CWD-walked config from the real repo) — but that mechanism is CWD-based (`set_current_dir` to a tempdir), and `registry_path()` is HOME-based, so `TestEnv`'s existing trick doesn't cover it. A fix needs either an `INFIGRAPH_HOME`/`INFIGRAPH_REGISTRY_PATH`-style env override checked before the `HOME` fallback (set by a `TestEnv`-equivalent guard in `crates/infigraph-core`'s own test harness), or tests switching to a non-global `Registry` construction path instead of the process-wide singleton file.

**Fits under PR4's already-shipped scope** (project registry write-safety) as a follow-up hardening item, or could ride with PR9/PR13 (bounding derived/global state). Small, self-contained fix.

### R-NEW.2 — stdio-mode tool calls are not bound to the launching repo/cwd

**Confirmed absent.** Every tool handler resolves its project root purely from the per-call JSON `path` argument (`open_prism`/`open_prism_read_only`, `crates/infigraph-mcp/src/tools/helpers.rs:60-70`, via `resolve_project_path`) — there is no check anywhere against the MCP process's own launch cwd. `acquire_instance_lock` (`crates/infigraph-mcp/src/main.rs:203-231`) uses one global `$HOME/.infigraph/mcp.lock`, not a per-repo lock, and only arbitrates watcher-primary status — it has no bearing on tool-call scope. `main()`'s arg parsing (`main.rs:9-55`) never captures or stores a "this process = this repo" fact; the launch cwd is implicit and unenforced. Net effect: in stdio mode (one process per client session in practice, per observed `ps` trees this session), any tool call — including destructive ones like `delete_project` — can target an arbitrary path belonging to a completely different, unrelated project, with nothing to stop it.

**Complication:** the multi-repo group feature (`group_add`/`group_index`/`group_build`, cross-service tools) deliberately requires one MCP instance to touch several distinct repo paths at once, so a blanket "reject any path outside launch cwd" rule would break a real, intentional capability. A viable design needs to distinguish the launch-cwd "home" repo from repos explicitly registered into a group that the home repo belongs to. HTTP/UI mode (port 9749) is arguably a separate story — already multi-tenant by design, may not want the same restriction.

**Not yet scoped to a PR** — closest fit is PR7 (instance registry / `mcp.lock` identity), since it already touches "what does this MCP process represent" as a concept, but the actual fix (per-call path allowlisting) is more of a tool-dispatch-layer change than an instance-registry change. Needs a short design pass before it gets its own plan — flagging as a fifth **Open Question For The User**: should stdio-mode tool calls be scoped to the launch cwd (+ its group members), and if so, is that worth its own PR or a late addition to PR7?

---

## 6. Newly Identified Gaps (2026-07-24)

Found during a live incident this session: two independent processes (`infigraph index` and `infigraph watch`, both auto-started against the main tree) got stuck for 1+ hour each at ~99% CPU with a ~8.4TB bogus VSZ, both holding `index.lock`/`graph.lock` the entire time, both confirmed via `lsof` to be repeatedly reopening the same `.infigraph/graph` file. Root cause: the graph DB was corrupted by an unrelated disk-full (`ENOSPC`) event earlier in the session. Manually removing `.infigraph/graph`/`graph.wal` and rerunning `infigraph index --full` fixed it in 10s — proving the fix is trivial once you know to do it, but nothing in the system detects or recovers from this on its own. Related work in flight: `docs/superpowers/plans/2026-07-24-watcher-daemon-split.md` (toggle-gated, targeting `intuit/infigraph` directly, independent of the PR numbering below).

### R-NEW.3 — MCP's `index_project` subprocess call has no timeout, no monitoring, no recovery

**Confirmed absent.** `tool_index_project` (`crates/infigraph-mcp/src/tools/index.rs:28-30`) spawns the CLI (`prefers CLI subprocess, falls back to in-process` per `docs/CODE-PARSING.md`) via `Command::output()` — a synchronous, **unbounded** blocking call. If the child hangs (as it did this session), the MCP worker thread blocks forever; the only thing that "gave up" was the outer tool-calling harness's own generic 1800s idle-timeout, which does **not** kill the underlying child process — confirmed empirically, since the stuck process was found still running an hour later, well past that timeout.

**Proposed fix** (design agreed with user, not yet planned): replace the blocking `.output()` with a spawn + bounded-wait loop. On timeout: kill the child, retry once plain (handles transient slowness without destroying a large repo's legitimate long-running index). If the retry **also** times out — especially quickly, which is the actual signature observed this session (near-instant hang on open, not a slow grind) — that's evidence of corruption, not slowness; escalate to `--full` automatically for this specific case (unlike the watcher case below, `index_project`'s whole job already is to (re)build the graph, so self-healing via `--full` here doesn't cross the same line the CLAUDE.md wipe-on-failure caution warns about for other subsystems). Timeout value: anchor to something empirical rather than guessed — this repo's own reindex takes ~10s cold, so a few minutes comfortably covers real use while still catching a hang far short of the hour-plus we just lived through.

### R-NEW.4 — `index_project`'s MCP schema doesn't expose the `full` parameter the handler already supports

**Confirmed absent.** `tool_index_project` reads `args.get("full")` (`tools/index.rs:19`), but its schema registration (`crates/infigraph-mcp/src/lib.rs:378-379`) passes an empty extra-properties object (`json!({})`) with only `path` required — `full` is not in the advertised schema anywhere. Net effect: no MCP client can ever populate `full: true` through the documented tool-calling interface; only direct CLI use (`infigraph index --full`) can reach it. This matters directly for R-NEW.3's self-healing design (harmless there, since the escalation happens inside the Rust handler, not via the exposed schema) but is also its own small, real gap: any MCP-based agent trying to explicitly request a full reindex, or trying to act on a human-readable recovery message from elsewhere (e.g. R-NEW.5 below) that says "run a full reindex," has no tool call available to do it. Trivial fix: add `full` as an optional boolean to the schema. Bundle with R-NEW.3 (same file, same PR).

### R-NEW.5 — The watcher daemon has no startup-readiness check; a stuck-on-open daemon runs forever undetected

**Confirmed absent, and a different code path than R-NEW.3.** The watcher daemon is spawned fire-and-forget (`cmd.spawn()`, not `.output()`/`.wait()` — `infigraph_core::watch::daemon::spawn_daemon`, new in this session's Task 1 of the watcher-daemon-split plan; the pre-existing CLI equivalent `spawn_watcher` had the same fire-and-forget shape). Nothing blocks waiting on it, so R-NEW.3's "timeout the blocking call" fix doesn't apply — there is no blocking call. But the failure mode is otherwise identical: this session's stuck `infigraph watch` process hung at *startup*, opening the same corrupted graph file, before ever reaching its long-running watch loop, and nothing noticed for over an hour.

**Proposed fix** (design agreed with user, not yet planned): a readiness probe run by whatever spawns the daemon (`ensure_daemon_running` or its caller) — after spawn, poll for a bounded window (a few seconds) for evidence the daemon reached its steady-state loop (e.g. `watch.log` receiving its first write, or `watch.lock`'s holder PID matching the new process). If not ready in time: kill and retry once. If the retry **also** fails to become ready in the same short bound: unlike R-NEW.3, do **not** auto-trigger `infigraph index --full` here — a watch daemon's job is to watch an already-good graph, not rebuild one, and CLAUDE.md's existing caution ("any wipe-and-rebuild-on-open-failure logic must distinguish transient errors from actual corruption") argues for surfacing a clear, actionable error (kill the daemon, log/return "watcher failed to start twice, likely a corrupted/inaccessible graph DB — run `infigraph index --full`") over silently auto-healing. Once R-NEW.4 lands, that message becomes actionable by an MCP-based caller too, not just a human with CLI access.

**Not yet root-caused, explicitly deferred, not a blocker for R-NEW.3/R-NEW.5:** *why* `GraphStore::open`'s existing file-size sanity check (referenced in this repo's own `CLAUDE.md`: "a truncated/corrupt file can make Kuzu's parser read a bogus size field... Check file size against a sane minimum first") didn't catch this specific corruption (a plausible-sized 36MB file, not a near-zero truncated one). R-NEW.3 and R-NEW.5 are symptom-level, generic safety nets that provide value regardless of this root cause ever getting fixed — user explicitly confirmed sequencing this after, not before, per this session's discussion.

**Fit into the PR scheme above:** R-NEW.3/R-NEW.4 are a small, self-contained pair (one file, `tools/index.rs` + `lib.rs`'s schema list) — closest fit is standalone or riding with **PR11** (reliability: watchdog, crash containment — R5.1-R5.6 territory, same conceptual family). R-NEW.5 depends on the watcher-daemon-split's `ensure_daemon_running` (already in flight, separate plan/branch) landing first, since it extends that exact function — sequence after `docs/superpowers/plans/2026-07-24-watcher-daemon-split.md` ships, not before.
