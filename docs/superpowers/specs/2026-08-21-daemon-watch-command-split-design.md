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

### `WatchableTask` — the shared primitive

One generic type, used identically for the code-watch producer and the doc-watch producer
(and reusable for any future watchable activity), rather than two handwritten
implementations:

```rust
struct WatchableTask {
    role: &'static str,           // "code" | "docs" — drives config section, lock role, logging
    token: CancellationToken,     // this task's own cancellation token (a child of the daemon's)
    handle: Option<JoinHandle<()>>,
}

impl WatchableTask {
    fn spawn(parent: &CancellationToken, root: &Path, role: &'static str, run: impl Future<Output = ()> + Send + 'static) -> Self
    async fn stop(&mut self)     // cancel this task's token, await the join handle
    async fn restart(&mut self, parent: &CancellationToken, root: &Path, run: impl Future<Output = ()> + Send + 'static)
}
```

Each `run` future is a `tokio::task::spawn`ed async loop built around:

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

**What a `WatchableTask` never touches directly:** Kuzu, `held_prism`, or the drain-scheduling
state. It only ever calls `queue.lock().unwrap()...` (mutate) `.drop()` — identical to what
`route_or_serve_request` already does today from the coordinator side. This is what makes the
split safe: the two sides only share one already-`Arc<Mutex<_>>`-wrapped piece of state, not
raw Kuzu connections or in-flight-drain bookkeeping.

### `IndexingTask` — unifying the coordinator's in-flight work

The coordinator already runs its actual work — draining the queue, building a full reindex,
running SCIP enrichment — as genuine tokio tasks: `drain_rt.spawn_blocking(...)` returns a real
`tokio::task::JoinHandle<T>` in all three cases (blocking, not plain `spawn`, because the work
inside touches Kuzu and the filesystem synchronously — that doesn't change here). What's
missing is a shared shape: today each of `InFlightDrain`, `InFlightFullReindex`, and
`InFlightScip` is its own hand-rolled struct, reaped by its own ~30-60 line inline block in the
loop, with no cancellation token and no common vocabulary with `WatchableTask`.

```rust
struct IndexingTask<T> {
    handle: JoinHandle<T>,
    token: CancellationToken,   // child of daemon_token
}
```

The coordinator's scheduling logic is unchanged — it still decides synchronously, on its own
thread, when to call `drain_rt.spawn_blocking(...)` for a drain, a full reindex, or SCIP
enrichment, and still reaps each `IndexingTask`'s `handle` the same way (`is_finished()`/
`block_on`) it reaps `InFlightDrain` et al. today. What changes is that all three now share one
generic type instead of three duplicated ones, and each carries a `daemon_token.child_token()`
— giving them the same cancellation vocabulary as `WatchableTask`, not just a bare
`JoinHandle`.

