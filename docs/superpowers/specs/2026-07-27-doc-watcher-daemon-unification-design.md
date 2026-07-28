# Doc-Watcher Daemon Unification Design

**Goal:** Extend the existing code-watch daemon (`infigraph watch`, spawned via `ensure_daemon_running` in `crates/infigraph-core/src/watch/daemon.rs`, coordinated through `.infigraph/watch.lock`, toggled on via `INFIGRAPH_WATCH_DAEMON=1`) so it also handles document reindexing — instead of building a second, parallel `watch-docs` daemon/lock/subcommand stack. One detached process per repo, one filesystem watch subscription, two independent reindex handlers.

## Background

`feat/watcher-daemon-split` (merged into `feat/health-beacons`) gave MCP's code-watcher an opt-in, off-by-default detached-daemon mode, generalizing the CLI's pre-existing `infigraph watch`/`watch-stop`/`watch-status` machinery. Doc-watching (`infigraph_docs::watch::watch_docs`, driven by MCP's `tool_watch_docs`/`auto_start_doc_watch`) was out of scope for that plan — not because of any architectural incompatibility, but because the CLI never had a `watch-docs` subcommand to generalize from in the first place (only one-shot `index-docs`/`reindex-docs`/`clean-docs`).

Rather than building a second parallel daemon stack for docs (own lock file, own CLI subcommand, own `ensure_docs_daemon_running` primitive) — which would duplicate the OS-level `notify` filesystem watch for the same repo root a second time — this design extends the existing code-watch daemon to run both jobs, since both already watch the identical directory tree.

**Motivation:** consistency/future-proofing, not a specific incident. Doc-watchers dying with MCP worker restarts today is a known asymmetry versus code-watching's new daemon option, worth closing before it widens further — not an active bug report.

**Scope:** daemon-mode only. Today's in-process default (`INFIGRAPH_WATCH_DAEMON` unset — two independent threads, one per subsystem, tracked in separate `WATCHERS`/`DOC_WATCHERS` maps) is untouched. This design only changes what happens when the toggle is on.

**Target:** fork-only, builds on `feat/health-beacons`, no upstream PR without explicit user go-ahead (standing directive).

## Architecture

The daemon process spawned by `spawn_daemon` (still `infigraph watch <root>`, no new subcommand, no new spawn primitive, no new lock file) keeps its single `notify::Watcher` subscription on the repo root. Its event loop dispatches each changed-file event to whichever of two independent handlers are currently attached:

- **Code handler** — today's `infigraph_core::watch::watch_project` reindex logic, unchanged.
- **Doc handler** — today's `infigraph_docs::watch::watch_docs` reindex logic, unchanged.

Handlers attach/detach dynamically based on repo state, checked on a periodic poll (not just at daemon startup):

- Code handler attaches once `.infigraph/` exists (matches `ensure_daemon_running`'s existing precondition).
- Doc handler attaches once `.infigraph/docs.kuzu` exists, and detaches if it stops existing (e.g. after `clean_docs`).

This means a daemon started for a code-only repo automatically picks up doc-watching later once `index_docs` runs for the first time — no restart required — and the reverse (docs indexed first, code indexed later) works the same way.

## Components

- **`crates/infigraph-core/src/watch/`** — gains a handler abstraction (e.g. a `WatchHandler` trait or small enum) with `is_interested(path) -> bool` and `reindex()` per handler kind, so the daemon's dispatch loop is generic rather than hard-coding two copies of dispatch logic. The daemon's poll loop owns a `Vec<Box<dyn WatchHandler>>` (or equivalent) that grows/shrinks as attach/detach conditions change.
- **`.infigraph/watch.lock`** — unchanged; still means "the repo's shared watch daemon," now covering both jobs instead of just code.
- **New stop sentinel** — `.infigraph/watch.stop.docs`, checked alongside the existing `.infigraph/watch.stop`, so the doc handler can be detached independently without killing the daemon or the code handler. The daemon process itself exits only once **both** handlers are detached, or the original `watch.stop` (full-shutdown) sentinel is set.
- **`crates/infigraph-mcp/src/tools/watch.rs` / `tools/docs.rs`** — `auto_start_watch_inner` (code) and `auto_start_doc_watch_inner` (docs) both gain/keep daemon-mode awareness; under `INFIGRAPH_WATCH_DAEMON=1` both converge to calling the same `ensure_daemon_running(root, cli_binary)` — today only the code path has this branch at all, docs gets it added here for the first time.
- **`tool_get_watch_status` / `tool_get_doc_watch_status`** — report per-handler attach state (attached/detached/never-indexed) rather than only "daemon alive or not," so status output reflects which of the two jobs the shared daemon is actually doing right now.
- **`tool_stop_watch` / `tool_stop_watch_docs`** — under daemon mode, each writes its own sentinel (existing `watch.stop` for code, new `watch.stop.docs` for docs) rather than either one tearing down the whole process.

## Data Flow

```
filesystem event
  → daemon's single notify::Watcher receiver
  → for each changed path:
      for each currently-attached handler:
        if handler.is_interested(path): mark pending for that handler
  → each handler's own debounce timer/reindex call fires independently,
    exactly as today's two separate loops already do internally
```

Only the event *source* is shared. Debounce timing and reindex logic per handler are untouched copies of what exists today — this design does not change reindex behavior, only how the filesystem event feed reaches it.

## Error Handling

A panic or reindex error in one handler must not affect the other handler or the daemon process itself. Each handler's reindex call is wrapped independently (mirroring how `watch_docs`/`watch_project` already log-and-continue on error today) — one bad handler tick logs and continues; it does not propagate to the other handler or exit the loop.

## Testing

Mirrors `feat/watcher-daemon-split`'s existing test shape in `crates/infigraph-core/tests/watch_daemon.rs`, extended with:

- Dual-handler dispatch: a single event correctly reaches both handlers when both are attached, and only the relevant handler when just one is.
- Dynamic pickup: `docs.kuzu` appearing mid-run attaches the doc handler without a restart; disappearing (post `clean_docs`) detaches it.
- Independent detach: writing `watch.stop.docs` detaches only the doc handler and leaves the daemon + code handler running; the daemon process only exits once both handlers are gone.
- Regression: existing code-only daemon-mode tests (from the original split) continue passing unchanged — this design must not alter code-watch daemon behavior when docs were never indexed for that repo.
- In-process default path: existing `crates/infigraph-mcp/tests/watcher_reindex.rs` coverage for the two-separate-threads model is unaffected (out of scope for this change, asserted via regression run only).

## Out of Scope

- Unifying the in-process (non-daemon) default path — stays as two independent threads, exactly as today.
- Any change to `INFIGRAPH_WATCH_DAEMON`'s toggle semantics — same env var, same off-by-default behavior, now does more work when on.
- A `watch-docs` CLI subcommand — not needed under this design, since `infigraph watch` now does both jobs.
- PR7b's `mcp.lock` identity/heartbeat/takeover work — orthogonal, tracked separately.
