# Daemon/Watch Command-Surface Split — Design

**Date:** 2026-08-21
**Status:** Draft

## Problem

`infigraph daemon` is a single process that does two unrelated jobs in one blocking loop
(`watch_project_with_periodic`): it serves `DaemonKuzu` file-drop write requests, and it
watches the filesystem to auto-reindex on change. There is no way to control these
independently. The only lifecycle commands today are `WatchStop`/`WatchStatus`, which act on
whatever is holding `.infigraph/watch.lock` — an external `infigraph daemon` process or the
MCP server's in-process watcher thread. Stopping "watching" against an external daemon today
means killing the whole process, write-serving included, because `cmd_daemon`'s watch call is
synchronous and blocking: when it returns, the function proceeds to shut everything down and
exit.

Doc-watching has the same shape internally (`doc_thread`, its own `Arc<AtomicBool>` shutdown
flag, joined at the end of `cmd_daemon`) but is even less controllable — it's only ever
signalled right after the main watch loop returns, i.e. only at full process exit. At the MCP
layer, by contrast, code-watch and doc-watch are already fully separate: distinct tools
(`watch_project`/`stop_watch` vs `watch_docs`/`stop_watch_docs`), distinct auto-start
functions (`auto_start_watch` vs `auto_start_doc_watch`), distinct daemon-mode toggle test
coverage. The CLI/daemon layer doesn't reflect that separation at all.

This design gives `daemon` (process-level) and `watch`/`watch-docs` (activity-level) their own
command surfaces, at both the CLI and MCP layers, without disturbing the daemon's existing
write-serving/drain-coordination logic, which stays exactly as it is today.

## Non-goals

- Rewriting or restructuring the existing drain-scheduling/request-serving coordinator's
  *scheduling* logic inside `watch_project_with_periodic` — `held_prism` (a live, directly-held
  Kuzu connection) and the decision of *when* to start a drain stay exactly as they are today,
  synchronous, on the coordinator's own thread. Only the bookkeeping around the work those
  decisions kick off changes — see `IndexingTask` below. Converting the coordinator's own
  connection-holding and scheduling into an async task is a separate, much larger, and riskier
  change this design deliberately does not take on.
- Reviving the dead `periodic_secs`/`on_periodic` parameter. Confirmed (again, independently)
  as **R3.3.4** in `docs/DESIGN-hardening.md` §3.3: every production call site passes
  `periodic_secs: 0`. The chosen remediation there is detect-and-surface (the
  `ast_generation`/`scip_generation` gap shown by `doctor`/`get_stats`), not a silent
  background auto-refresh timer, because SCIP re-enrichment can run for minutes. That's
  shipped and correct as-is; this design does not touch it.
- Removing or restructuring `IndexWorkQueue`. Kuzu's multithreading support may eventually
  reduce the queue's role, but the queue also does batching/coalescing (R3.3.5) and backs the
  cross-*process* single-writer invariant (`index.lock`) — concerns independent of Kuzu's raw
  thread-safety. Flagged under Future Considerations, not in scope here.
- The install/update-gracefully-stops-live-daemons idea the user floated in a prior session.
  Separate, smaller feature; not addressed here.

## Architecture