This also gives the existing R5.4 shutdown watchdog (`c085be0`) a cleaner story: today it polls
`index_op_held_by_self` to decide whether to defer `daemon stop`'s hard exit while a write is
genuinely in flight. With `IndexingTask` in place, "a write is in flight" and "wait for it
before hard-exiting" are expressed in the same terms as the rest of this design — a live
`IndexingTask` under `daemon_token` — rather than a special case bolted onto the watchdog
alone. The watchdog's actual polling mechanism (`index_op_held_by_self`, `watchdog_should_
defer`, the grace/ceiling durations from R5.4) does not change; only the vocabulary describing
what it's waiting for becomes uniform.

### Cancellation hierarchy

```
daemon_token = CancellationToken::new()
code_token   = daemon_token.child_token()   // WatchableTask (code-watch producer)
docs_token   = daemon_token.child_token()   // WatchableTask (doc-watch producer)
// each IndexingTask (drain / full-reindex / SCIP) also takes daemon_token.child_token()
// when the coordinator spawns it — one-shot, so no separate `watch`/`watch-docs`-style
// command ever targets an IndexingTask directly; it's cancelled only via daemon_token.
```

- `daemon stop` cancels `daemon_token` → both `WatchableTask` children and any live
  `IndexingTask` cancel/are-awaited accordingly → the coordinator (still checking `stop_rx`/the
  existing `watch.stop` sentinel — unchanged) also exits → process exits. This is today's only
  "stop everything" path, now reachable by a dedicated command instead of only via `kill <pid>`
  or the old overloaded `watch-stop`.
- `watch stop` cancels `code_token` only. The coordinator, request-serving, and `docs_token`'s
  task are untouched — the daemon process stays alive, still serving `DaemonKuzu` writes.
- `watch-docs stop` cancels `docs_token` only, symmetric.
- `watch start` / `watch-docs start` spawn a fresh child token off `daemon_token` and a new
  `WatchableTask`.
- `restart` = `stop()` then `start()`.

This removes the need to hand-wire which signals a given "stop" needs to hit (today: `stop_rx`
+ `doc_shutdown`, soon a third `code_token` if done by hand) — cancelling the daemon's token
alone correctly tears down everything beneath it.

### Command surface

**CLI**, three independently controllable things per project, all against the same
`infigraph daemon` process:

| Command | Scope | Effect |
|---|---|---|
| `infigraph daemon start\|stop\|restart` | process | whole process: write-serving + both `WatchableTask`s |
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
lock role string, and which `WatchableTask` to act on) — not two handwritten copies, per this
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
to `config.toml` *and* immediately cancels the live task's token (via its role); `enable` clears
the flag and, if nothing is currently running for that role, does not itself start a task —
consistent with `auto_start_on_boot` being a distinct, narrower toggle for *boot-time* startup
specifically. Every opportunistic auto-start call site (`should_auto_watch`,
`start_daemon_watcher_for_startup_dir`, `ensure_daemon_watcher`, `auto_start_doc_watch`) checks
`watch_enabled(role)` before spawning, so `disable` also prevents future auto-starts, not just
pausing a currently-live one.

## Dependencies

- Add `tokio_util` (small official companion crate to `tokio`, same maintainers) for
  `CancellationToken`.
- Widen `infigraph-core`'s existing `tokio` dependency (currently `default-features = false,
  features = ["rt", "rt-multi-thread"]`, used only for `drain_rt`'s `spawn_blocking` executor)
  to add `"time"` (for `tokio::time::interval`/`sleep`) and confirm whether `tokio_util`
  requires tokio's `"sync"` feature transitively — verify exact flags at implementation time.

## Error handling / edge cases

- A `WatchableTask` whose `run` future panics: `JoinHandle::await` returns `Err(JoinError)`;
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
  "is a write in flight" shifts to checking for a live `IndexingTask`, per above.
- An `IndexingTask` whose `JoinHandle` is still running when `daemon_token` cancels: the
  coordinator's existing drain-in-progress-at-shutdown handling (waiting out `drain_in_flight`/
  `full_reindex_in_flight`/`scip_in_flight` via `drain_rt.block_on(...)` before returning) is
  unchanged — `IndexingTask` only changes how the in-flight state is tracked, not the
  wait-it-out-before-exiting behavior itself.

## Testing

- Unit: `WatchableTask::spawn`/`stop`/`restart` lifecycle, independent of any real filesystem
  watching (a no-op `run` future that just awaits `token.cancelled()`).
- Unit: `IndexingTask<T>` reap behavior (finished/still-running/panicked) against a stub
  `spawn_blocking` closure, independent of a real drain/full-reindex/SCIP body — mirrors the
  existing per-variant tests (`build_daemon_command_appends_to_an_existing_log_instead_of_
  truncating`-style isolation) but for the unified type.
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
  thread-safety, and this design's `WatchableTask` split works unchanged regardless of how the
  queue evolves later.
- Whether `infigraph install`/`update` should gracefully `daemon stop` known live daemons
  before swapping the binary — floated, not approved, separate feature.
