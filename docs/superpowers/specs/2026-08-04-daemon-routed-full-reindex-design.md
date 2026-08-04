# Daemon-Routed Full Reindex — Design

**Date:** 2026-08-04
**Status:** Draft
**Closes:** [GitHub issue #50](https://github.com/pradeepmouli/infigraph/issues/50) (real fix — the mitigation is already merged and closed)
**Related:** [R-NEW.4](docs/superpowers/specs/2026-07-21-remaining-hardening-design.md#r-new4--index_projects-mcp-schema-doesnt-expose-the-full-parameter-the-handler-already-supports) (`index_project`'s MCP schema doesn't expose `full`) — closed as a side effect of this work
**Builds on:** `docs/superpowers/plans/2026-08-03-daemon-index-work-queue.md` (`IndexWorkQueue`, `execute_drain`, `index.lock` serialization, the `Arc<Infigraph>` upgrade) — all shipped, `feat/hardening` at `61766cc`

## Background

`infigraph index --full` currently refuses outright under `INFIGRAPH_BACKEND=daemon` (`crates/infigraph-cli/src/index.rs`, the issue #50 mitigation): wiping `.infigraph/graph` is a direct, local filesystem operation with no routing through the daemon, and the daemon holds a persistent, open Kùzu `Database` connection on that exact directory for its whole lifetime. Deleting the files out from under that handle is unsafe — verified against both upstream Kùzu's and the `lbug` fork's own concurrency docs: safe concurrent access is guaranteed only for multiple connections from the *same* `Database` object, within one process, never across two.

The fix has to make the daemon itself do the wipe-and-rebuild, since it's the only process safely allowed to touch that directory.

## Architecture

**Build-fresh-then-swap**, not wipe-in-place. The daemon builds an entirely new database at a separate path, `.infigraph/graph.rebuilding/`, running a full index pass against it while the live `.infigraph/graph` is completely untouched. On successful completion, it atomically swaps the two directories in and moves the old one aside (quarantine-style, not deleted) rather than mutating the live graph directly at any point.

This was chosen over two alternatives considered during design:

1. **Wipe-in-place** (the issue's original sketch: poison the daemon's own connection, delete `.infigraph/graph`, reopen fresh, reindex) — works, but leaves a real window (the whole rebuild duration) where the graph doesn't exist at all, and every read attempted in that window needs some kind of "reindex in progress" signal.
2. **Route through `IndexWorkQueue`** as a fifth coalesced variant — rejected early: the queue's coalescing model assumes the same backend handle stays valid across a drain, which a full wipe-and-rebuild violates by construction. A full reindex is categorically different from "more incremental work arrived" — it invalidates the very handle the queue's design depends on staying live.

Build-fresh-then-swap avoids both problems. Since `DaemonKuzuBackend`'s reads already reopen a fresh connection per call (an existing design choice, unrelated to this work, made to close a staleness gap), reads transparently keep hitting the live, valid old graph right up until the swap — no "reindex in progress" signal needed on the read path at all. `rename(2)` on the same filesystem is atomic at the OS level (the same guarantee `daemon_protocol::write_atomic` already relies on elsewhere in this codebase for `.result` files), so there's no half-swapped state a concurrent read could observe: a read either sees the fully-old graph or the fully-new one, never a mix. An already-open read connection that was mid-query when the rename happens is unaffected by POSIX rename semantics — the rename only changes what a *future* path lookup resolves to; an already-open file descriptor keeps referring to the same underlying data.

**Disk cost:** the rebuild needs roughly 2x disk space for the graph's own on-disk size while old and new coexist. Explicitly accepted as a non-issue — `.infigraph/graph`'s own size is small in practice regardless of codebase size (this was confirmed against this project's own real numbers, not assumed); the disk pressure this whole session repeatedly hit on the dev machine came from build caches and `target/` directories, unrelated to graph size. No disk-preflight check is in scope for this plan.

## Components & data flow

### 1. `WriteRequest::FullReindex` (new, no payload)

Added to the `WriteRequest` enum (`crates/infigraph-core/src/daemon_protocol.rs`) alongside the other 13 variants. No fields — it always means "rebuild everything," unlike `Index`'s optional `paths`.

### 2. CLI (`cmd_index`, `crates/infigraph-cli/src/index.rs`)

The existing daemon-mode `--full` refusal branch is replaced: submit `WriteRequest::FullReindex` (same `submit_write_request`/`submit_write_request_named` pattern every other daemon-routed ad-hoc request already uses) and wait for the reply, instead of erroring out.

### 3. MCP schema (`tool_index_project`, `crates/infigraph-mcp/src/tools/index.rs` + `lib.rs`'s tool registration)

`full` is added to `index_project`'s advertised MCP schema (currently an empty extra-properties object even though the handler already reads `args.get("full")`) so MCP clients can request a full reindex too. This is R-NEW.4, closed as a natural side effect of touching this exact code path — not a separate follow-up.

### 4. Daemon-side handler (new, in `crates/infigraph-core/src/watch/mod.rs`'s request-serving path)

`WriteRequest::FullReindex` does **not** go through `route_or_serve_request`'s queue-enqueue arms — it gets its own dedicated handling, structurally alongside (not inside) the existing `serve_request_locked` fallback, since it needs different post-lock behavior than a plain `serve_one_request` call.

Sequence, once dispatched:

1. Acquire `index.lock` via `begin_index_op(root, "infigraph daemon (full reindex)", Duration::from_secs(30))` — the distinctive role string is for observability (logs, `AlreadyRunning` skip-notes) only; nothing downstream branches on it. Acquiring this lock is what gets "wait for any in-progress drain to finish first" for free — it's the same lock every other write already serializes on.
2. Once `Acquired`: drain the `IndexWorkQueue` (reusing `IndexWorkQueue::drain()` as-is — no new queue API) and discard its `items`/`removals`/`whole_project`. Every retained `Waiter`, however, gets an explicit reply — a `WriteResult::Err` (or a dedicated "superseded by full reindex" variant, implementer's call at plan time) — never silence. This is the "residue" rule: anything only *queued*, not yet executing, is genuinely moot (the full reindex will re-scan every file from disk regardless of what was queued), but the *clients waiting on it* still get answered.
3. Build a fresh `Infigraph` at `.infigraph/graph.rebuilding/` (a new path, sibling to the live `.infigraph/graph`) and run a full index pass against it — plain `scan_changed_files` against the empty fresh graph naturally treats every file as changed (its `get_file_hashes()` call returns an empty map), so no special "force" mode is needed.
4. On success: poison the daemon's currently-held `Arc<Infigraph>` (reuses the existing `poison_watch_db` mechanism, same one Task 3/4's error-recovery paths already use), then atomically swap: rename `.infigraph/graph` → `.infigraph/graph.quarantine.<unix-ts>/` (feeding the same naming convention already sketched for R3.1.2's quarantine mechanism in the hardening backlog, rather than deleting outright), then rename `.infigraph/graph.rebuilding` → `.infigraph/graph`. The next `watch_db()` call reopens fresh against the new path, same as any other post-poison reopen.
5. Reply to `FullReindex`'s own waiter with the resulting `IndexResult`.
6. Release `index.lock` (guard drop, same as every other lock-guarded operation in this codebase).

### 5. Failure handling mid-rebuild

If the full index pass against `.infigraph/graph.rebuilding/` fails partway through (extraction error, disk error, etc.): the live `.infigraph/graph` was never touched, so the daemon simply deletes the incomplete `.rebuilding` directory and replies with the error — no swap happens, no poisoning of the live handle happens, the daemon keeps serving the old (still fully valid) graph exactly as before the attempt. This is a real advantage of build-fresh-then-swap over wipe-in-place: a failed rebuild leaves the live graph completely unharmed, rather than the daemon being left with no graph at all.

If the swap itself fails partway through (e.g. the first rename succeeds but the second doesn't — genuinely rare, since both renames are on the same filesystem and adjacent in time, but not physically impossible): the daemon is left with neither a `.infigraph/graph` nor is fully certain of state. This needs to be handled loudly rather than silently — surface a clear, actionable error naming both possible directory names (`graph.quarantine.<ts>/`, `graph.rebuilding/`) so a human (or a future `infigraph doctor`/`verify`, per the broader hardening backlog) can recover, rather than attempting automatic recovery here.

## Testing

A real end-to-end test (extending `crates/infigraph-core/tests/daemon_kuzu_e2e.rs`) that:
- Seeds a project, indexes it via a real spawned daemon, confirms real content exists.
- Submits `WriteRequest::FullReindex` and confirms the reply carries a real `IndexResult` and the graph is genuinely rebuilt (not just "didn't error").
- Concurrently submits an ad-hoc `Index` request for a single file *during* the full reindex and confirms its waiter gets an explicit "superseded" reply rather than hanging or timing out.
- Confirms a read (e.g. `search` or `get_stats`) issued during the rebuild window sees the still-valid *old* graph, not an error or empty result — proving the "reads are unaffected" property is real, not just assumed.
- Confirms the old graph directory is quarantined (renamed aside, matching the `graph.quarantine.<ts>/` pattern), not deleted.
- A failure-injection test (extraction failure mid-rebuild) confirming the live graph survives untouched and the `.rebuilding` directory is cleaned up.

## Open questions for the implementer (not blocking design approval)

- Exact `WriteResult` shape for "superseded by full reindex" — a new variant, or reuse `WriteResult::Err` with a descriptive message? Either is consistent with this codebase's existing patterns; left as a plan-time decision.
- Whether the quarantine directory this creates should respect the same N=2 bounded-retention rule already sketched for R3.1.2 in the hardening backlog (`docs/superpowers/plans/2026-07-22-pr9-data-integrity.md`), or start unbounded and get folded into that mechanism later when PR9 actually lands. Leaning toward: reuse if PR9's quarantine helper exists by the time this is implemented, otherwise leave unbounded with a `TODO` pointing at PR9 — but this is a sequencing question for whoever picks up this plan, not a design gap.