**Naming note:** `watch_project_with_periodic` loses fsevent-watching (→ a producer `Task<()>`)
and the periodic-ignore-rebuild ticker (→ inside that producer's own scheduling). What remains
— drain-scheduling, request-serving, `held_prism` — isn't "watching" anymore; it's the write
coordinator, and needs a name that says so (e.g. `run_daemon_write_coordinator` — exact name is
an implementation-plan detail, not a spec-level decision, but the fact that it changes is not:
the old name would be actively misleading once this ships).

### `Task<T>` — one shared primitive, not two

`WatchableTask` and `IndexingTask<T>` from earlier drafts of this design collapse into a single
generic type. Both are "a cancellable tokio task carrying a role," differing only in which
tokio constructor spawns them and whether a caller cares about the return value — not a reason
for two named types:

```rust
struct Task<T> {
    role: &'static str,          // "code" | "docs" | "full-reindex-build" | "scip-enrich" — config
                                  // section / lock role / logging, per use site
    token: CancellationToken,    // this task's own token, a child of daemon_token
    handle: JoinHandle<T>,
}

impl<T: Send + 'static> Task<T> {
    // For a long-running async loop (the watch producers): T = ()
    fn spawn(parent: &CancellationToken, role: &'static str, fut: impl Future<Output = T> + Send + 'static) -> Self

    // For one-shot, Kuzu-touching work (build phase, SCIP enrichment's import step): runs on
    // tokio's blocking-thread pool, since the closure makes synchronous FFI calls
    fn spawn_blocking(parent: &CancellationToken, role: &'static str, f: impl FnOnce() -> T + Send + 'static) -> Self

    async fn stop(self)              // cancel the token, await the handle, discard the result
    fn is_finished(&self) -> bool    // for the coordinator's per-tick reap-or-not check
    async fn join(self) -> Result<T, JoinError>
}
```

`stop`/`restart` (`stop` then a fresh `spawn`) are how `watch`/`watch-docs enable|disable|
start|stop|restart` act on the code/docs producers (`T = ()`). `is_finished`/`join` are how the
coordinator reaps a full-reindex-build or SCIP-enrichment `Task<T>` on its own tick, exactly as
it reaps `InFlightFullReindex`/`InFlightScip` today — just through one generic type instead of
two duplicated ad-hoc structs, and now carrying a `CancellationToken` neither of them had.
`InFlightDrain` stays completely untouched, for the reasons below.

### Where producer tasks run, and what they wrap

Code/docs producer `Task<()>`s run on their own small dedicated runtime (`watch_rt`, a couple
of worker threads), separate from `drain_rt` (used for `spawn_blocking` indexing work only).
Deliberate isolation, not just tidiness: a bug in a producer's loop body stalling its worker
thread can't also stall `drain_rt`'s ability to dispatch/reap indexing work, or vice versa —
mirrors the watch-activity/indexing-activity boundary this whole design already draws
everywhere else (separate commands, separate tokens, `IndexWorkQueue` as the only shared
state). Cost is one more small runtime per daemon process; negligible, since these tasks are
I/O-bound (waiting on channels/timers), not CPU-bound.

**What a producer `Task<()>` wraps differs between the CLI daemon and MCP's in-process
watcher, and that's fine — it's purely internal, no MCP tool surface change.** The CLI daemon's
`code_token`/`docs_token` tasks are producer-*only*: they feed `queue`, and the coordinator
(unchanged, still alive independently) does the actual draining. MCP's `tool_watch_project`/
`tool_watch_docs` have no separate coordinator to defer to — today, even with
`serve_requests: false`, that single call already does its own local drain-scheduling against
its own `held_prism`, because nothing else will. Neither the coordinator nor its
drain-scheduling logic is exposed as its own MCP tool anywhere, today or in this design — the
tool surface stops at `watch_project`/`stop_watch`/`get_watch_status` (plus this design's new
`enable_watch`/`disable_watch`/`restart_watch` and their `_docs` counterparts), so there's
nothing external that ever needs to address "the coordinator" directly for MCP's case. MCP's
`Task<()>` therefore wraps producer *and* local drain-coordination together as one internal
unit, matching today's `watch_project` behavior exactly, just restructured to be
cancellation-token-driven instead of `stop_rx`/sentinel-driven.

### Dedup — claiming a role before spawning

Duplicate-spawn prevention exists today, scattered across at least three call sites with no
shared implementation: `try_start_full_reindex`'s `if drain_in_flight || full_reindex_in_flight
{ return None; }`, `watcher_running`'s two-tier check (in-process `WATCHERS` map via
`is_watching()`, OR a trial flock on `.infigraph/watch.lock` for another process/another MCP
worker), and the `auto_start_watch`/`auto_start_doc_watch` no-duplicates guarantees. These are
two genuinely different tiers, not one problem wearing two hats, and `Task<T>` should not
conflate them:

- **In-process dedup** — "is a `Task<T>` for this role already running in this process." A
  live `Task<T>` (or `Option<Task<T>>` per slot, for the coordinator's single-instance-per-role
  cases like drain/full-reindex-build/SCIP) already carries this information — no separate
  boolean needed once `Task<T>` exists. For roles that can have more than one instance per
  process conceptually (there aren't any today, but the shape should not assume otherwise), a
  small `TaskRegistry<K>` (`Mutex<HashSet<K>>`, `try_claim(key) -> Option<Claim<K>>` returning
  `None` if already claimed, `Claim` releasing on `Drop` so an aborted/panicked task never
  leaves a phantom entry) generalizes this.
- **Cross-process dedup** — "is another process (another MCP worker, an external `infigraph
  daemon`) already doing this for this root." This is what the trial flock on `.infigraph/
  watch.lock` in `watcher_running` already does today, and it isn't something an in-memory
  registry can answer on its own — it has to stay file-lock-based.

`Task::spawn`/`spawn_blocking`'s dedup-aware variant does both checks in one call — in-process
registry first (cheap), then the cross-process trial-flock (only for producer roles that have
one, i.e. code/docs watching; full-reindex-build and SCIP enrichment are purely in-process,
single-daemon-instance concerns and only need the first tier) — returning a single `Busy`/
`AlreadyRunning` outcome instead of the caller hand-rolling both checks itself. This replaces
`watcher_running`'s standalone logic, `try_start_full_reindex`'s inline flags, and gives
`auto_start_watch`/`auto_start_doc_watch`'s no-duplicates guarantee for free rather than as a
guarantee each call site has to separately uphold.

A code/docs producer's `fut` is a `tokio::task::spawn`ed async loop built around:

```rust
loop {
    tokio::select! {
        _ = token.cancelled() => break,
        Some(event) = fsevent_rx.recv() => { /* mark dirty, feed queue */ }
        _ = ignore_rebuild_interval.tick() => { /* rebuild ignore matcher */ }
    }
}
```

using `tokio::time::interval` for periodic work (no hand-rolled `Instant`/`Duration` ticker)
and a `notify`-callback-bridged `tokio::sync::mpsc` channel in place of today's
`std::sync::mpsc` receiver, so the fsevent wait is a real `.await` arm rather than a polled
`recv_timeout`. `notify`'s own watcher setup and restart-with-backoff logic (`MAX_RESTARTS`)
moves into this task unchanged in behavior, just expressed with `tokio::time::sleep` instead of
`std::thread::sleep`.

**What a code/docs `Task<()>` never touches directly:** Kuzu, `held_prism`, or the
drain-scheduling state. It only ever calls `queue.lock().unwrap()...` (mutate) `.drop()` —
identical to what `route_or_serve_request` already does today from the coordinator side. This
is what makes the split safe: the two sides only share one already-`Arc<Mutex<_>>`-wrapped
piece of state, not raw Kuzu connections or in-flight-drain bookkeeping.

### `Task<T>` for full-reindex and SCIP enrichment — and why drain is excluded

The coordinator runs three kinds of work as `drain_rt.spawn_blocking(...)` tokio tasks today:
queue drains, full reindexes, and SCIP enrichment. Of these, only the latter two get wrapped in
`Task<T>` (via its `spawn_blocking` constructor). **The drain (`InFlightDrain`) is deliberately
left exactly as it is today, with no unification.** Reasoning: drain is the fast, frequent
operation — scheduled whenever the queue has anything, reaped every tick — and today's "wait it
out" behavior at shutdown is already fine for it. Full reindex and SCIP enrichment are the
genuinely long-running ones (minutes, including running external SCIP indexer binaries) with
natural internal checkpoints (build-fresh-then-swap phases; between each external indexer
binary) where checking `token.is_cancelled()` and bailing early meaningfully shortens `daemon
stop`'s shutdown time. A cancellation token buys real value there and none for drain, so it's
added only where it pays for itself.

`InFlightFullReindex` and `InFlightScip` are replaced by `Task<FullReindexBuildOutcome>`/
`Task<ScipOutcome>` (one shared generic type instead of two duplicated ad-hoc structs);
`InFlightDrain` is untouched. The coordinator's scheduling logic is otherwise unchanged — it
still decides synchronously, on its own thread, when to call `drain_rt.spawn_blocking(...)`,
and still reaps each task the same way (`is_finished()`/`block_on` on `.join()`) it does today.

**"Full reindex" is really two phases, and only one of them becomes a `Task<T>`.**
`build_full_reindex` (the `Task<T>`-wrapped part) builds an entirely fresh graph at
`.infigraph/graph.rebuilding/` — its own connection, opened fresh; the live graph is never
touched. Abandoning it mid-build via cancellation is always safe: there's nothing to roll back.
Checked whether any of it can move off `spawn_blocking` the way SCIP's subprocess phase did —
no: it's filesystem cleanup (trivial), Kuzu FFI (`open_local_kuzu_at`/`upsert_files_bulk`/
`resolve_calls`/`derive_tested_by_edges` — irreducibly blocking), and AST extraction/parsing
(CPU-bound, not I/O-bound — parallelism is `rayon`'s job, already a dependency, not
async/await's). No external subprocess anywhere in it, unlike SCIP's Part A. It stays entirely
inside `Task<T>`'s `spawn_blocking` closure.

`finish_full_reindex` (the swap phase — `poison_watch_db`, `graph.lock`, snapshot, retire,
`rename`, reopen, reconcile embeddings) is **not** wrapped in `Task<T>` at all, and gets no
`CancellationToken`. It runs synchronously on the coordinator's own thread, after the build
task is reaped, exactly as it does today. This is deliberate, not an oversight: once the swap
starts, it must run to completion (or use its own existing rollback path on failure) — a
`CancellationToken` checked mid-swap would risk exactly the "no live, openable graph" outage
window `roll_back_to_retired` exists to prevent. Naming it clearly as its own non-cancellable
step (rather than folding it into `Task<T>`'s vocabulary) is meant to stop a future reader from
assuming it shares the build phase's cancellation semantics.

This also gives the existing R5.4 shutdown watchdog (`c085be0`) a cleaner story for the
long-running case specifically: today it polls `index_op_held_by_self` to decide whether to
defer `daemon stop`'s hard exit while a write is genuinely in flight (any of the three). With
`Task<T>` in place, a live full-reindex-build or SCIP-enrichment task can additionally be asked
to cooperatively cancel via `daemon_token` rather than purely waited out — the watchdog's
actual polling mechanism (`index_op_held_by_self`, `watchdog_should_defer`, the grace/ceiling
durations from R5.4) does not change, and drain-in-flight is still purely waited out as today.

### Async subprocess spawning for SCIP indexers

`run_scip_indexers` (Part A of a full reindex, per `cmd_daemon`'s own comment: "Part A
(running the external indexer binaries) is deliberately unlocked... Part B [Kuzu
import]... needs `index.lock`") has no Kuzu dependency at all — it's pure OS process
management, currently done via `run_with_timeout`'s blocking `Command::spawn()` plus a
busy-poll `try_wait()` loop. Unlike Part B (the Kuzu-touching build-fresh-then-swap, which
stays `spawn_blocking`-wrapped inside `Task<T>` — see above), Part A genuinely can become
async: `tokio::process::Command` + `.wait().await` is a real non-blocking wait, integrated with
the OS's process-exit notification via tokio's reactor, not a poll loop — a strict improvement
over what's there today, and it lets indexer subprocesses for different languages run
concurrently as real async tasks instead of each consuming a blocking-thread-pool slot.
`run_scip_indexers` moves onto `tokio::process::Command`; `run_with_timeout`'s busy-poll
`try_wait()` pattern is retired in favor of `tokio::time::timeout(...)` wrapping the async
`.wait()`.

### Cancellation hierarchy

```
daemon_token = CancellationToken::new()
code_token   = daemon_token.child_token()   // Task<()> — code-watch producer
docs_token   = daemon_token.child_token()   // Task<()> — doc-watch producer
// full-reindex-build and SCIP-enrichment Task<T>s also take daemon_token.child_token()
// when the coordinator spawns them — one-shot, so no separate `watch`/`watch-docs`-style
// command ever targets one directly; cancelled only via daemon_token. Drain has no token
// at all (InFlightDrain, unchanged).
```

- `daemon stop` submits `WatchControl { role: "daemon", action: Stop }` (see the process-
  boundary section below), which cancels `daemon_token` → both producer `Task<()>`s and any
  live full-reindex-build/SCIP `Task<T>` cancel/are-awaited accordingly → the coordinator
  (still also checking `stop_rx`, for the manual-`kill`-as-fallback case) exits → process
  exits. This replaces today's overloaded, undecorated `watch.stop` sentinel with a named
  command instead of an unlabeled file drop.
- `watch stop` cancels `code_token` only. The coordinator, request-serving, and `docs_token`'s
  task are untouched — the daemon process stays alive, still serving `DaemonKuzu` writes.
- `watch-docs stop` cancels `docs_token` only, symmetric.
- `watch start` / `watch-docs start` spawn a fresh child token off `daemon_token` and a new
  `Task<()>` via `Task::spawn`.
- `restart` = `stop()` then `start()`.

This removes the need to hand-wire which signals a given "stop" needs to hit (today: `stop_rx`
+ `doc_shutdown`, soon a third `code_token` if done by hand) — cancelling the daemon's token
alone correctly tears down everything beneath it.

### Crossing the process boundary: `WatchControl` requests bridge into the token hierarchy

`CancellationToken` is in-process-only. A separate `infigraph watch stop` CLI invocation, or an
MCP tool call, is a different OS process from the running `infigraph daemon` — it cannot call
`.cancel()` on anything living in that process's memory. Only two things cross a process
boundary here: OS signals (how `daemon stop`'s full-process kill already works today — SIGTERM/
Ctrl-C to the PID recorded in `watch.lock`, caught by the daemon's existing handler) and files
— which is what the existing `watch.stop`/`watch.stop.docs` sentinel pattern already is
(`tool_stop_watch_docs` writes `.infigraph/watch.stop.docs` specifically because there's no
in-process `DOC_WATCHERS` entry to touch once a daemon is alive).

So the token hierarchy governs propagation *inside* the daemon process once triggered; the
bridge is not a bespoke sentinel-file convention but the **existing `WriteRequest`/
`route_or_serve_request` protocol**, extended with new watch-control variants. Concretely:

```rust
enum WriteRequest {
    // ...Index, RemoveFiles, UpsertFilesBulk, ResolveCalls, FullReindex...
    WatchControl { role: &'static str, action: WatchAction },  // role: "code" | "docs" | "daemon"
}
enum WatchAction { Start, Stop, Enable, Disable, Restart }
```

`role: "daemon"` (only `Stop`/`Restart` are meaningful for it) is what `daemon stop`/`daemon
restart` submit, cancelling `daemon_token` itself. This matters because — checked
`cmd_watch_stop`'s actual body — today's CLI stop command has *never* been OS-signal-based:
`std::fs::write(&sentinel, b"")` is the entire implementation; a real SIGTERM/Ctrl-C only
happens if someone manually runs `kill <pid>` against the daemon, which the CLI's own command
doesn't do. Both are independently checked in the loop today (`stop_rx.try_recv()` for a real
signal, `sentinel.exists()` for the file), and either currently breaks the one fused loop. So
the coordinator's own full-exit belongs in this same unified protocol too — `role: "daemon"`
replaces the *undecorated* `watch.stop` sentinel exactly as `role: "code"`/`"docs"` replace
`watch.stop`/`watch.stop.docs`'s per-activity uses — rather than leaving it as a separate
legacy mechanism sitting beside the new one. A manual `kill <pid>`/Ctrl-C remains available as
an always-present fallback (still caught by the daemon's existing `ctrlc` handler, still
feeding `stop_rx`), but it's a fallback, not what any command in this design's own surface
actually sends.

routed through `route_or_serve_request`'s existing `match request { ... }` alongside the other
variants — not a parallel `sentinel.exists()` check. `route_or_serve_request`'s signature grows
to also take a handle to the coordinator's producer `Task<()>`s (code/docs), the same way it
already takes `queue`; the new arm cancels/respawns the named role's task (and, for `Enable`/
`Disable`, also flips `config.toml`'s persisted flag) the same way the existing arms mutate
`queue` or drive `try_start_full_reindex`. This **replaces** today's `watch.stop`/
`watch.stop.docs` sentinel-file convention entirely rather than relocating it — one request
protocol, one dispatcher, instead of a structured-request path and a bespoke sentinel-file path
doing adjacent jobs side by side. `tool_stop_watch`/`tool_stop_watch_docs`'s daemon-alive branch
(today: writes the sentinel file directly) becomes: submit a `WatchControl { role, action: Stop
}` request the same way a client already submits `Index`/`FullReindex` — through the existing
client-side submit-and-poll-for-reply helper, no new protocol shape for callers to learn.

Detection is still event-driven, not a per-tick poll: the coordinator's existing
`requests_dir` handling (`std::fs::read_dir(&requests_dir)` inside `if serve_requests`, every
~200ms tick today regardless of whether anything changed) gets a **second, narrow
`notify::Watcher` registration scoped to `.infigraph/requests/`** (non-recursive) feeding the
same dispatch, rather than remaining a blind poll — a genuine improvement to the existing
request-serving path, not just to watch-control specifically, since both now go through the
identical `WriteRequest` shape. (The ignore-matcher-rebuild ticker elsewhere stays a
`tokio::time::interval` — it's about picking up edits to `.gitignore`/`.infigraphignore` at the
project root, which *are* already covered by the main content watch, just not on a useful
cadence for that purpose, so there's no natural event to hang it on instead.)

A `WatchControl { action: Stop }` request's arrival translates into cancelling the named
producer's own local token — a targeted, sub-process-level analog of what a full OS signal does
for `daemon stop`. Every *external* caller (a fresh CLI process's `watch stop`/`watch-docs
stop`, and both the existing and new MCP tools) funnels through this same request-based bridge
when targeting an already-running external daemon. Only a *same-process* caller — MCP's own
in-process, non-daemon-mode watcher; the daemon's own Ctrl-C handler — can skip the bridge and
touch a token directly, because it's already living in the process that owns it.

This also resolves the third case `tool_watch_project_respects_daemon_mode_toggle` surfaces:
with `INFIGRAPH_BACKEND=daemon`, `tool_watch_project` never touches the in-process `WATCHERS`
map at all — it delegates entirely to the external daemon via `ensure_daemon_watcher` and
returns. No local `Task<()>` exists to control in that mode. So the new `enable_watch`/
`disable_watch`/`restart_watch` MCP tools (and their `_docs` counterparts) must, like
`tool_stop_watch`/`tool_stop_watch_docs` already do, detect daemon-mode and route through the
`WatchControl` request bridge to the external daemon's `code_token`/`docs_token` rather than
assume a local task exists — there are three cases in total, not two: the CLI daemon's
producer-only task, MCP's in-process combined task (daemon-mode off), and MCP delegating
entirely to an external daemon (daemon-mode on) with no local task of its own.

### Command surface

**CLI**, three independently controllable things per project, all against the same
`infigraph daemon` process:

| Command | Scope | Effect |
|---|---|---|
| `infigraph daemon start\|stop\|restart` | process | whole process: write-serving + both producer `Task<()>`s |
| `infigraph watch enable\|disable\|start\|stop\|restart` | `code_token`'s task | fsevent-driven code reindexing only |
| `infigraph watch-docs enable\|disable\|start\|stop\|restart` | `docs_token`'s task | doc-watch loop only |

Exact subcommand shape for the docs case (a hyphenated top-level command vs. a `docs` flag/
subcommand under `watch`) is left to the implementation plan.

**MCP**, mirroring the existing code/docs tool separation:

| Existing | New (code) | New (docs) |
|---|---|---|
| `watch_project`, `stop_watch`, `get_watch_status` | `enable_watch`, `disable_watch`, `restart_watch` | `enable_watch_docs`, `disable_watch_docs`, `restart_watch_docs` |
| `watch_docs`, `stop_watch_docs` | | |

Both the CLI command handlers and the MCP tool implementations are one generic
role-parameterized function each (role = `"code"` or `"docs"`, selecting the config section,
lock role string, and which `Task<()>` to act on) — not two handwritten copies, per this
repo's DRY-first convention.

### Persisted policy: `enable`/`disable`

`enable`/`disable` are persisted (survive process restarts and MCP boots); `start`/`stop` are
one-shot (act on the currently-live task only, don't touch persisted policy — a later restart
or auto-start re-arms watching by default).

Generalizes the exact precedence already established by
`crates/infigraph-mcp/src/session_context.rs::auto_start_watch_on_boot_enabled`
(env var → `config.toml` → hardcoded default `true`), parameterized by section instead of
hardcoded to `"watch"`:

```rust
fn watch_enabled(section: &str) -> bool {  // section: "watch" | "watch_docs"
    let env_key = format!("INFIGRAPH_{}_ENABLED", section.to_uppercase());
    if let Ok(v) = std::env::var(&env_key) {
        return v != "0" && v.to_lowercase() != "false";
    }
    load_config_file().section_enabled(section)  // [watch].enabled / [watch_docs].enabled
}
```

`WatchConfig` in `session_context.rs` gains an `enabled: bool` field (default `true`) alongside
the existing `auto_start_on_boot`; a new `WatchDocsConfig` mirrors it. `disable` writes the flag
to `config.toml` — the persisted half — and, per the process-boundary bridge above, immediate
effect on an already-live task requires the same `WatchControl` request mechanism `stop` uses (a
same-process caller can cancel the token directly; an external CLI/MCP caller cannot, so it
still needs the request-based bridge). `enable` clears the flag and, if nothing is currently
running for that
role, does not itself start a task — consistent with `auto_start_on_boot` being a distinct,
narrower toggle for *boot-time* startup specifically. Every opportunistic auto-start call site
(`should_auto_watch`, `start_daemon_watcher_for_startup_dir`, `ensure_daemon_watcher`,
`auto_start_doc_watch`) checks `watch_enabled(role)` before spawning, so `disable` also
prevents future auto-starts, not just pausing a currently-live one.

## Dependencies

- Add `tokio_util` (small official companion crate to `tokio`, same maintainers) for
  `CancellationToken`.
- Widen `infigraph-core`'s existing `tokio` dependency (currently `default-features = false,
  features = ["rt", "rt-multi-thread"]`, used only for `drain_rt`'s `spawn_blocking` executor)
  to add `"time"` (for `tokio::time::interval`/`sleep`) and `"process"` (for
  `tokio::process::Command`), and confirm whether `tokio_util` requires tokio's `"sync"`
  feature transitively — verify exact flags at implementation time.

## Error handling / edge cases

- A producer `Task<()>` whose `fut` panics: `JoinHandle::await` returns `Err(JoinError)`;
  `stop()`/`restart()` must not itself panic on a dead handle — log and treat as already-
  stopped, mirroring how `doc_thread.join()` is already tolerant of this today (`let _ =
  doc_thread.join();`).
- `watch stop` against a role with no live task (e.g. already disabled): report "not running"
  rather than erroring, consistent with existing `stop_watch_by_path_reports_no_watcher_when_
  none_running` behavior.
- `daemon stop` while a `watch`/`watch-docs` task is mid-restart: cancelling `daemon_token`
  cancels both children regardless of their individual state — no special-casing needed, this
  is exactly what the parent/child hierarchy is for.
- The coordinator's own shutdown watchdog (R5.4/`c085be0`'s in-flight-write-aware grace period)
  is unaffected in behavior — it already only fires on `daemon_token`'s cancellation path (full
  process exit), never on a `watch`/`watch-docs`-only stop. Only its internal vocabulary for
  "is a write in flight" shifts to checking for a live full-reindex-build/SCIP `Task<T>`, per
  above.
- A full-reindex-build/SCIP `Task<T>` whose `JoinHandle` is still running when `daemon_token`
  cancels: gets the cooperative-cancel opportunity described above at its next checkpoint, but
  the coordinator still falls back to waiting out the handle (`drain_rt.block_on(...)`) if it
  doesn't exit promptly — same fallback shape as today, just with an earlier-exit path added
  on top. `InFlightDrain` in-progress-at-shutdown handling is completely unchanged: purely
  waited out, no token, no cooperative-cancel path.

## Testing

- Unit: `Task::spawn`/`stop`/`restart` lifecycle (the producer/`T = ()` path), independent of
  any real filesystem watching (a no-op future that just awaits `token.cancelled()`).
- Unit: `TaskRegistry::try_claim` — second claim on a live key returns `None`; the claim
  releases (allowing a fresh one) after the guard drops, including on a panicking task.
- Regression: dedup-aware spawn covers the existing duplicate-prevention tests'
  scenarios — `test_second_watch_project_call_declines_when_already_watching` (cross-process,
  via the trial-flock tier), `test_auto_start_watch_no_duplicates`/
  `test_auto_start_doc_watch_no_duplicates` (in-process, via the registry tier).
- Unit: `Task::spawn_blocking` reap behavior (finished/still-running/panicked, plus the
  cooperative-cancel-at-checkpoint path) against a stub closure, independent of a real
  full-reindex/SCIP body — mirrors the existing per-variant test isolation but for the unified
  type. `InFlightDrain`'s existing tests are untouched, since the struct itself doesn't change.
- Unit: `run_scip_indexers` on `tokio::process::Command` — a real short-lived child process,
  confirm the non-blocking `.wait()` path and `tokio::time::timeout` cancellation behave
  equivalently to today's `run_with_timeout` coverage (e.g.
  `scip_enrich_exit_message_warns_on_nonzero_exit`), and that multiple language indexers
  genuinely run concurrently rather than serially.
- Unit: `watch_enabled("watch")` / `watch_enabled("watch_docs")` precedence (env var → config
  → default), mirroring the existing `auto_start_watch_on_boot_enabled_env_override_priority`
  test shape.
- Integration (real process, matching `cmd_watch_daemon_also_indexes_docs_without_restart`'s
  style): spawn a real detached `infigraph daemon`, `infigraph watch stop`, confirm the process
  is still alive and still serves a write request, confirm no further fsevent-triggered
  reindexing happens; `infigraph watch start`, confirm reindexing resumes without restarting
  the process. Symmetric test for `watch-docs`.
- Integration: `infigraph daemon stop` still tears down both tasks and the coordinator, process
  exits, matching today's `graceful_shutdown.rs`-style coverage.
- Regression: `watch disable` persists across a `daemon restart` (task does not come back).

## Future considerations (explicitly out of scope)

- Kuzu/`lbug`'s multithreading support may eventually let concurrent writers bypass
  `IndexWorkQueue`'s single-drain-coordinator role. Not pursued here — the queue's batching
  (R3.3.5) and the cross-process single-writer invariant are separate concerns from Kuzu's raw
  thread-safety, and this design's `Task<T>` split works unchanged regardless of how the queue
  evolves later.
- Whether `infigraph install`/`update` should gracefully `daemon stop` known live daemons
  before swapping the binary — floated, not approved, separate feature.
