# Write-Safety Locks — Design

**Date:** 2026-07-20
**Status:** Approved (user-reviewed design; spec pending review)
**Parent:** `docs/DESIGN-hardening.md` — implements R2.3.6/R2.3.7 (write safety), draws on R2.3.1 (lock identity), R3.3.1 (sidecar atomicity), R4.1 (error taxonomy direction)
**Delivery:** four implementation PRs + one spec-only PR, each independently mergeable against fork `main` (v3.2.1 base), opened as separate upstream PRs

## Problem

Infigraph's single-writer invariant for per-project Kuzu graphs is only partially enforced. A cross-process advisory `WriteLock` (flock on `.infigraph/graph.lock`, commit 57a19ca) exists and is wired at the low-level write entry points (`kuzu_backend`, `store_write`, `store_parquet`, `store.rs` remove paths, `scip/mod.rs`, `resolve/calls.rs`, `multi/combined.rs`), so individual write calls from concurrent processes serialize. The residual gaps, all observed or derived from live incidents in `DESIGN-hardening.md` (I-3, I-4, I-12, I-13):

1. **Infinite blocking, zero diagnostics.** `WriteLock::acquire` blocks forever. The lock file is empty — no holder identity. A wedged holder silently hangs every other process (the I-13 black-hole pattern, at the lock layer).
2. **Unlocked write paths.** `GraphStore::open()` runs `init_schema` + migrations without the lock. Destructive recovery paths (`recovery::wipe_code_and_docs`, `Infigraph::init()`'s recreate) touch disk without it. "Caller must hold WriteLock" is enforced by comments only.
3. **Lock-per-call ≠ operation scope.** Two concurrent index runs (e.g. `index_project` from two agent sessions, or an explicit index racing watcher batches) interleave their individually-locked write batches → logically inconsistent graph. No corruption, but silent lost updates.
4. **Shared-state races (sessions + registry).** `save_session` performs three writes with no locking: (a) session JSON read-modify-write (same-day merge), (b) narrative markdown append, (c) `sessions/embeddings.bin` full load-modify-save — the last is a real observed-workload lost-update risk (two sessions saving concurrently is routine on this machine) and a torn-write risk (non-atomic full-file rewrite of a length-prefixed binary). `consolidate_memory` and session purge do the same load-modify-save on the same file. Separately, `~/.infigraph/registry.json` is an unlocked read-modify-write from any process (`register_repo` on every index run; group commands), so concurrent index runs on different projects can silently drop each other's registry entries — a plausible cause of the unexplained registry shrinkage observed on 2026-07-20.

Out of scope here, specced separately as PR 5: restructuring which processes create workers and whether workers become per-project.

## Design

### Architecture

Two lock files per project, one shared plumbing module:

| Lock | Granularity | Held by | Purpose |
|------|------------|---------|---------|
| `.infigraph/graph.lock` | Per write call (existing) | Any graph write | Corruption floor — no two processes mutate Kuzu concurrently |
| `.infigraph/index.lock` | Per logical operation (new) | Index runs, SCIP import, watcher batches | Consistency — no two index operations interleave; enables coalescing |
| `.infigraph/sessions/sessions.lock` | Per save/consolidate/purge (new) | Session store mutations | Lost-update/torn-write protection for JSON + embeddings |

**`lockfile` module** (`crates/infigraph-core/src/lockfile.rs`) — the single owner of lock mechanics, used by all three (and by any future lock, e.g. a redesigned `mcp.lock` in PR 5):

- **Identity payload.** On acquisition, write JSON into the lock file: `{ pid, pid_start_time, build_hash, role, acquired_at }`. `pid_start_time` (from `sysinfo` or `/proc`-equivalent) guards against PID reuse. `build_hash` is the compile-time git SHA + dirty flag injected via `build.rs` (fallback: hash of the binary).
- **Bounded wait.** `acquire(path, role, timeout)` polls `try_lock_exclusive` with backoff up to `timeout` (default 30 s; index-run callers may pass longer). On expiry, return `Busy` (below). No infinite block anywhere.
- **Stale-break.** If the flock is *free* but a payload exists → previous holder died without cleanup; overwrite. If the flock is *held*: read payload; if payload PID is dead, or alive with mismatched start-time (PID reuse) → the flock holder must be a different (live) process that inherited the file — treat as held. flock held + no readable payload (old binary, or corrupted payload) → **unknown holder: never break, only bounded-wait then `Busy`**. Conservative rule: a held flock we cannot identify is never broken.
- **`Busy` error.** `Busy { lock_path, holder_pid: Option<u32>, holder_role: Option<String>, held_for: Option<Duration> }`, with a `Display` rendering suitable for MCP tool output: `graph is being written by infigraph watch (PID 95723), held 4s — waited 30s, giving up`. Structured so the hardening spec's future error taxonomy (R4.1) can wrap it without rework.

### PR 1 — `lockfile` module + adopt in `WriteLock`

- New `lockfile.rs` as above, unit + cross-process tests (following the existing `write_lock*.rs` child-process test pattern).
- `WriteLock::acquire`/`try_acquire` delegate to it. Public behavior change: `write_lock()` gains a bounded wait (default 30 s) and returns the `Busy` error instead of blocking forever. `try_write_lock()` unchanged semantics, now stamps identity.
- Compatibility: pure flock semantics are unchanged, so an old binary and a new binary interoperate; the old binary's bare lock reads as "unknown holder" (bounded-wait only, never broken). Lock files remain transient coordination state — no schema/format versioning required.

### PR 2 — Close unlocked write paths + compiler enforcement

- `GraphStore::open()` acquires the write lock around `init_schema`/migrations (schema DDL is a write).
- Destructive paths acquire it before touching disk: `recovery::wipe_code_and_docs`, `Infigraph::init()`'s recreate path.
- Replace every "Caller must hold WriteLock" comment with a witness parameter: the guarded methods take `&WriteLock`, making the invariant compile-time-checked. Mechanical refactor of `store_bulk`, `store_write`, `store_parquet`, `resolve/calls`, `resolve/inherits`, `remove_file_conn` and their callers.
- Audit for any remaining write path that constructs a Kuzu connection and mutates without the lock; wire what's found.
- **Surface WAL-replay failures.** `GraphStore::open_read_only` currently sets `throw_on_wal_replay_failure(false)`, silently serving a torn base image after a failed recovery — this is how the 2026-07-19 corruption stayed latent until it segfaulted a day later. Replay failure becomes a surfaced error carrying a corruption verdict, so callers route into quarantine (per `DESIGN-hardening.md` R3.1) instead of scanning damaged pages. Where tolerating replay failure is genuinely intended (best-effort diagnostics), it must be explicit at the call site and logged.

### PR 3 — Operation lock + index coalescing

- `index.lock` (via `lockfile`) held across a full logical operation: `infigraph index` (CLI), `tool_index_project` (MCP), SCIP import, and each watcher batch flush.
- **Coalescing (user-selected semantics):** an index request that finds `index.lock` held by a live holder returns immediately with success-with-note: `index already in progress (infigraph index, PID X, started 90s ago) — skipped`. It does not queue, wait, or re-run. Small writes (watcher batches) use a normal bounded wait since batches are short.
- The fine-grained `graph.lock` remains beneath it unchanged — `index.lock` is about run-level consistency, `graph.lock` about call-level corruption.

### PR 4 — Shared-state write safety: sessions + registry

- `sessions.lock` (via `lockfile`) held across the whole `save_session` critical section: JSON read-merge-write + narrative append + `embeddings.bin` load-modify-save. Same lock in `consolidate_memory` and session purge.
- `embed::save_embeddings` (as used for the sessions store) becomes atomic: write to a temp file in the same directory, `rename(2)` over the target — the R3.3.1 pattern.
- Narrative append stays an append; it's inside the lock anyway.
- **`~/.infigraph/registry.json`**: today it's an unlocked read-modify-write from any process (`register_repo` runs on every `index_project`; `group_add`/`group_sync` edit it too), so two concurrent index runs on *different* projects can silently drop each other's registry entries. `Registry::save` (and the load-modify-save callers) wrap in `~/.infigraph/registry.lock` (via `lockfile`) and write atomically via temp + rename.

### Groups

No group-specific lock design is needed: a group's combined graph directory (`~/.infigraph/groups/<name>/.infigraph/`) is project-shaped, and all lock paths derive from the graph's own location, so `graph.lock` and `index.lock` apply to combined graphs exactly as to projects (the combined store already acquires the existing `WriteLock` — `multi/combined.rs`). `group_index`/`group_build` take the *group directory's* `index.lock` for the combined-build phase and each *member's* `index.lock` while indexing that member — so member-level coalescing works unchanged, and a group build never interleaves with a direct index run on the same member. The crash-recovery reindex path (which iterates registered projects *and* group dirs) participates in the same locks once PR 2 wires it.

### PR 5 — Worker architecture + Cozo backend strategy (spec-only, no implementation)

An RFC-style design document (lands in `docs/`), fully specifying the worker-layer restructuring **and the storage-backend decision, evaluated specifically as lbug-vs-CozoDB** — coupled decisions, since the backend's concurrency model determines how much process architecture is needed at all. Required content, with constraints already established:

**Backend (Cozo) evaluation:**
- Leverage the existing in-tree investment: `graph/cozo_store.rs`, `cozo_vs_kuzu.rs`, `kuzu_to_cozo.rs`, `golden_cozo_export.rs` — data migration and comparison are already prototyped.
- **Crash-torture acceptance harness** (the deciding test, derived from the 2026-07-19/20 incident): `kill -9` mid-write in a loop, then verify the reopened DB either recovers or *fails with an error* — never SIGSEGVs (lbug's observed behavior on the quarantined graphs; those files are the regression fixtures).
- Cross-process concurrency per Cozo storage engine: sqlite engine (inherits SQLite's file locking — potentially eliminates most per-project locking machinery) vs rocksdb (single-process — would *require* the per-project-worker architecture). This coupling is the reason backend and worker architecture are one RFC.
- Native HNSW vector index evaluation: can it absorb `embeddings.bin`/`hnsw.bin`, eliminating the sidecar-consistency problem class (R3.3) entirely?
- Query migration cost: all tool Cypher → CozoScript (Datalog), including the user-facing `query_graph` tool's exposed query language (breaking change vs. translation shim — must be decided, not hand-waved).
- Maintenance-risk comparison stated honestly: both lbug and Cozo are lightly-maintained; the differentiator to assess is ownability (small pure-Rust codebase vs. large C++ core) plus build/packaging wins (no cmake, no C++20 toolchain floor, Windows cross-compile unblocked, multi-GB build-artifact reduction).

**Worker architecture:**

- **Fixed constraint:** MCP stdio front-ends are per-session by transport design and cannot be per-project; a session works across multiple projects (per-call `path` routing). Scoping therefore happens at the worker layer or below.
- **Candidate architectures to be evaluated:** (a) per-project worker processes with front-ends routing calls by resolved project path — single-writer by construction, locks become a backstop; (b) status quo per-session workers relying on PRs 1–4's locks; (c) hybrid: per-session workers for reads, single per-project write-delegate.
- **Must specify:** worker spawn ownership and lifecycle (who creates, who reaps — ties into the instance registry, R2.2), takeover/upgrade using PR 1's identity format, the I-15 prerequisite (front-end must survive and respawn on any worker exit, not just SIGSEGV, before any deliberate worker replacement is safe), idle shutdown (sccache-style) to prevent orphan accumulation (I-5), and interaction with the global `mcp.lock` watcher/UI role (R2.3.1–R2.3.5).
- **Prior art to draw on:** Watchman (socket + version handshake + graceful `shutdown-server`), Gradle daemon (version-keyed coexistence), sccache (idle timeout), launchd/systemd socket activation as an opt-in supervised mode (spec §2.6).

### PR 6 — Health beacons on MCP tool responses

Generalize the existing "✓ Auto-started watcher" footer pattern: tool responses append a one-line ⚠ footer **only when a degraded condition exists** — silent when healthy, so no steady-state token cost. Conditions (each independently detectable from durable state, per the ground-truth rule in `DESIGN-hardening.md` R5.3):

- worker restarted (crash or otherwise) since this client's previous call — directly de-cloaks the I-13 "crash was invisible" failure mode
- SCIP enrichment stale relative to AST generation (R3.3.4's `ast_generation` vs `scip_generation` gap)
- embedder running on trigram fallback / HNSW unavailable
- a lock wait exceeded threshold while serving this call (`Busy` retries that eventually succeeded)
- watchers inactive for this project (e.g. this process is non-primary and no primary is running)

Implementation: a `health_footer(project)` helper called at the end of tool dispatch; conditions computed from lock files, generation counters, and a worker-start timestamp — no new state stores. Depends on PR 1 (lock identity) and pairs with, but does not require, R3.3.4 generation tracking (conditions ship incrementally as their signals exist).

## Error Handling

One error shape (`Busy`, defined in `lockfile`) for all three locks. Callers translate to their surface: MCP tools return the rendered message; the CLI prints it and exits non-zero; the watcher logs and retries next batch. Unreadable/corrupt lock payloads are treated as bare locks (unknown holder) — never an error by themselves. Stale-break events log one line (future audit-log candidates, R6.3).

## Testing

TDD throughout; extend the existing `write_lock*.rs` suites (which already spawn real child processes for cross-process assertions). New coverage minimum:

- bounded-wait expiry returns `Busy` with correct holder identity
- stale-break on dead PID; no break on PID-reuse (start-time mismatch); no break on unknown holder
- old-format empty lock file: held → bounded-wait; free → adopted
- two concurrent index runs: exactly one runs, the other returns the coalescing note
- watcher batch vs. index run: batch waits, never interleaves
- concurrent `save_session` × 2: both sessions' embeddings present afterward (lost-update regression)
- `save_embeddings` atomicity: reader never observes a torn file (kill-during-write test)
- witness-parameter enforcement: compile-time only; existing suites still pass

## Compatibility & Migration

- No data-format changes; lock files are transient. Mixed old/new binary fleets degrade to bounded-wait against unknown holders (the exact I-12 scenario, handled conservatively).
- `graph.lock` file location and flock semantics unchanged — existing external tooling (e.g. `lsof`-based diagnostics) keeps working.
- Each PR leaves the tree green independently; later PRs depend on PR 1's module but not on each other.

## Decomposition & Delivery

| PR | Branch (fork) | Depends on | Size |
|----|--------------|-----------|------|
| 1. `lockfile` module + WriteLock adoption | `feat/lockfile-identity` | — | M |
| 2. Close unlocked paths + witness param | `feat/write-lock-enforcement` | PR 1 | M (mechanical) |
| 3. `index.lock` + coalescing | `feat/index-operation-lock` | PR 1 | M |
| 4. Sessions + registry shared-state safety | `feat/shared-state-write-safety` | PR 1 | S–M |
| 5. Worker-architecture + Cozo backend RFC | `docs/worker-architecture-rfc` | PRs 1–4 (references) | Spec only |
| 6. Health beacons on tool responses | `feat/health-beacons` | PR 1 (lock identity); R3.3.4 optional | S |

Each branches off fork `main` (v3.2.1 base) and is opened as its own upstream PR. PRs 2–4 rebase onto PR 1 once it merges (or stack on its branch if opened before).
