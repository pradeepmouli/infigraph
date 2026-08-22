# Daemon/Watch Command-Surface Split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split `infigraph daemon` (one process, one blocking loop) into independently-controllable process lifecycle (`daemon start/stop/restart`) and activity lifecycle (`watch`/`watch-docs enable/disable/start/stop/restart`), via a shared `Task<T>` cancellable-task primitive and a unified `WriteRequest::WatchControl` cross-process control protocol.

**Architecture:** A new `Task<T>` type (tokio task + `CancellationToken`, two constructors — `spawn` for async producer loops, `spawn_blocking` for one-shot Kuzu work) replaces three duplicated ad-hoc in-flight-work structs and gives code/docs watching real independent cancellation. Watch-control crosses process boundaries through the *existing* `WriteRequest`/`route_or_serve_request` daemon protocol (a new `WatchControl` variant), not a new sentinel convention. The drain-scheduling/request-serving coordinator and the full-reindex swap phase are explicitly left untouched.

**Tech Stack:** Rust, tokio (`rt`, `rt-multi-thread`, `time`, `process`, `sync`), `tokio_util::sync::CancellationToken`, `notify` (fsevents), `clap` (CLI), MCP tool handlers.

**Spec:** `docs/superpowers/specs/2026-08-21-daemon-watch-command-split-design.md` — read it before starting; this plan implements it task-by-task, and cites the exact spec section per phase below.

## Global Constraints

- `infigraph-core`'s `tokio` dependency stays `default-features = false` — only add the specific features each task needs (`time`, `process`; confirm whether `tokio_util`'s `CancellationToken` needs `sync` transitively in Task 1).
- Never touch `held_prism`, the coordinator's drain-scheduling decisions, or `finish_full_reindex`'s swap-phase code path (`poison_watch_db` → `graph.lock` → snapshot → retire → rename → reopen) — these stay synchronous and non-cancellable, exactly as today. (Spec: Non-goals, "Full reindex is really two phases".)
- `InFlightDrain` is not touched or unified — leave it exactly as it is. (Spec: "why drain is excluded".)
- Every new CLI/MCP command that targets an *external* (already-running) daemon must go through the `WriteRequest::WatchControl` request bridge — never assume in-process state exists. (Spec: "Crossing the process boundary".)
- `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings` must pass before any commit that isn't a WIP checkpoint the plan explicitly marks otherwise.
- Run `cargo test -p infigraph-core` (and the affected crate) after every task, not just at the end of a phase — this repo's CI runs the full suite; catching a regression one task late is cheaper than five tasks late.

---

## File Structure

**New files:**
- `crates/infigraph-core/src/watch/task.rs` — `Task<T>`, `TaskRegistry<K>`, `Claim<K>` (Phase 1)
- `crates/infigraph-core/src/watch/producer.rs` — the extracted async fsevent-watching loop, used by both the CLI daemon's producer-only mode and MCP's in-process combined mode (Phase 3)

**Modified files:**
- `crates/infigraph-core/Cargo.toml` — add `tokio_util`, widen `tokio` features (Phase 1)
- `crates/infigraph-core/src/watch/mod.rs` — `pub mod task;`, `pub mod producer;`; `try_start_full_reindex`/`finish_full_reindex` onto `Task<T>` (Phase 2); extract producer logic, rename the coordinator, wire `daemon_token`/`code_token`/`docs_token` (Phase 3)
- `crates/infigraph-core/src/daemon_protocol.rs` — `WriteRequest::WatchControl` variant (Phase 3)
- `crates/infigraph-cli/src/index.rs` — `run_scip_indexer_cmd` onto `tokio::process::Command` (Phase 2)
- `crates/infigraph-cli/src/info_commands.rs` — `cmd_daemon` rewiring; new `cmd_daemon_stop`/`cmd_daemon_restart`/`cmd_watch_*`/`cmd_watch_docs_*` handlers (Phase 3-4)
- `crates/infigraph-cli/src/main.rs` — new `Commands` variants + `run()` dispatch arms (Phase 4)
- `crates/infigraph-mcp/src/session_context.rs` — `WatchConfig.enabled`, new `WatchDocsConfig` (Phase 4)
- `crates/infigraph-mcp/src/tools/watch.rs` — `enable_watch`/`disable_watch`/`restart_watch` tools (Phase 5)
- `crates/infigraph-mcp/src/tools/docs.rs` — `_docs` counterparts (Phase 5)
- `crates/infigraph-mcp/src/lib.rs` (or wherever tools are registered — confirm at Task 15) — register the five new MCP tools

**Test files:**
- `crates/infigraph-core/src/watch/task.rs` — inline `#[cfg(test)] mod tests` (Task<T>/TaskRegistry unit tests)
- `crates/infigraph-core/tests/watch_daemon.rs` — full-reindex/SCIP `Task<T>` integration coverage
- `crates/infigraph-cli/src/index.rs` — inline tests for `run_scip_indexer_cmd`'s async version
- `crates/infigraph-core/tests/watch_control.rs` (new) — `WatchControl` request routing, producer stop/start integration
- `crates/infigraph-cli/tests/watch_daemon_docs.rs` — extend for `watch-docs` CLI commands
- `crates/infigraph-mcp/tests/watcher_daemon_mode.rs` — extend for the new MCP tools' daemon-mode routing

---

## Phase 1: `Task<T>` + `TaskRegistry` Primitive

*(Spec: "`Task<T>` — one shared primitive, not two"; "Dedup — claiming a role before spawning". Foundational, zero behavior change — a new module nothing else depends on yet.)*

### Task 1: Add dependencies

**Files:**
- Modify: `crates/infigraph-core/Cargo.toml`

**Interfaces:**
- Produces: `tokio_util::sync::CancellationToken` available to `infigraph-core`; `tokio::time`, `tokio::process` available.

- [ ] **Step 1: Add the dependency lines**

In `crates/infigraph-core/Cargo.toml`, find the existing `tokio` line:

```toml
tokio = { version = "1", default-features = false, features = ["rt", "rt-multi-thread"] }
```

Replace it with:

```toml
tokio = { version = "1", default-features = false, features = ["rt", "rt-multi-thread", "time", "process", "sync"] }
tokio-util = "0.7"
```

(`"sync"` is included pre-emptively — `tokio_util::sync::CancellationToken` depends on tokio's internal synchronization primitives; if `cargo build` succeeds without it, drop it from the feature list before Step 3's commit, since Global Constraints says only add what's needed.)

- [ ] **Step 2: Verify it builds**

Run: `cargo build -p infigraph-core`
Expected: builds cleanly. If `tokio-util` pulls in an unexpected transitive dependency tree, run `cargo tree -p infigraph-core -i tokio-util` and confirm it's small (it should be — `tokio-util` is a thin, official companion crate).

- [ ] **Step 3: Commit**

```bash
git add crates/infigraph-core/Cargo.toml Cargo.lock
git commit -m "build(core): add tokio_util, widen tokio features for Task<T>"
```

### Task 2: `Task<T>` primitive

**Files:**
- Create: `crates/infigraph-core/src/watch/task.rs`
- Modify: `crates/infigraph-core/src/watch/mod.rs:1` (add `pub mod task;`)

**Interfaces:**
- Produces: `pub struct Task<T>`, `impl<T: Send + 'static> Task<T> { fn spawn(...), fn spawn_blocking(...), async fn stop(self), fn is_finished(&self) -> bool, async fn join(self) -> Result<T, tokio::task::JoinError> }`. Every later task that needs a cancellable unit of work (Phase 2's full-reindex-build/SCIP, Phase 3's code/docs producers) imports this.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/src/watch/task.rs`:

```rust
//! `Task<T>`: one cancellable-task primitive for both long-running producer
//! loops (spawned via [`Task::spawn`]) and one-shot blocking work (spawned
//! via [`Task::spawn_blocking`]) — see
//! docs/superpowers/specs/2026-08-21-daemon-watch-command-split-design.md
//! "`Task<T>` — one shared primitive, not two".

use std::future::Future;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub struct Task<T> {
    pub role: &'static str,
    pub token: CancellationToken,
    handle: JoinHandle<T>,
}

impl<T: Send + 'static> Task<T> {
    pub fn spawn(
        parent: &CancellationToken,
        role: &'static str,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> Self {
        let token = parent.child_token();
        let handle = tokio::task::spawn(fut);
        Task {
            role,
            token,
            handle,
        }
    }

    pub fn spawn_blocking(
        parent: &CancellationToken,
        role: &'static str,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Self {
        let token = parent.child_token();
        let handle = tokio::task::spawn_blocking(f);
        Task {
            role,
            token,
            handle,
        }
    }

    /// Cancel the token and await the handle, discarding the result.
    /// Tolerant of a panicked task (logs, doesn't propagate the panic) --
    /// mirrors how `doc_thread.join()` is already tolerant of this today.
    pub async fn stop(self) {
        self.token.cancel();
        if let Err(e) = self.handle.await {
            eprintln!("[task:{}] stop: task panicked: {e}", self.role);
        }
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    pub async fn join(self) -> Result<T, tokio::task::JoinError> {
        self.handle.await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn spawn_runs_until_cancelled() {
        let parent = CancellationToken::new();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = ran.clone();
        let task = Task::spawn(&parent, "test", async move {
            ran_clone.store(true, Ordering::SeqCst);
            // Wait for cancellation rather than returning immediately, so
            // `stop()` below is exercised against a genuinely running task.
            futures_lite_wait().await;
        });
        // give the spawned task a tick to run its first statement
        tokio::task::yield_now().await;
        assert!(ran.load(Ordering::SeqCst));
        task.stop().await;
    }

    #[tokio::test]
    async fn stop_tolerates_a_panicking_task() {
        let parent = CancellationToken::new();
        let task: Task<()> = Task::spawn(&parent, "test", async {
            panic!("boom");
        });
        // Must not panic itself, and must return promptly.
        task.stop().await;
    }

    #[tokio::test]
    async fn spawn_blocking_join_returns_the_value() {
        let parent = CancellationToken::new();
        let task = Task::spawn_blocking(&parent, "test", || 42);
        let result = task.join().await.unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn is_finished_reflects_completion() {
        let parent = CancellationToken::new();
        let task = Task::spawn_blocking(&parent, "test", || ());
        // Poll until finished rather than asserting immediately -- spawn_blocking
        // genuinely runs on a separate thread, so completion isn't synchronous
        // with the spawn call returning.
        for _ in 0..100 {
            if task.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(task.is_finished());
        task.join().await.unwrap();
    }

    // A tiny helper standing in for "wait until cancelled" without pulling
    // in the CancellationToken from the closure's own scope (kept out of
    // the closure to keep the spawn_runs_until_cancelled test's intent
    // clear: it's testing that `stop()` can interrupt a task that never
    // returns on its own).
    async fn futures_lite_wait() {
        std::future::pending::<()>().await
    }
}
```

Note: `spawn_runs_until_cancelled`'s task never checks its own token — that's intentional for this test (it verifies `stop()` cancels the token and the `JoinHandle` resolves via `tokio::task::spawn`'s own abort-on-drop... actually `tokio::task::spawn` does NOT abort on drop by default, so a task that never checks its token would hang `stop()` forever). Fix this before running: replace `futures_lite_wait()` with a token-aware wait so the test is honest about what `stop()` actually guarantees (it cancels the token; the task must itself observe cancellation to exit — `Task` does not force-abort). Rewrite the test body:

```rust
    #[tokio::test]
    async fn spawn_runs_until_cancelled() {
        let parent = CancellationToken::new();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = ran.clone();
        let child_token = parent.child_token(); // mirrors what Task::spawn does internally
        let task = Task::spawn(&parent, "test", {
            let token = child_token.clone();
            async move {
                ran_clone.store(true, Ordering::SeqCst);
                token.cancelled().await;
            }
        });
        tokio::task::yield_now().await;
        assert!(ran.load(Ordering::SeqCst));
        task.stop().await;
    }
```

This still isn't quite right either — `Task::spawn` creates its own child token internally (`self.token`), and the closure passed to `spawn` can't see it before `Task::spawn` returns. Resolve this properly: `Task::spawn`'s signature needs the future to receive the token, or the caller creates the child token itself and passes it into the future *and* into `Task::spawn` isn't how the API is shaped above (`Task::spawn` derives its own child from `parent`). Fix by having the test derive the exact same child token `Task::spawn` will derive — but `CancellationToken::child_token()` creates a *new, distinct* child each call, so this won't line up.

**Resolve this design gap now, before writing more tests**: `Task::spawn`'s future needs access to *its own* task's token to be cancellable at all — today's sketch above has no way for the future to observe `self.token`. Fix `Task::spawn`'s signature to pass the token into the future-constructing closure:

```rust
    pub fn spawn(
        parent: &CancellationToken,
        role: &'static str,
        make_fut: impl FnOnce(CancellationToken) -> F,
    ) -> Self
    where
        F: Future<Output = T> + Send + 'static,
    {
        let token = parent.child_token();
        let handle = tokio::task::spawn(make_fut(token.clone()));
        Task { role, token, handle }
    }
```

and correspondingly:

```rust
    pub fn spawn_blocking(
        parent: &CancellationToken,
        role: &'static str,
        make_f: impl FnOnce(CancellationToken) -> T + Send + 'static,
    ) -> Self
    where
        T: Send + 'static,
    {
        let token = parent.child_token();
        let handle = tokio::task::spawn_blocking(move || make_f(token));
        Task { role, token, handle: /* see note below */ handle }
    }
```

Wait — `spawn_blocking`'s closure runs synchronously and can't `.await` a token; giving it the token is still useful (it can call `token.is_cancelled()` at a checkpoint, non-async), so `make_f: impl FnOnce(CancellationToken) -> T` is correct there without an `F: Future` bound. Update the struct's doc comment to note this asymmetry: `spawn`'s closure receives an owned token for `.await`-based cancellation; `spawn_blocking`'s closure receives one for synchronous `is_cancelled()` checkpoint polling. Rewrite Step 1's full file with this corrected signature before proceeding to Step 2.

- [ ] **Step 1 (corrected): Write `task.rs` with the token-passing signatures**

Replace the `impl<T: Send + 'static> Task<T>` block from Step 1 with:

```rust
impl<T: Send + 'static> Task<T> {
    /// Spawn a long-running async task. `make_fut` receives this task's own
    /// child token, so the future can `.await` `token.cancelled()` to know
    /// when to stop.
    pub fn spawn<F>(
        parent: &CancellationToken,
        role: &'static str,
        make_fut: impl FnOnce(CancellationToken) -> F,
    ) -> Self
    where
        F: Future<Output = T> + Send + 'static,
    {
        let token = parent.child_token();
        let handle = tokio::task::spawn(make_fut(token.clone()));
        Task {
            role,
            token,
            handle,
        }
    }

    /// Spawn one-shot blocking work on tokio's blocking-thread pool.
    /// `make_f` receives this task's own child token for synchronous
    /// `token.is_cancelled()` checkpoint polling (the closure runs
    /// synchronously, so it cannot `.await` cancellation).
    pub fn spawn_blocking(
        parent: &CancellationToken,
        role: &'static str,
        make_f: impl FnOnce(CancellationToken) -> T + Send + 'static,
    ) -> Self {
        let token = parent.child_token();
        let token_for_closure = token.clone();
        let handle = tokio::task::spawn_blocking(move || make_f(token_for_closure));
        Task {
            role,
            token,
            handle,
        }
    }

    pub async fn stop(self) {
        self.token.cancel();
        if let Err(e) = self.handle.await {
            eprintln!("[task:{}] stop: task panicked: {e}", self.role);
        }
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    pub async fn join(self) -> Result<T, tokio::task::JoinError> {
        self.handle.await
    }
}
```

and the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn spawn_runs_until_cancelled() {
        let parent = CancellationToken::new();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = ran.clone();
        let task = Task::spawn(&parent, "test", move |token| async move {
            ran_clone.store(true, Ordering::SeqCst);
            token.cancelled().await;
        });
        tokio::task::yield_now().await;
        assert!(ran.load(Ordering::SeqCst));
        task.stop().await;
    }

    #[tokio::test]
    async fn stop_tolerates_a_panicking_task() {
        let parent = CancellationToken::new();
        let task: Task<()> = Task::spawn(&parent, "test", |_token| async {
            panic!("boom");
        });
        task.stop().await;
    }

    #[tokio::test]
    async fn spawn_blocking_join_returns_the_value() {
        let parent = CancellationToken::new();
        let task = Task::spawn_blocking(&parent, "test", |_token| 42);
        let result = task.join().await.unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn spawn_blocking_checkpoint_sees_cancellation() {
        let parent = CancellationToken::new();
        let task = Task::spawn_blocking(&parent, "test", |token| {
            // Simulates a long-running blocking body with a checkpoint.
            std::thread::sleep(std::time::Duration::from_millis(50));
            token.is_cancelled()
        });
        parent.cancel();
        let saw_cancelled = task.join().await.unwrap();
        assert!(saw_cancelled, "checkpoint should observe the parent's cancellation");
    }

    #[tokio::test]
    async fn is_finished_reflects_completion() {
        let parent = CancellationToken::new();
        let task = Task::spawn_blocking(&parent, "test", |_token| ());
        for _ in 0..100 {
            if task.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(task.is_finished());
        task.join().await.unwrap();
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p infigraph-core --lib watch::task -- --nocapture`
Expected: FAIL — compile error (`task` module not yet registered in `watch/mod.rs`).

- [ ] **Step 3: Register the module**

In `crates/infigraph-core/src/watch/mod.rs`, near the top (alongside other `pub` module-level items like `pub mod queue;`, `pub mod daemon;` if present — check the file's existing module declarations and add alongside them):

```rust
pub mod task;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p infigraph-core --lib watch::task -- --nocapture`
Expected: PASS (5 tests: `spawn_runs_until_cancelled`, `stop_tolerates_a_panicking_task`, `spawn_blocking_join_returns_the_value`, `spawn_blocking_checkpoint_sees_cancellation`, `is_finished_reflects_completion`).

- [ ] **Step 5: `cargo fmt` and `clippy`**

Run: `cargo fmt -p infigraph-core -- --check && cargo clippy -p infigraph-core --lib -- -D warnings`
Fix anything flagged (a common one: `Task<T>`'s `role`/`token` fields being `pub` but never read outside the module yet — `#[allow(dead_code)]` is NOT the fix; `role` is read by `stop()`'s eprintln and will be read by later tasks' dedup/logging, so clippy shouldn't flag it as genuinely dead — if it does, it means a field truly isn't used yet, in which case leave it `pub` without a lint suppression and let Phase 2/3's usage silence it naturally).

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/watch/task.rs crates/infigraph-core/src/watch/mod.rs
git commit -m "feat(core): add Task<T> cancellable-task primitive"
```

### Task 3: `TaskRegistry<K>` in-process dedup

**Files:**
- Modify: `crates/infigraph-core/src/watch/task.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub struct TaskRegistry<K>`, `pub struct Claim<K>` (RAII, releases on drop), `impl<K: Eq + Hash + Clone + Send + 'static> TaskRegistry<K> { fn new() -> Self, fn try_claim(&self, key: K) -> Option<Claim<K>> }`. Phase 2/3 tasks that need duplicate-spawn prevention use this.

- [ ] **Step 1: Write the failing test**

Append to `crates/infigraph-core/src/watch/task.rs`, above the existing `mod tests` block (as new top-level items) or inline within it — place the implementation above `#[cfg(test)] mod tests` and add these test functions inside that same `mod tests` block:

```rust
use std::collections::HashSet;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

pub struct TaskRegistry<K> {
    active: Arc<Mutex<HashSet<K>>>,
}

impl<K> Default for TaskRegistry<K> {
    fn default() -> Self {
        TaskRegistry {
            active: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

impl<K: Eq + Hash + Clone + Send + 'static> TaskRegistry<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically claim `key`. Returns `None` if it's already claimed --
    /// the caller should treat that as "busy, decline to spawn a
    /// duplicate" (mirrors `try_start_full_reindex`'s
    /// `if drain_in_flight || full_reindex_in_flight { return None; }`,
    /// generalized).
    pub fn try_claim(&self, key: K) -> Option<Claim<K>> {
        let mut active = self.active.lock().unwrap();
        if active.contains(&key) {
            None
        } else {
            active.insert(key.clone());
            Some(Claim {
                key,
                active: Arc::clone(&self.active),
            })
        }
    }
}

pub struct Claim<K: Eq + Hash> {
    key: K,
    active: Arc<Mutex<HashSet<K>>>,
}

impl<K: Eq + Hash> Drop for Claim<K> {
    fn drop(&mut self) {
        self.active.lock().unwrap().remove(&self.key);
    }
}
```

and add to `mod tests`:

```rust
    #[test]
    fn try_claim_declines_a_second_claim_on_the_same_key() {
        let registry: TaskRegistry<&str> = TaskRegistry::new();
        let first = registry.try_claim("code").expect("first claim should succeed");
        let second = registry.try_claim("code");
        assert!(second.is_none(), "a live claim on the same key must decline a second one");
        drop(first);
    }

    #[test]
    fn try_claim_allows_a_fresh_claim_after_the_first_drops() {
        let registry: TaskRegistry<&str> = TaskRegistry::new();
        let first = registry.try_claim("code").unwrap();
        drop(first);
        let second = registry.try_claim("code");
        assert!(second.is_some(), "dropping the first claim should free the key for a fresh one");
    }

    #[test]
    fn different_keys_do_not_contend() {
        let registry: TaskRegistry<&str> = TaskRegistry::new();
        let code = registry.try_claim("code").unwrap();
        let docs = registry.try_claim("docs");
        assert!(docs.is_some(), "distinct keys must not contend with each other");
        drop(code);
    }

    #[test]
    fn claim_releases_even_if_the_holder_panics() {
        let registry: TaskRegistry<&str> = TaskRegistry::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _claim = registry.try_claim("code").unwrap();
            panic!("simulated panic while holding the claim");
        }));
        assert!(result.is_err());
        // The Claim's Drop impl runs during unwind, so a fresh claim must
        // now succeed -- this is what prevents an aborted/panicked task
        // from leaving a permanent phantom "still running" entry.
        let fresh = registry.try_claim("code");
        assert!(fresh.is_some(), "a panicking holder must not leak its claim");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p infigraph-core --lib watch::task -- --nocapture`
Expected: FAIL (compile error — `TaskRegistry`/`Claim` not yet defined if you added tests before implementation; if you added both together per Step 1's instructions, this step instead confirms all four new tests plus the five from Task 2 compile and the four new ones would fail before Step 1's implementation existed — in practice, since Step 1 above includes both test and implementation together, run this step against a version of the file with the test functions present but the `TaskRegistry`/`Claim`/`impl` block commented out, to genuinely observe a failure, then uncomment for Step 3).

- [ ] **Step 3: Confirm the implementation from Step 1 is in place, run again**

Run: `cargo test -p infigraph-core --lib watch::task -- --nocapture`
Expected: PASS — 9 tests total (5 from Task 2, 4 new).

- [ ] **Step 4: `cargo fmt` and `clippy`**

Run: `cargo fmt -p infigraph-core -- --check && cargo clippy -p infigraph-core --lib -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/watch/task.rs
git commit -m "feat(core): add TaskRegistry in-process dedup/claim"
```

---

## Phase 2: Full-Reindex Build + SCIP Enrichment → `Task<T>`, Async SCIP Subprocess Spawning

*(Spec: "`Task<T>` for full-reindex and SCIP enrichment — and why drain is excluded"; "Async subprocess spawning for SCIP indexers". `InFlightDrain` and `finish_full_reindex`'s swap phase are untouched — see Global Constraints.)*

### Task 4: Convert `InFlightFullReindex`'s build phase to `Task<FullReindexBuildOutcome>`

**Files:**
- Modify: `crates/infigraph-core/src/watch/mod.rs` (`try_start_full_reindex`, `finish_full_reindex`, `watch_project_with_periodic`'s reap block, the `InFlightFullReindex` struct)
- Test: `crates/infigraph-core/tests/watch_daemon.rs`

**Interfaces:**
- Consumes: `Task<T>` from Task 2 (`crate::watch::task::Task`).
- Produces: `try_start_full_reindex` now returns `Option<Task<FullReindexTaskOutput>>` in place of `Option<InFlightFullReindex>`; a `daemon_token: &CancellationToken` parameter threads through `watch_project_with_periodic`, `try_start_full_reindex`.

- [ ] **Step 1: Write the failing test**

Add to `crates/infigraph-core/tests/watch_daemon.rs` (the file already covers `watch_project_with_periodic`-adjacent scenarios per the earlier `daemon_protocol_watcher_wiring.rs`/`watch_daemon.rs` test suites; place this alongside the existing full-reindex tests such as `out_of_scope_write_request_contends_with_a_held_index_lock`):

```rust
#[test]
fn full_reindex_build_task_can_be_cancelled_before_it_starts_the_swap() {
    // Regression coverage for R2.4.5 (docs/DESIGN-hardening.md): the build
    // phase must be a genuinely cancellable Task<T>, not just a JoinHandle
    // with no token. This test spawns a daemon against a real temp project,
    // submits a WriteRequest::FullReindex, cancels the daemon's token
    // immediately, and confirms:
    //   1. the live graph is untouched (the build phase never reached the
    //      swap -- cancellation before the swap starts is always safe,
    //      per the spec's "build phase ... live graph never touched")
    //   2. no panic, no hang -- the coordinator's existing drain-in-flight
    //      wait-it-out-at-shutdown path still reaps the cancelled task
    //      cleanly.
    let (dir, root) = crate::common::make_project(&[("src/main.py", "def main(): pass")]);
    crate::common::tool_index_project(&root);

    let daemon_token = tokio_util::sync::CancellationToken::new();
    // ... spawn the daemon loop against `daemon_token` per the real
    // integration harness this test file already uses for watch_project_with_periodic
    // (see out_of_scope_write_request_contends_with_a_held_index_lock for the
    // existing pattern: submit a request file, drive one or more loop ticks,
    // assert on the reply file / graph state).
    //
    // Submit a FullReindex request, then cancel daemon_token before the
    // loop has a chance to reap the build task's completion.
    let requests_dir = root.join(".infigraph").join("requests");
    std::fs::create_dir_all(&requests_dir).unwrap();
    let request_path = requests_dir.join("full-reindex-cancel-test.request");
    std::fs::write(&request_path, r#"{"FullReindex":null}"#).unwrap();

    daemon_token.cancel();

    // The rebuilding side-path must never have been swapped in -- confirm
    // the live graph's mtime is unchanged from before the request.
    let live_graph = root.join(".infigraph").join("graph");
    let mtime_before = std::fs::metadata(&live_graph).unwrap().modified().unwrap();
    // (drive the loop briefly here per the harness's existing pattern)
    let mtime_after = std::fs::metadata(&live_graph).unwrap().modified().unwrap();
    assert_eq!(mtime_before, mtime_after, "cancelling before the swap must leave the live graph untouched");

    drop(dir);
}
```

This test's exact harness plumbing (how `watch_daemon.rs`'s existing tests drive `watch_project_with_periodic` against a real temp project and real request files) must be copied from the nearest existing test in that file rather than invented — **before writing the final version of this test, read `out_of_scope_write_request_contends_with_a_held_index_lock` and `watch_triggered_file_removal_contends_with_a_held_index_lock` in full and match their exact setup/drive/assert shape**, since this plan was written without re-reading those two tests' full bodies and the sketch above is illustrative of intent, not the literal final code.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-core --test watch_daemon full_reindex_build_task_can_be_cancelled -- --nocapture`
Expected: FAIL (compile error — `daemon_token` param doesn't exist on `watch_project_with_periodic` yet).

- [ ] **Step 3: Thread `daemon_token` through and convert `InFlightFullReindex`**

In `crates/infigraph-core/src/watch/mod.rs`:

1. Add `use crate::watch::task::Task;` and `use tokio_util::sync::CancellationToken;` near the top.
2. Add a `daemon_token: &CancellationToken` parameter to `watch_project_with_periodic`'s signature (after `on_full_reindex: Option<Arc<FullReindexCallback>>`):

```rust
pub fn watch_project_with_periodic<MR, F>(
    root: &Path,
    make_registry: MR,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    on_event: impl Fn(WatchEvent) + Send + 'static,
    periodic_secs: u64,
    on_periodic: Option<F>,
    serve_requests: bool,
    on_full_reindex: Option<Arc<FullReindexCallback>>,
    daemon_token: &CancellationToken,
) -> Result<()>
```

3. `watch_project` (the thin wrapper) gains its own owned token for the two call sites that don't yet have a real hierarchy (Phase 3 replaces this properly — for now, pass a fresh, unparented token so the signature change compiles and existing callers are unaffected):

```rust
pub fn watch_project<MR>(
    root: &Path,
    make_registry: MR,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    on_event: impl Fn(WatchEvent) + Send + 'static,
) -> Result<()>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry> + Send + 'static,
{
    let token = CancellationToken::new();
    watch_project_with_periodic(
        root,
        make_registry,
        debounce_ms,
        stop_rx,
        on_event,
        0,
        None::<fn(&crate::IndexResult)>,
        false,
        None,
        &token,
    )
}
```

4. Change `try_start_full_reindex`'s signature to take `daemon_token: &CancellationToken` and return `Option<Task<FullReindexTaskOutput>>`:

```rust
fn try_start_full_reindex<MR>(
    root: &Path,
    path: &Path,
    queue: &Arc<Mutex<crate::watch::queue::IndexWorkQueue>>,
    make_registry: &MR,
    drain_in_flight: bool,
    full_reindex_in_flight: bool,
    drain_rt: &tokio::runtime::Runtime,
    daemon_token: &CancellationToken,
) -> Option<Task<FullReindexTaskOutput>>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    // ... unchanged gating logic through the `begin_index_op` call ...

    let root_buf = root.to_path_buf();
    let task = Task::spawn_blocking(daemon_token, "full-reindex-build", move |token| {
        // `token.is_cancelled()` is checked inside `build_full_reindex` at
        // its existing natural checkpoint (see Step 4 below) -- abandoning
        // the build is always safe since the live graph is never touched
        // by this phase.
        FullReindexTaskOutput {
            result: build_full_reindex(&root_buf, registry, &token),
            guard,
        }
    });

    Some(task)
}
```

(Remove the old `InFlightFullReindex` struct and its three fields `handle`/`request_path`/`reply_path` — fold `request_path`/`reply_path` into the caller's own bookkeeping in `watch_project_with_periodic`'s reap block, since `Task<T>` doesn't carry them. Concretely: `watch_project_with_periodic` needs a small companion struct alongside the `Task`, e.g. `struct PendingFullReindex { task: Task<FullReindexTaskOutput>, request_path: PathBuf, reply_path: PathBuf }`, replacing `full_reindex_in_flight: Option<InFlightFullReindex>` with `full_reindex_in_flight: Option<PendingFullReindex>`.)

5. `build_full_reindex` gains a `token: &CancellationToken` parameter and one checkpoint — after the (cheap, always-safe-to-finish) cleanup step, before the (expensive) Kuzu open+scan+upsert+resolve sequence:

```rust
fn build_full_reindex(
    root: &Path,
    registry: crate::lang::LanguageRegistry,
    token: &CancellationToken,
) -> Result<FullReindexBuildOutcome> {
    const REBUILDING_NAME: &str = "graph.rebuilding";
    let rebuilding_path = root.join(".infigraph").join(REBUILDING_NAME);

    let _ = std::fs::remove_dir_all(&rebuilding_path);
    let _ = std::fs::remove_file(&rebuilding_path);
    crate::graph::remove_wal_family(&rebuilding_path);

    if token.is_cancelled() {
        return Err(anyhow::anyhow!("full reindex build cancelled before starting"));
    }

    let build_result = Infigraph::open_local_kuzu_at(root, registry, rebuilding_path.clone())
        .and_then(|fresh| {
            // ... unchanged body ...
        });

    if build_result.is_err() {
        let _ = std::fs::remove_dir_all(&rebuilding_path);
        let _ = std::fs::remove_file(&rebuilding_path);
        crate::graph::remove_wal_family(&rebuilding_path);
    }

    build_result
}
```

(A cancellation-triggered `Err` here flows through `finish_full_reindex`'s existing `Err(e)` arm exactly like any other build failure — no new error handling needed there, since `finish_full_reindex` already treats "the build task returned an error" as "reply with an error, leave the live graph untouched," which is precisely correct for a cancelled build too.)

6. Update `watch_project_with_periodic`'s call sites to pass `daemon_token`, and its reap block to use `Task<T>`'s `is_finished()`/`join()` via `drain_rt.block_on(...)` in place of the old `drain_in_flight`-style field access (the reap logic's *shape* — check `is_finished()`, then `drain_rt.block_on(task.join())`, then call `finish_full_reindex` — stays the same; only the type changes from `InFlightFullReindex`'s bare `handle` field to `Task<T>`'s `join()` method).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p infigraph-core --test watch_daemon full_reindex_build_task_can_be_cancelled -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run the full existing full-reindex test suite to confirm no regression**

Run: `cargo test -p infigraph-core --test watch_daemon -- --test-threads=1`
Expected: PASS — every pre-existing test in this file (per this repo's CLAUDE.md guidance, always confirm with `--test-threads=1` before treating a failure as real, since this suite is prone to resource-contention flakiness under high parallelism).

- [ ] **Step 6: `cargo fmt`, `clippy`, commit**

```bash
cargo fmt -p infigraph-core -- --check
cargo clippy -p infigraph-core --lib --tests -- -D warnings
git add crates/infigraph-core/src/watch/mod.rs crates/infigraph-core/tests/watch_daemon.rs
git commit -m "feat(core): full-reindex build phase becomes a cancellable Task<T>"
```

### Task 5: Convert `InFlightScip` to `Task<()>` (or its real output type)

**Files:**
- Modify: `crates/infigraph-core/src/watch/mod.rs` (the `scip_in_flight` reap block in `watch_project_with_periodic`, `InFlightScip`'s replacement)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`cmd_daemon`'s `on_full_reindex` closure, which is what actually spawns the SCIP-enrichment work today via `drain_rt.spawn_blocking`)

**Interfaces:**
- Consumes: `Task<T>` from Task 2.
- Produces: `scip_in_flight: Option<Task<()>>` in `watch_project_with_periodic`, replacing `Option<InFlightScip>`.

- [ ] **Step 1: Write the failing test**

Add to `crates/infigraph-core/tests/watch_daemon.rs`:

```rust
#[test]
fn scip_enrichment_task_is_cancellable_via_daemon_token() {
    // Mirrors full_reindex_build_task_can_be_cancelled_before_it_starts_the_swap
    // but for the SCIP-enrichment Task<T> that on_full_reindex spawns.
    // Confirms: cancelling daemon_token before SCIP enrichment's Kuzu-import
    // checkpoint leaves the graph's scip_generation counter unchanged (no
    // partial/half-applied enrichment).
    //
    // Exact harness plumbing: copy from an existing SCIP-enrichment-adjacent
    // test in this file (search for "on_full_reindex" and "scip_in_flight"
    // usages in the existing test suite before finalizing this test's body).
    todo!("copy exact harness setup from the nearest existing on_full_reindex test in this file before implementing");
}
```

Note: this `todo!()` is a placeholder *only* for the harness-copying step explicitly called out in Task 4's Step 1 note — the executor must replace it with real, concrete test code copied from the nearest existing test before treating this task as started, not leave it as a stub. Do this replacement as the literal first action of Step 1, before running anything.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-core --test watch_daemon scip_enrichment_task_is_cancellable -- --nocapture`
Expected: FAIL — either the `todo!()` panics (if not yet replaced — go back and replace it first) or a genuine compile/assertion failure once real code is in place.

- [ ] **Step 3: Convert the SCIP-enrichment spawn site**

In `crates/infigraph-core/src/watch/mod.rs`, the reap block that today does (per the earlier full read of `watch_project_with_periodic`):

```rust
} else if let (Some(cb), Some(prism)) = (on_full_reindex.clone(), held_prism.clone()) {
    let handle = drain_rt.spawn_blocking(move || {
        cb(prism, languages);
    });
    scip_in_flight = Some(InFlightScip { handle });
}
```

becomes:

```rust
} else if let (Some(cb), Some(prism)) = (on_full_reindex.clone(), held_prism.clone()) {
    let task = Task::spawn_blocking(daemon_token, "scip-enrich", move |_token| {
        cb(prism, languages);
    });
    scip_in_flight = Some(task);
}
```

and its reap arm:

```rust
if scip_in_flight
    .as_ref()
    .is_some_and(|s| s.is_finished())
{
    let task = scip_in_flight.take().expect("checked is_some just above");
    if let Err(join_err) = drain_rt.block_on(task.join()) {
        eprintln!("[watch] scip-enrich task panicked: {join_err}");
    }
}
```

`scip_in_flight`'s field type changes from `Option<InFlightScip>` to `Option<Task<()>>`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p infigraph-core --test watch_daemon scip_enrichment_task_is_cancellable -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run the full suite, `cargo fmt`, `clippy`, commit**

```bash
cargo test -p infigraph-core --test watch_daemon -- --test-threads=1
cargo fmt -p infigraph-core -- --check
cargo clippy -p infigraph-core --lib --tests -- -D warnings
git add crates/infigraph-core/src/watch/mod.rs crates/infigraph-core/tests/watch_daemon.rs
git commit -m "feat(core): SCIP enrichment becomes a cancellable Task<T>"
```

### Task 6: `run_scip_indexer_cmd` onto `tokio::process::Command` + timeout

**Files:**
- Modify: `crates/infigraph-cli/src/index.rs` (`run_scip_indexer_cmd`, `run_scip_indexer_to`, `run_scip_indexers`)
- Modify: `crates/infigraph-cli/Cargo.toml` (confirm `tokio` is already a dependency with `process`/`rt`/`time` features — if not, add it matching infigraph-core's Task 1 additions)

**Interfaces:**
- Consumes: nothing from earlier tasks in this plan (independent of `Task<T>` — this is a separate, self-contained improvement to a leaf function).
- Produces: `run_scip_indexer_cmd` becomes `async fn`, returning the same `bool` (success) it does today; callers up the chain (`run_scip_indexer_to`, `run_scip_indexers`) become async and run on a small local tokio runtime instead of `std::thread::scope`.

- [ ] **Step 0: Confirm `infigraph-cli`'s existing tokio setup**

Before writing code, check `crates/infigraph-cli/Cargo.toml` for its current `tokio` dependency line (this plan was written without re-reading it — confirm the exact current features present, since `infigraph-cli` may already depend on tokio independently of `infigraph-core`, possibly with a different feature set that needs widening rather than adding fresh).

- [ ] **Step 1: Write the failing test**

Add to `crates/infigraph-cli/src/index.rs`'s existing `#[cfg(test)] mod tests` block (the file already has one, per `scip_enrich_exit_message_warns_on_nonzero_exit`'s location):

```rust
    #[tokio::test]
    async fn run_scip_indexer_cmd_async_reports_success_and_failure_like_today() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let output_path = root.join("out.scip");

        // A command that succeeds and writes nothing meaningful -- exercises
        // the `output_flag.is_none()` default-rename branch is NOT hit here
        // since we pass an explicit flag-less no-op; assert only on the
        // success/failure signal, matching today's `run_scip_indexer_cmd`
        // contract (`Ok(s) if s.success() => output_path.exists()` -- a
        // command with no output_flag and no default index.scip produced
        // returns false here correctly, since output_path never gets created).
        let succeeded = run_scip_indexer_cmd_async(
            root,
            if cfg!(windows) { "cmd" } else { "true" },
            if cfg!(windows) { &["/C", "exit", "0"] } else { &[] },
            "test-indexer",
            None,
            None,
            &output_path,
            std::time::Duration::from_secs(5),
        )
        .await;
        assert!(!succeeded, "no output_flag and no index.scip produced means false, matching today's contract");

        let failing_succeeded = run_scip_indexer_cmd_async(
            root,
            if cfg!(windows) { "cmd" } else { "false" },
            if cfg!(windows) { &["/C", "exit", "1"] } else { &[] },
            "test-indexer",
            None,
            None,
            &output_path,
            std::time::Duration::from_secs(5),
        )
        .await;
        assert!(!failing_succeeded);
    }

    #[tokio::test]
    async fn run_scip_indexer_cmd_async_times_out_a_hung_process() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let output_path = root.join("out.scip");

        // `sleep 5` with a 200ms timeout -- must return false (or otherwise
        // signal failure) within roughly the timeout, not wait the full 5s.
        // This is new behavior: nothing today bounds a hung SCIP indexer at all.
        let start = std::time::Instant::now();
        let succeeded = run_scip_indexer_cmd_async(
            root,
            if cfg!(windows) { "cmd" } else { "sleep" },
            if cfg!(windows) { &["/C", "timeout", "/T", "5"] } else { &["5"] },
            "hung-indexer",
            None,
            None,
            &output_path,
            std::time::Duration::from_millis(200),
        )
        .await;
        let elapsed = start.elapsed();
        assert!(!succeeded, "a timed-out indexer must report failure");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "must return promptly on timeout, not wait for the full process duration; took {elapsed:?}"
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-cli run_scip_indexer_cmd_async -- --nocapture`
Expected: FAIL (compile error — `run_scip_indexer_cmd_async` doesn't exist yet).

- [ ] **Step 3: Implement `run_scip_indexer_cmd_async`**

In `crates/infigraph-cli/src/index.rs`, add alongside (don't yet delete) the existing synchronous `run_scip_indexer_cmd`:

```rust
async fn run_scip_indexer_cmd_async(
    root: &Path,
    cmd: &str,
    args: &[&str],
    label: &str,
    extra_path: Option<&str>,
    output_flag: Option<&str>,
    output_path: &Path,
    timeout: std::time::Duration,
) -> bool {
    let mut command = tokio::process::Command::new(cmd);
    command.args(args).current_dir(root);

    if let Some(flag) = output_flag {
        command.arg(flag).arg(output_path);
    }

    if let Some(extra) = extra_path {
        let path = std::env::var("PATH").unwrap_or_default();
        let sep = if cfg!(windows) { ";" } else { ":" };
        command.env("PATH", format!("{extra}{sep}{path}"));
    }

    {
        let ig = crate::scip_download::infigraph_dir();
        let java_macos = ig.join("java").join("Contents").join("Home");
        if java_macos.exists() {
            command.env("JAVA_HOME", &java_macos);
        } else {
            let java_home = ig.join("java");
            if java_home.join("bin").exists() {
                command.env("JAVA_HOME", &java_home);
            }
        }
        let dotnet_root = ig.join("dotnet");
        if dotnet_root.exists() {
            command.env("DOTNET_ROOT", &dotnet_root);
        }
    }

    let run = async {
        match command.status().await {
            Ok(s) if s.success() => {
                if output_flag.is_none() {
                    let default_out = root.join("index.scip");
                    if default_out.exists() && default_out != output_path {
                        let _ = std::fs::rename(&default_out, output_path);
                    }
                }
                output_path.exists()
            }
            Ok(s) => {
                eprintln!("Auto-SCIP: {label} exited with {s}");
                false
            }
            Err(e) => {
                eprintln!("Auto-SCIP: failed to run {label}: {e}");
                false
            }
        }
    };

    match tokio::time::timeout(timeout, run).await {
        Ok(succeeded) => succeeded,
        Err(_elapsed) => {
            eprintln!("Auto-SCIP: {label} timed out after {timeout:?}");
            false
        }
    }
}
```

(Kept as a genuinely new, separate function rather than an in-place rewrite of `run_scip_indexer_cmd` for this step, so the existing synchronous callers — `run_scip_java`, if it calls the sync version too — keep compiling while this task focuses narrowly on the new function's own correctness. Wiring callers onto it is Step 5.)

Note the added `timeout` parameter — today's call chain has no timeout concept anywhere (confirmed in the spec-correction commit `6afa15f`); pick a concrete default in Step 5's wiring (e.g. a `SCIP_INDEXER_TIMEOUT` constant — suggest 10 minutes, generous enough for `rust-analyzer`'s cold-start `cargo metadata` resolution the existing code comments call out as slow, but bounded rather than infinite).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p infigraph-cli run_scip_indexer_cmd_async -- --nocapture`
Expected: PASS (both tests).

- [ ] **Step 5: Wire callers onto the async version, run indexers as tokio tasks instead of `std::thread::scope`**

Update `run_scip_indexer_to` to `async fn`, calling `run_scip_indexer_cmd_async` (define a `SCIP_INDEXER_TIMEOUT: Duration` constant near the top of the file, e.g. `const SCIP_INDEXER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);`, and pass it through). Update `run_scip_indexers` to spawn `tokio::task::spawn` tasks instead of `std::thread::scope`'s `s.spawn`, awaiting all of them via `futures::future::join_all` or a manual loop over `JoinHandle`s (confirm whether `futures`/`futures-util` is already a dependency before reaching for `join_all` — if not, a plain `for handle in handles { results.push(handle.await.unwrap()); }` loop is simpler and adds no new dependency). Since `run_scip_indexers` itself is called from `cmd_daemon`'s `on_full_reindex` closure (already running inside a `Task::spawn_blocking` context per Task 5, i.e. NOT already on an async runtime), it needs its own small runtime to drive the now-async work — add a `tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(...)` wrapper at `run_scip_indexers`' own boundary, keeping its outward signature (`fn run_scip_indexers(...) -> Vec<(&'static str, PathBuf, bool)>`, synchronous) unchanged so `cmd_daemon`'s existing call site (`crate::index::run_scip_indexers(&root, &languages)`) needs no changes at all.

Also update `run_auto_scip_on`'s two call sites (the other caller of `run_scip_indexers`, per the earlier callers list) — since `run_scip_indexers`' outward signature doesn't change, this should be a no-op for that caller.

- [ ] **Step 6: Delete the now-dead synchronous `run_scip_indexer_cmd`**

Confirm no remaining callers (`run_scip_java` may also need updating to call the async version — check its body before deleting the sync one; if `run_scip_java` isn't touched by this task's scope, leave the sync `run_scip_indexer_cmd` in place for it and only route the non-Java path through the async version, noting this as a followup rather than silently leaving inconsistent behavior unexplained).

- [ ] **Step 7: Run the full existing SCIP-related test suite**

Run: `cargo test -p infigraph-cli scip -- --test-threads=1`
Expected: PASS, including the pre-existing `scip_enrich_exit_message_warns_on_nonzero_exit` (unaffected — different function) and `test_indexers_dedup` (in `scip_download.rs`, also unaffected).

- [ ] **Step 8: `cargo fmt`, `clippy`, commit**

```bash
cargo fmt -p infigraph-cli -- --check
cargo clippy -p infigraph-cli --lib --tests -- -D warnings
git add crates/infigraph-cli/src/index.rs crates/infigraph-cli/Cargo.toml
git commit -m "feat(cli): SCIP indexer subprocess spawning onto tokio::process, with a real timeout"
```

---

## Phase 3: Producer Split — `Task<()>` for Code/Docs Watching, `WriteRequest::WatchControl`, `route_or_serve_request` Extension

*(Spec: "Where producer tasks run, and what they wrap"; "Crossing the process boundary"; "Cancellation hierarchy". This is the core behavioral unlock — the daemon can survive `watch stop` without exiting. Highest-risk phase; budget real iteration time here.)*

**Before starting this phase**, re-read `watch_project_with_periodic`'s full current body (`crates/infigraph-core/src/watch/mod.rs`, ~580 lines) and `cmd_daemon`'s full current body (`crates/infigraph-cli/src/info_commands.rs`) in full — this plan's earlier phases already modified both files, so the line numbers and exact surrounding context cited in the original spec conversation are now stale. Also re-read `crates/infigraph-mcp/src/tools/watch.rs::tool_watch_project` in full (already partially captured above) — note it has **three** watch-loop variants, not two: `watch_project` (plain), `watch_project_auto_resolve` (when `auto_resolve=true`), and the daemon-delegation path (`ensure_daemon_watcher`, no local task at all). The spec's "MCP wraps producer + local coordination" framing needs to account for `watch_project_auto_resolve` too — confirm its signature and behavior before designing Task 8's producer extraction, since it may need its own `Task<()>`-wrapping treatment distinct from plain `watch_project`.

### Task 7: `WriteRequest::WatchControl` variant

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs`

**Interfaces:**
- Produces: `WriteRequest::WatchControl { role: WatchRole, action: WatchAction }` variant; `pub enum WatchRole { Code, Docs, Daemon }`; `pub enum WatchAction { Start, Stop, Enable, Disable, Restart }` (both new pub enums in `daemon_protocol.rs`, `derive(Debug, Clone, Serialize, Deserialize, PartialEq)` matching the existing enum's derives — confirm `WriteRequest`'s exact derive list before matching it, since this plan's earlier read of the enum didn't capture its derive attributes).

- [ ] **Step 1: Write the failing test**

Add a test to `crates/infigraph-core/src/daemon_protocol.rs`'s existing test module (confirm one exists; if not, add `#[cfg(test)] mod tests` near the bottom of the file):

```rust
    #[test]
    fn watch_control_request_round_trips_through_json() {
        let req = WriteRequest::WatchControl {
            role: WatchRole::Code,
            action: WatchAction::Stop,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: WriteRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, parsed);
    }

    #[test]
    fn watch_control_covers_all_role_action_combinations_without_panicking_on_serialize() {
        for role in [WatchRole::Code, WatchRole::Docs, WatchRole::Daemon] {
            for action in [
                WatchAction::Start,
                WatchAction::Stop,
                WatchAction::Enable,
                WatchAction::Disable,
                WatchAction::Restart,
            ] {
                let req = WriteRequest::WatchControl { role, action };
                let json = serde_json::to_string(&req).unwrap();
                let _: WriteRequest = serde_json::from_str(&json).unwrap();
            }
        }
    }
```

(`WatchRole`/`WatchAction` need `Copy` for the `for role in [...]` loop to work by value across both loops as written — add `Copy` to their derive list alongside `Clone`.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-core --lib daemon_protocol::tests::watch_control -- --nocapture`
Expected: FAIL (compile error).

- [ ] **Step 3: Add the variant and enums**

In `crates/infigraph-core/src/daemon_protocol.rs`, near the top (alongside other small enums/types the file defines), add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchRole {
    Code,
    Docs,
    /// Full-process stop/restart -- replaces the undecorated `watch.stop`
    /// sentinel's role (see docs/superpowers/specs/2026-08-21-daemon-watch-
    /// command-split-design.md, "Crossing the process boundary").
    Daemon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchAction {
    Start,
    Stop,
    Enable,
    Disable,
    Restart,
}
```

Add the variant to `WriteRequest` (match the enum's exact existing derive attributes — copy them verbatim from the line directly above `pub enum WriteRequest {`):

```rust
    /// Control a watch-activity's lifecycle (code-watching, doc-watching,
    /// or the whole daemon process) from outside the process that owns it.
    /// See docs/superpowers/specs/2026-08-21-daemon-watch-command-split-design.md
    /// "Crossing the process boundary: WatchControl requests bridge into
    /// the token hierarchy" -- CancellationToken is in-process-only, this
    /// is how an external CLI invocation or MCP tool call reaches an
    /// already-running daemon's tokens.
    WatchControl { role: WatchRole, action: WatchAction },
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p infigraph-core --lib daemon_protocol::tests::watch_control -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run the full daemon_protocol test suite, `cargo fmt`, `clippy`, commit**

```bash
cargo test -p infigraph-core --lib daemon_protocol
cargo fmt -p infigraph-core -- --check
cargo clippy -p infigraph-core --lib -- -D warnings
git add crates/infigraph-core/src/daemon_protocol.rs
git commit -m "feat(core): add WriteRequest::WatchControl variant"
```

### Task 8: Extract the fsevent-watching producer into `watch/producer.rs`

**Files:**
- Create: `crates/infigraph-core/src/watch/producer.rs`
- Modify: `crates/infigraph-core/src/watch/mod.rs` (`pub mod producer;`, remove the extracted logic from `watch_project_with_periodic`)

**Interfaces:**
- Consumes: `Task<T>` (Task 2), `IndexWorkQueue` (existing, `crate::watch::queue`).
- Produces: `pub async fn run_producer(root: PathBuf, queue: Arc<Mutex<IndexWorkQueue>>, on_event: impl Fn(WatchEvent) + Send + 'static, token: CancellationToken) -> ()` — the extracted fsevent-watch loop, spawnable via `Task::spawn(parent, role, |token| producer::run_producer(root, queue, on_event, token))`.

This is the largest, highest-risk single task in this plan — it moves real, currently-working logic (fsevent registration, restart-with-backoff, dirty-marking, batch accumulation/flush) out of a 580-line function into a new async context, while the remaining coordinator logic must keep working against the exact same `queue`.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/tests/producer.rs`:

```rust
//! Integration coverage for the extracted fsevent-watching producer
//! (crates/infigraph-core/src/watch/producer.rs). Verifies it correctly
//! feeds IndexWorkQueue on real filesystem events and stops cleanly on
//! cancellation, WITHOUT any coordinator/drain logic running alongside it
//! -- that's the whole point of the split.

use infigraph_core::watch::queue::IndexWorkQueue;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn producer_feeds_the_queue_on_a_real_file_change() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::fs::write(root.join("main.py"), "def main(): pass").unwrap();

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();

    let queue_clone = Arc::clone(&queue);
    let root_clone = root.clone();
    let token_clone = token.clone();
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(
            root_clone,
            queue_clone,
            |_evt| {},
            token_clone,
        )
        .await;
    });

    // Give the watcher time to register, then trigger a real fsevent.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    std::fs::write(root.join("main.py"), "def main(): return 1").unwrap();

    // Poll for the queue to reflect the change (batching has its own
    // internal debounce window, so this needs a generous poll budget).
    let mut saw_queued_work = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if !queue.lock().unwrap().is_empty() {
            saw_queued_work = true;
            break;
        }
    }
    assert!(saw_queued_work, "producer should have marked main.py dirty and queued it");

    token.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn producer_exits_promptly_on_cancellation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();

    let queue_clone = Arc::clone(&queue);
    let root_clone = root.clone();
    let token_clone = token.clone();
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(root_clone, queue_clone, |_evt| {}, token_clone).await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let start = std::time::Instant::now();
    token.cancel();
    handle.await.unwrap();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "cancellation should be near-instant (event-driven select!, not a poll interval)"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-core --test producer -- --nocapture`
Expected: FAIL (compile error — `infigraph_core::watch::producer` doesn't exist yet).

- [ ] **Step 3: Write `producer.rs`, extracting the fsevent-handling logic**

This step requires copying real logic out of `watch_project_with_periodic`'s current body (as it stands after Phase 2's edits — re-read it first, per this phase's opening note). The pieces to extract, verbatim in behavior:

- `create_watcher` closure (notify::Watcher setup)
- The `MAX_RESTARTS`/backoff restart-on-`Disconnected` logic
- `ignore_matcher`/`last_ignore_rebuild` state and its periodic rebuild
- The `rx.recv_timeout(...)` event-handling match arm's body (dirty-marking via `crate::dirty::mark_dirty`, `batch.add`, `queue.lock().unwrap().add_watch_removal(...)`, the `on_event` callback calls)
- `batch`/`ChangeBatch` accumulation and its flush-into-queue call

Structure:

```rust
//! The fsevent-watching producer: owns a `notify::Watcher`, marks dirty
//! paths, and feeds `IndexWorkQueue` -- nothing else. Never touches Kuzu,
//! `held_prism`, or drain-scheduling state; see
//! docs/superpowers/specs/2026-08-21-daemon-watch-command-split-design.md
//! "What a code/docs `Task<()>` never touches directly".

use crate::watch::queue::IndexWorkQueue;
use crate::watch::{WatchEvent, WatchEventKind};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_RESTARTS: u32 = 3;

pub async fn run_producer(
    root: PathBuf,
    queue: Arc<Mutex<IndexWorkQueue>>,
    on_event: impl Fn(WatchEvent) + Send + 'static,
    token: CancellationToken,
) {
    let root = root.canonicalize().unwrap_or(root);
    let infigraph_dir = root.join(".infigraph");

    let mut ignore_matcher = crate::ignore_rules::IgnoreMatcher::build(&root);
    let mut ignore_rebuild_interval = tokio::time::interval(Duration::from_secs(300));

    // Bridge notify's std::sync::mpsc callback into a tokio channel so the
    // fsevent wait is a real `.await` arm inside `tokio::select!` below,
    // rather than a polled `recv_timeout`.
    let (tokio_tx, mut tokio_rx) = tokio::sync::mpsc::unbounded_channel::<notify::Result<Event>>();

    let create_watcher = |root: &Path,
                           tx: tokio::sync::mpsc::UnboundedSender<notify::Result<Event>>|
     -> anyhow::Result<RecommendedWatcher> {
        let (std_tx, std_rx) = std_mpsc::channel::<notify::Result<Event>>();
        let config = Config::default();
        let mut watcher = RecommendedWatcher::new(std_tx, config)?;
        crate::watch::register_watch_dirs(&mut watcher, root)?;
        // Bridge the std channel onto the tokio one via a dedicated
        // blocking thread -- notify's own callback API is synchronous, so
        // this thread is the adapter, not a workaround.
        std::thread::spawn(move || {
            for evt in std_rx {
                if tx.send(evt).is_err() {
                    break;
                }
            }
        });
        Ok(watcher)
    };

    let mut watcher = match create_watcher(&root, tokio_tx.clone()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[watch-producer] failed to start watcher: {e}");
            return;
        }
    };
    let mut restart_count: u32 = 0;
    let mut batch = crate::watch::batch::ChangeBatch::new(1000);
    let storm_threshold: usize = std::env::var("INFIGRAPH_STORM_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);

    loop {
        tokio::select! {
            _ = token.cancelled() => {
                break;
            }
            _ = ignore_rebuild_interval.tick() => {
                ignore_matcher = crate::ignore_rules::IgnoreMatcher::build(&root);
            }
            maybe_evt = tokio_rx.recv() => {
                match maybe_evt {
                    Some(Ok(event)) => {
                        let watch_kind = match event.kind {
                            EventKind::Create(_) => WatchEventKind::Created,
                            EventKind::Modify(_) => WatchEventKind::Modified,
                            EventKind::Remove(_) => WatchEventKind::Removed,
                            _ => continue,
                        };
                        for path in event.paths {
                            if ignore_matcher.is_ignored(&path, path.is_dir()) {
                                continue;
                            }
                            let rel = match path.strip_prefix(&root) {
                                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                                Err(_) => continue,
                            };
                            match watch_kind {
                                WatchEventKind::Removed => {
                                    if let Err(e) = crate::dirty::mark_dirty(&infigraph_dir, std::slice::from_ref(&rel)) {
                                        eprintln!("[watch-producer] failed to persist dirty mark for {rel}: {e}");
                                    }
                                    queue.lock().unwrap().add_watch_removal(rel);
                                    on_event(WatchEvent {
                                        kind: watch_kind.clone(),
                                        path,
                                        has_cross_file_calls: false,
                                    });
                                }
                                WatchEventKind::Created | WatchEventKind::Modified => {
                                    if path.is_dir() {
                                        let _ = crate::watch::register_watch_dirs(&mut watcher, &path);
                                    } else {
                                        if let Err(e) = crate::dirty::mark_dirty(&infigraph_dir, std::slice::from_ref(&rel)) {
                                            eprintln!("[watch-producer] failed to persist dirty mark for {rel}: {e}");
                                        }
                                        batch.add(path);
                                    }
                                }
                                WatchEventKind::WatcherRestarted | WatchEventKind::WatcherDied => {}
                            }
                        }
                        if !batch.is_empty() && batch.is_ready() {
                            let paths = batch.drain();
                            let mut q = queue.lock().unwrap();
                            crate::watch::flush_batch_into_queue(&mut q, paths, &root, storm_threshold);
                        }
                    }
                    Some(Err(e)) => eprintln!("[watch-producer] watch error: {e}"),
                    None => {
                        restart_count += 1;
                        if restart_count > MAX_RESTARTS {
                            eprintln!("[watch-producer] watcher died {restart_count} times, giving up");
                            on_event(WatchEvent {
                                kind: WatchEventKind::WatcherDied,
                                path: root.clone(),
                                has_cross_file_calls: false,
                            });
                            break;
                        }
                        let backoff = Duration::from_secs(restart_count as u64);
                        eprintln!("[watch-producer] watcher disconnected, restarting ({restart_count}/{MAX_RESTARTS}) after {}s", backoff.as_secs());
                        tokio::time::sleep(backoff).await;
                        match create_watcher(&root, tokio_tx.clone()) {
                            Ok(new_watcher) => {
                                watcher = new_watcher;
                                on_event(WatchEvent {
                                    kind: WatchEventKind::WatcherRestarted,
                                    path: root.clone(),
                                    has_cross_file_calls: false,
                                });
                            }
                            Err(e) => {
                                eprintln!("[watch-producer] watcher restart failed: {e}");
                                on_event(WatchEvent {
                                    kind: WatchEventKind::WatcherDied,
                                    path: root.clone(),
                                    has_cross_file_calls: false,
                                });
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}
```

**Known gaps this sketch leaves for the implementer to close, called out explicitly rather than silently glossed over:**
- `register_watch_dirs`, `flush_batch_into_queue`, `ChangeBatch`, `IgnoreMatcher` are currently private (`fn`, not `pub fn`) inside `watch/mod.rs` per this plan's earlier reads — this task must change their visibility to `pub(crate)` (not full `pub`, since they're internal plumbing, not part of `infigraph-core`'s public API) so `producer.rs` can call them as a sibling module.
- The root-existence-check (`if !root.exists() { ... break; }`) and the `.infigraph/requests/`-scoped `WatchControl` detection (Task 9) both belong in this same `tokio::select!` — this sketch omits the root-existence check for brevity; add it as a `tokio::time::interval`-driven branch (reusing the existing 200ms-equivalent cadence conceptually, though the whole point of this rewrite is to avoid needing a fixed poll for most things — a slower interval, e.g. every 2s, is appropriate for a check this cheap and this rarely meaningful).
- `R3.3.5`'s startup dirty-recovery (`crate::dirty::pending_dirty`) currently runs once at the top of `watch_project_with_periodic`, before the loop — this producer needs the equivalent recovery step at its own startup, since it's now the sole owner of dirty-marking for fsevent-triggered work.

- [ ] **Step 4: Register the module, adjust visibility**

In `crates/infigraph-core/src/watch/mod.rs`: add `pub mod producer;`; change `register_watch_dirs`, `flush_batch_into_queue` to `pub(crate) fn`; confirm `ChangeBatch` (in `watch/batch.rs`) and `IgnoreMatcher` (in `ignore_rules.rs`) are already `pub`/`pub(crate)` enough to use from `producer.rs` (they likely already are, since `watch_project_with_periodic` itself uses them from the sibling `mod.rs` file — `pub(crate)` should suffice; adjust only if the compiler disagrees).

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p infigraph-core --test producer -- --nocapture`
Expected: PASS (both tests). If `producer_feeds_the_queue_on_a_real_file_change` is flaky, increase its poll budget rather than treating it as a real failure first — fsevent delivery timing varies by platform/CI load.

- [ ] **Step 6: `cargo fmt`, `clippy`, commit**

```bash
cargo fmt -p infigraph-core -- --check
cargo clippy -p infigraph-core --lib --tests -- -D warnings
git add crates/infigraph-core/src/watch/producer.rs crates/infigraph-core/src/watch/mod.rs crates/infigraph-core/tests/producer.rs
git commit -m "feat(core): extract fsevent-watching into an async producer"
```

**Do not yet remove the fsevent-handling logic from `watch_project_with_periodic` itself** — that removal, and rewiring `watch_project_with_periodic`'s callers onto the new producer, is Task 10/11. Landing `producer.rs` as an additively-tested new module first (this task) de-risks the larger rewiring task that follows.

### Task 9: `route_or_serve_request` handles `WatchControl`

**Files:**
- Modify: `crates/infigraph-core/src/watch/mod.rs` (`route_or_serve_request`, `watch_project_with_periodic`'s call site)

**Interfaces:**
- Consumes: `WriteRequest::WatchControl` (Task 7), `Task<()>` handles for code/docs producers (Task 10 will make these real — this task can be written against a temporary/stub signature and re-verified once Task 10 lands real producer state).
- Produces: `route_or_serve_request` gains a new match arm; its signature grows a parameter carrying mutable access to the coordinator's producer task slots.

- [x] **Step 1: Write the failing test**

This is genuinely dependent on Task 10's `daemon_token`/`code_task`/`docs_task` state existing on the coordinator — **write this task's test as part of Task 10's Step 1 instead of separately**, to avoid a test that can't compile against real state. Skip ahead: implement Task 9's `route_or_serve_request` match arm as part of Task 10, described there. (This task's own section exists to flag the `WriteRequest`/dispatch change as its own reviewable unit in the plan's structure, per the writing-plans skill's "one action, one reviewer's gate" guidance — but its actual code lands together with Task 10 since they share one signature change. Mark this task's checkbox complete only once Task 10 is done and this file's diff includes the `WatchControl` match arm.)

- [x] **Step 2: (moved to Task 10)**
- [x] **Step 3: (moved to Task 10)**

### Task 10: Rewire `cmd_daemon` and `watch_project_with_periodic` onto `daemon_token`/`code_task`/`docs_task`

**Files:**
- Modify: `crates/infigraph-core/src/watch/mod.rs` (rename the function per the spec's naming note; remove extracted producer logic; add `route_or_serve_request`'s `WatchControl` arm; add the `.infigraph/requests/`-scoped `notify::Watcher` for event-driven request detection)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`cmd_daemon`: own `daemon_token`, spawn `code_task`/`docs_task` via `Task::spawn` against `producer::run_producer`, spawn `watch_rt` as a dedicated small runtime for them)

**Interfaces:**
- Consumes: `Task<T>` (Task 2), `producer::run_producer` (Task 8), `WriteRequest::WatchControl` (Task 7).
- Produces: the renamed coordinator function's new signature (drop `on_event`'s fsevent-specific usage, since that's now the producer's job — the coordinator still needs a *narrower* `on_event` for its own periodic/drain-outcome events); `cmd_daemon`'s new internal shape (owns `daemon_token: CancellationToken`, `watch_rt: tokio::runtime::Runtime`, `code_task: Option<Task<()>>`, `docs_task: Option<Task<()>>`).

This task is the load-bearing rewrite of this whole plan. Given its size, break it into sub-steps that each compile and test independently rather than one giant diff.

- [x] **Step 1: Rename the coordinator function**

Per the spec's naming note: `watch_project_with_periodic` → `run_write_coordinator` (concrete name chosen here, matching the spec's suggested `run_daemon_write_coordinator` shortened for readability — confirm this doesn't collide with an existing identifier before committing to it). Do this as its own mechanical, behavior-preserving rename first:

Run a project-wide rename via the `lsp-refactor` skill (per this session's tool list, `lsp-refactor:lsp-refactor` is available and is the correct tool for a project-wide rename — it catches every reference, including doc comments and test code, that a hand-edit would miss) rather than hand-editing each call site. Invoke it for renaming `watch_project_with_periodic` to `run_write_coordinator` across the whole workspace.

Run: `cargo build --workspace` and `cargo test -p infigraph-core --lib` after the rename, before touching anything else, to confirm the rename alone didn't break anything.

Commit this rename as its own step before continuing:

```bash
git add -A
git commit -m "refactor(core): rename watch_project_with_periodic -> run_write_coordinator"
```

- [x] **Step 2: Write the failing integration test for the core behavioral unlock**

Create `crates/infigraph-core/tests/watch_control.rs`:

```rust
//! Integration coverage for R2.4.4-R2.4.6 (docs/DESIGN-hardening.md): the
//! daemon must survive a WatchControl { role: Code, action: Stop } request
//! without exiting or losing its ability to serve DaemonKuzu writes.

// (Copy the real-process-spawning harness pattern from
// crates/infigraph-cli/tests/watch_daemon_docs.rs::cmd_watch_daemon_also_indexes_docs_without_restart
// -- that test already spawns a genuine detached `infigraph watch <root>`
// child process and drives it via real request files; this test needs the
// same shape, targeting a real `infigraph daemon` process this time. Read
// that test's full body before writing this one -- it was not re-read in
// full while drafting this plan, so its exact helper functions (e.g. any
// KillOnDrop-style guard, exact binary-resolution logic) must be copied
// verbatim rather than re-invented.)

#[test]
fn daemon_survives_watch_control_stop_and_keeps_serving_writes() {
    // 1. Spawn a real `infigraph daemon` child process against a temp project.
    // 2. Wait for it to become alive (watch.lock).
    // 3. Submit a WriteRequest::WatchControl { role: Code, action: Stop }
    //    request file into .infigraph/requests/.
    // 4. Poll for the reply, confirm success.
    // 5. Confirm the daemon process is STILL ALIVE (its PID still running) --
    //    this is the actual regression test for R2.4.4's whole premise.
    // 6. Submit an Index write request; confirm it still gets served
    //    (write-serving unaffected by the code-watch stop).
    // 7. Modify a file in the project; confirm NO reindex happens (the
    //    producer really did stop, not just log a message).
    // 8. Submit WatchControl { role: Code, action: Start }; modify the file
    //    again; confirm reindexing resumes.
    // 9. Kill the daemon process (KillOnDrop-style cleanup).
    todo!("implement against the real harness pattern from watch_daemon_docs.rs, per the note above")
}
```

- [x] **Step 3: Run to verify it fails**

Run: `cargo test -p infigraph-core --test watch_control -- --nocapture`
Expected: FAIL (the `todo!()` panics — replace it with real code before proceeding; this is the same "don't leave the placeholder in place" instruction as Task 5's Step 1).

- [x] **Step 4: Rewrite `cmd_daemon`**

In `crates/infigraph-cli/src/info_commands.rs`, restructure `cmd_daemon` (full current body already captured earlier in this plan's source conversation — re-read it fresh at implementation time per this phase's opening note) to:

```rust
pub(crate) fn cmd_daemon(root: &Path, debounce: u64) -> Result<()> {
    std::env::remove_var("INFIGRAPH_BACKEND");

    if infigraph_core::watch::daemon::is_remote_backend() {
        println!(
            "File watching is not supported in remote mode (Neo4j backend). \
             Reindexing is triggered via webhooks instead."
        );
        return Ok(());
    }
    let lock_path = root.join(".infigraph").join("watch.lock");
    let _lock = acquire_watch_lock(&lock_path)?;

    println!(
        "Watching {} (debounce {}ms) — Ctrl-C to stop",
        root.display(),
        debounce
    );

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let daemon_token = tokio_util::sync::CancellationToken::new();

    let watchdog_root = root.to_path_buf();
    let watchdog_token = daemon_token.clone();
    ctrlc::set_handler(move || {
        let _ = stop_tx.send(());
        watchdog_token.cancel();
        // ... existing R5.4 shutdown-watchdog thread body, unchanged ...
    })
    .ok();

    // A dedicated small runtime for the code/docs producer Task<()>s --
    // separate from drain_rt (spawn_blocking indexing work only). See
    // spec: "Where producer tasks run, and what they wrap".
    let watch_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("infigraph-watch")
        .enable_all()
        .build()?;

    let queue: std::sync::Arc<std::sync::Mutex<infigraph_core::watch::queue::IndexWorkQueue>> =
        std::sync::Arc::new(std::sync::Mutex::new(infigraph_core::watch::queue::IndexWorkQueue::new()));

    let code_token = daemon_token.child_token();
    let code_root = root.to_path_buf();
    let code_queue = std::sync::Arc::clone(&queue);
    let mut code_task = Some(watch_rt.block_on(async {
        infigraph_core::watch::task::Task::spawn(&code_token, "code", move |token| {
            infigraph_core::watch::producer::run_producer(code_root, code_queue, |evt| {
                println!("[watch] {evt}");
            }, token)
        })
    }));

    let docs_token = daemon_token.child_token();
    let doc_shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let doc_root = root.to_path_buf();
    let doc_shutdown_for_thread = std::sync::Arc::clone(&doc_shutdown_flag);
    let doc_thread = std::thread::spawn(move || {
        if let Err(e) = infigraph_docs::watch::watch_docs_daemon_loop(&doc_root, debounce, doc_shutdown_for_thread) {
            eprintln!("[doc-watch-daemon] error: {e}");
        }
    });
    // Bridge docs_token cancellation onto the existing AtomicBool contract
    // watch_docs_daemon_loop already expects -- docs stays on its existing
    // thread+AtomicBool shape for this plan (not itself converted to
    // Task<()> internals in this pass; only its EXTERNAL control surface
    // unifies onto WatchControl/daemon_token). A small bridging task keeps
    // the two consistent:
    let docs_token_for_bridge = docs_token.clone();
    let doc_shutdown_bridge = std::sync::Arc::clone(&doc_shutdown_flag);
    let bridge_handle = watch_rt.spawn(async move {
        docs_token_for_bridge.cancelled().await;
        doc_shutdown_bridge.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    // ... on_full_reindex closure unchanged from today (Task 5/6 already
    // converted its internals to Task<T>/tokio::process::Command) ...

    infigraph_core::watch::run_write_coordinator(
        root,
        bundled_registry,
        debounce,
        stop_rx,
        queue,
        0,
        None::<fn(&infigraph_core::IndexResult)>,
        true,
        Some(on_full_reindex),
        &daemon_token,
        &mut code_task,
        &code_token,
    )?;

    daemon_token.cancel();
    watch_rt.block_on(async {
        if let Some(task) = code_task.take() {
            task.stop().await;
        }
        let _ = bridge_handle.await;
    });
    doc_shutdown_flag.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = doc_thread.join();

    println!("Watch stopped.");
    Ok(())
}
```

**This sketch has real gaps the implementer must resolve, not silently paper over:**
- `run_write_coordinator`'s exact new signature (it now needs to own/reap `code_task` for `WatchControl` requests targeting it, and needs `queue` passed in from the caller rather than constructing its own, since the producer now shares the same queue instance) — work out the precise parameter list by tracing every place inside the coordinator that currently constructs or owns `queue` today, and move that construction to `cmd_daemon` (as sketched above) instead.
- The docs side is **deliberately left on its existing `Arc<AtomicBool>`/thread shape internally** in this pass, per the sketch's comment — only bridging its *external* control surface onto `daemon_token`/`WatchControl`. Fully converting `watch_docs_daemon_loop` to a `Task<()>` internally is explicitly out of scope for this task (would require changes to `infigraph-docs`, a separate crate, and isn't required for the CLI/MCP command surface to work correctly) — note this as a real, intentional scope boundary in the PR description when this lands, not an oversight.
- `route_or_serve_request`'s new `WatchControl` match arm (Task 9) needs mutable access to `code_task`/`code_token`/`docs_token` to act on `Stop`/`Start`/`Restart`/`Enable`/`Disable` — thread these through `run_write_coordinator`'s call into `route_or_serve_request` (today: `route_or_serve_request(root, &path, &queue, &shared_registry, &make_registry, &mut held_prism, drain_in_flight, full_reindex_in_flight, &drain_rt)`; add `code_task`/`code_token`/`docs_token`/`daemon_token` params).
- The `.infigraph/requests/`-scoped `notify::Watcher` for event-driven `WatchControl`/write-request detection (replacing the coordinator's existing per-tick `std::fs::read_dir(&requests_dir)` poll) is its own sub-piece of work within this task — implement it as a second, small `notify::Watcher` registered non-recursively on `requests_dir` inside `run_write_coordinator`, feeding a `tokio::sync::mpsc` channel the coordinator's own event loop selects on (mirroring `producer.rs`'s bridging pattern from Task 8) — **but note `run_write_coordinator` is NOT itself async** (per this plan's Global Constraints, the coordinator stays synchronous) — so this can't be a `tokio::select!` arm the way the producer's is. Resolve this by keeping the existing `std::fs::read_dir` poll for now (still correct, just not the event-driven improvement the spec describes) and filing the event-driven upgrade as an explicit, separate follow-up task rather than blocking this phase's core behavioral unlock on it. Update the spec's "Async subprocess spawning"/"Crossing the process boundary" sections to note this deferral if it's taken.

- [x] **Step 5: Implement `route_or_serve_request`'s `WatchControl` arm**

```rust
WriteRequest::WatchControl { role, action } => {
    match (role, action) {
        (WatchRole::Code, WatchAction::Stop) => {
            if let Some(task) = code_task.take() {
                // Cancel without blocking this tick indefinitely -- spawn
                // the stop onto drain_rt and let a later tick reap it,
                // mirroring how full-reindex/SCIP tasks are reaped.
                // Simpler alternative for the plan's first pass: block
                // briefly, since a producer task should stop promptly
                // once cancelled (its own select! loop has no long
                // synchronous work to finish).
                drain_rt.block_on(task.stop());
            }
            reply_ok(reply_path);
            None
        }
        (WatchRole::Code, WatchAction::Start) => {
            if code_task.is_none() {
                let new_task = /* Task::spawn against producer::run_producer, mirroring cmd_daemon's own initial spawn */;
                *code_task = Some(new_task);
            }
            reply_ok(reply_path);
            None
        }
        (WatchRole::Code, WatchAction::Restart) => {
            if let Some(task) = code_task.take() {
                drain_rt.block_on(task.stop());
            }
            let new_task = /* same spawn as Start */;
            *code_task = Some(new_task);
            reply_ok(reply_path);
            None
        }
        (WatchRole::Code, WatchAction::Enable) | (WatchRole::Code, WatchAction::Disable) => {
            // Persisted-policy half lives in config.toml (Phase 4) -- this
            // arm only handles the immediate-effect half for a live daemon.
            // Enable/Disable's live-task effect is identical to Start/Stop
            // above; the distinction (persisted vs one-shot) is entirely
            // in which CLI/MCP command writes config.toml, not in this
            // dispatch arm.
            if action == WatchAction::Disable {
                if let Some(task) = code_task.take() {
                    drain_rt.block_on(task.stop());
                }
            } else if code_task.is_none() {
                let new_task = /* same spawn as Start */;
                *code_task = Some(new_task);
            }
            reply_ok(reply_path);
            None
        }
        (WatchRole::Docs, _) => {
            // Docs stays on its existing AtomicBool/sentinel shape
            // internally (per Step 4's note) -- bridge via docs_token
            // cancel/re-arm instead of a Task<()> handle. Stop/Disable ->
            // cancel docs_token (the bridge task then flips the
            // AtomicBool); Start/Enable/Restart need a fresh docs_token +
            // a freshly-spawned doc_thread + bridge task, which cmd_daemon
            // currently only constructs once at startup -- this requires
            // giving route_or_serve_request a way to ask cmd_daemon-level
            // state to respawn the doc thread, e.g. via a channel or a
            // shared `Option<JoinHandle<()>>` behind a Mutex passed in
            // alongside docs_token. Work out the exact plumbing at
            // implementation time; the shape mirrors the Code arm above,
            // just with an extra indirection for the doc_thread's
            // JoinHandle.
            reply_ok(reply_path);
            None
        }
        (WatchRole::Daemon, WatchAction::Stop) | (WatchRole::Daemon, WatchAction::Restart) => {
            reply_ok(reply_path);
            daemon_token.cancel();
            None
        }
        (WatchRole::Daemon, _) => {
            // Start/Enable/Disable have no meaning for role: Daemon --
            // the daemon process either exists (you're talking to it) or
            // doesn't (nothing to submit a request to). Reply with a
            // clear error rather than silently no-op-ing.
            reply_err(reply_path, "WatchControl { role: Daemon } only supports Stop/Restart");
            None
        }
    }
}
```

(`reply_ok`/`reply_err` are small helpers to write — check whether `route_or_serve_request`'s other arms already have an equivalent inline pattern via `write_atomic`/`WriteResult::Ok`/`WriteResult::Err` that these should reuse directly rather than introducing new helper names; likely yes, given every other arm in the existing function follows that exact shape.)

- [x] **Step 6: Replace the placeholder test with real code, run it**

Return to Step 2's `daemon_survives_watch_control_stop_and_keeps_serving_writes` test, implement it fully against the real harness pattern, and run:

Run: `cargo test -p infigraph-core --test watch_control -- --nocapture`
Expected: PASS.

- [x] **Step 7: Run the FULL workspace test suite**

This task touches the most load-bearing existing code in the whole plan. Run the complete suite, not a subset:

Run: `cargo test --workspace --test-threads=1`
Expected: PASS. Given this repo's disk-constrained-build guidance (per this session's user memory), if a full `--all`/`--workspace` run is infeasible on this machine, run per-crate (`cargo test -p infigraph-core`, `cargo test -p infigraph-cli`, `cargo test -p infigraph-mcp`, `cargo test -p infigraph-docs`) and prune build caches between crates.

- [x] **Step 8: `cargo fmt`, `clippy`, commit**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "feat(core,cli): daemon survives watch stop -- producer/coordinator split via Task<()>, daemon_token hierarchy"
```

This is the natural point to also mark Task 9's checkbox complete (its code landed here).

### Task 11: `WatchControl` reaches an external daemon — CLI submit helper

**Files:**
- Create or modify: a small client-side helper in `crates/infigraph-core/src/daemon_protocol.rs` or `crates/infigraph-cli/src/index.rs` (check for an existing "submit a WriteRequest and poll for the reply" helper before writing a new one — one almost certainly already exists, since every `Index`/`FullReindex` request from the CLI/MCP today goes through exactly this pattern; reuse it, don't duplicate it)

**Interfaces:**
- Consumes: `WriteRequest::WatchControl` (Task 7).
- Produces: a function CLI commands (Phase 4) and MCP tools (Phase 5) both call to submit a `WatchControl` request against an already-running daemon and get back success/failure.

- [ ] **Step 1: Locate the existing submit-and-poll helper**

Before writing anything, find how an existing daemon-mode caller (e.g. `Infigraph::index_via_daemon`, referenced earlier in this plan's research as `crates/infigraph-core/src/lib.rs::index_via_daemon`) submits a `WriteRequest` and awaits its `.result` reply file. Read that function in full.

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn submit_watch_control_and_await_reply_round_trips_against_a_real_daemon() {
    // Spawn a real infigraph daemon (harness pattern per Task 10's test),
    // call the new submit-and-await helper with WatchControl { role: Code,
    // action: Stop }, assert it returns success within a reasonable
    // timeout, and that the request/.result files are cleaned up
    // afterward (matching every other WriteRequest variant's contract).
    todo!("implement against the real daemon-spawning harness")
}
```

- [ ] **Step 3: Implement, following the exact pattern found in Step 1**

Since the exact existing helper's shape wasn't re-read while drafting this plan, this step's implementation must literally mirror that helper's signature/error-handling/timeout conventions rather than invent a new pattern — the goal is one consistent way to submit any `WriteRequest` (including `WatchControl`) to a daemon, not a second, parallel submission mechanism.

- [ ] **Step 4: Run the test, verify it passes**

- [ ] **Step 5: `cargo fmt`, `clippy`, commit**

```bash
git add -A
git commit -m "feat(core): client-side WatchControl submit-and-await helper"
```

---

## Phase 4: CLI Command Surface

*(Spec: "Command surface" (CLI table); "Persisted policy: enable/disable". Depends on Phase 3's `WatchControl` protocol being real.)*

### Task 12: New `Commands` variants

**Files:**
- Modify: `crates/infigraph-cli/src/main.rs` (the `Commands` enum, near the existing `Daemon`/`WatchStop`/`WatchStatus` variants at lines ~331-343 per this plan's earlier read — re-confirm exact lines before editing, since Phase 1-3 tasks may have shifted other files but not this one)

**Interfaces:**
- Produces: `Commands::DaemonStop`, `Commands::DaemonRestart` (new; `Commands::Daemon { debounce }` unchanged, still starts the process in the foreground — matches today's behavior exactly, this is what gets spawned detached); `Commands::Watch { action: WatchCliAction }`; `Commands::WatchDocs { action: WatchCliAction }` where `WatchCliAction` is a clap subcommand enum (`Enable`, `Disable`, `Start`, `Stop`, `Restart`).

- [ ] **Step 1: Add the enum variants**

In `crates/infigraph-cli/src/main.rs`, replace:

```rust
    /// Stop the background auto-watcher
    WatchStop,

    /// Check if a background watcher is running
    WatchStatus,
```

with (keeping `WatchStop`/`WatchStatus` as-is for backward compatibility — deprecate, don't remove, since existing scripts/muscle-memory may depend on them; `cmd_watch_stop` internally now submits `WatchControl { role: Daemon, action: Stop }` instead of writing the raw sentinel, per Task 10's protocol unification):

```rust
    /// Stop the background auto-watcher (deprecated alias for `daemon stop`)
    WatchStop,

    /// Check if a background watcher is running
    WatchStatus,

    /// Stop the daemon process (write-serving + all watching)
    DaemonStop,

    /// Restart the daemon process
    DaemonRestart,

    /// Control code-watching independently of the daemon process
    Watch {
        #[command(subcommand)]
        action: WatchCliAction,
    },

    /// Control doc-watching independently of the daemon process
    WatchDocs {
        #[command(subcommand)]
        action: WatchCliAction,
    },
```

and, near the `Commands` enum's own definition (top-level, alongside it, not nested inside it — check whether this file's convention for subcommand enums places them before or after the parent `Commands` enum, matching whatever `GroupAction` or similar existing nested-subcommand enum in this file already does, since `Group { action: GroupAction }` is visible in the earlier `run()` dispatch read):

```rust
#[derive(Subcommand, Debug, Clone)]
pub enum WatchCliAction {
    /// Persist: enable this watch activity (survives restarts/reindex)
    Enable,
    /// Persist: disable this watch activity
    Disable,
    /// One-shot: start this watch activity now
    Start,
    /// One-shot: stop this watch activity now
    Stop,
    /// One-shot: stop then start
    Restart,
}
```

- [ ] **Step 2: Run `cargo build` to verify it fails**

Run: `cargo build -p infigraph-cli`
Expected: FAIL — `run()`'s `match command` is non-exhaustive (new variants unhandled).

- [ ] **Step 3: Add dispatch arms (stub bodies first)**

In `run()`, alongside the existing `Commands::WatchStop => cmd_watch_stop(root),` line:

```rust
        Commands::WatchStop => cmd_watch_stop(root),
        Commands::WatchStatus => cmd_watch_status(root),
        Commands::DaemonStop => cmd_daemon_stop(root),
        Commands::DaemonRestart => cmd_daemon_restart(root),
        Commands::Watch { action } => cmd_watch_control(root, infigraph_core::daemon_protocol::WatchRole::Code, action),
        Commands::WatchDocs { action } => cmd_watch_control(root, infigraph_core::daemon_protocol::WatchRole::Docs, action),
```

- [ ] **Step 4: Implement the handler functions in `info_commands.rs`**

```rust
pub(crate) fn cmd_daemon_stop(root: &Path) -> Result<()> {
    submit_watch_control_and_await(root, WatchRole::Daemon, WatchAction::Stop)
}

pub(crate) fn cmd_daemon_restart(root: &Path) -> Result<()> {
    // Stop then explicitly re-ensure a fresh daemon is running -- role:
    // Daemon's Restart action (per Task 10's route_or_serve_request arm)
    // only cancels daemon_token; the process exiting means there's nothing
    // left to ask to "start itself" from inside. Re-spawn from the CLI
    // side instead, mirroring `ensure_daemon_running`'s existing pattern.
    submit_watch_control_and_await(root, WatchRole::Daemon, WatchAction::Stop)?;
    // Wait for the process to actually exit before respawning (poll
    // watch.lock's liveness, matching wait_for_daemon_ready's existing
    // shape in crates/infigraph-core/src/watch/daemon.rs).
    let lock_path = root.join(".infigraph").join("watch.lock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while infigraph_core::watch::daemon::daemon_is_alive(&lock_path) {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("daemon did not exit within 10s of a stop request");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let watch_binary = std::env::current_exe()?;
    match infigraph_core::watch::daemon::ensure_daemon_running_required(root, &watch_binary) {
        infigraph_core::watch::daemon::DaemonStartOutcome::Spawned => {
            println!("Daemon restarted.");
            Ok(())
        }
        infigraph_core::watch::daemon::DaemonStartOutcome::AlreadyRunning => {
            println!("Daemon already running (unexpected after a confirmed stop).");
            Ok(())
        }
        infigraph_core::watch::daemon::DaemonStartOutcome::Failed(e) => {
            anyhow::bail!("failed to restart daemon: {e}")
        }
    }
}

pub(crate) fn cmd_watch_control(root: &Path, role: infigraph_core::daemon_protocol::WatchRole, action: crate::WatchCliAction) -> Result<()> {
    use crate::WatchCliAction;
    use infigraph_core::daemon_protocol::WatchAction;
    let watch_action = match action {
        WatchCliAction::Enable => WatchAction::Enable,
        WatchCliAction::Disable => WatchAction::Disable,
        WatchCliAction::Start => WatchAction::Start,
        WatchCliAction::Stop => WatchAction::Stop,
        WatchCliAction::Restart => WatchAction::Restart,
    };
    if matches!(watch_action, WatchAction::Enable | WatchAction::Disable) {
        write_watch_policy_to_config(root, role, watch_action == WatchAction::Enable)?;
    }
    submit_watch_control_and_await(root, role, watch_action)
}
```

`submit_watch_control_and_await` is Task 11's helper; `write_watch_policy_to_config` is Task 14's — this task's `cmd_watch_control` compiles only once both exist, so implement this task's body but defer running it end-to-end until Task 14 lands (leave a `// TODO(Task 14): write_watch_policy_to_config` marker only as an interim, in-progress compile bridge, not as this task's final committed state — Task 14 must replace it with the real function before this phase is considered done).

- [ ] **Step 5: Run `cargo build` to verify it compiles (once Task 11/14's helpers exist — otherwise stub them minimally to unblock this task's own compile/test cycle)**

- [ ] **Step 6: `cargo fmt`, `clippy`, commit**

```bash
cargo fmt -p infigraph-cli -- --check
cargo clippy -p infigraph-cli --lib --bins -- -D warnings
git add crates/infigraph-cli/src/main.rs crates/infigraph-cli/src/info_commands.rs
git commit -m "feat(cli): daemon stop/restart, watch/watch-docs enable/disable/start/stop/restart commands"
```

### Task 13: Real end-to-end CLI integration test

**Files:**
- Test: `crates/infigraph-cli/tests/watch_daemon_docs.rs` (extend, matching its existing real-process-spawning style)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn watch_stop_leaves_the_daemon_process_alive() {
    // Real end-to-end: spawn `infigraph daemon` as a genuine detached
    // process (matching cmd_watch_daemon_also_indexes_docs_without_restart's
    // existing pattern -- read it in full first), run `infigraph watch stop`
    // as a SEPARATE process invocation, confirm the daemon's PID is still
    // alive afterward, confirm `infigraph daemon-stop` (or `daemon stop`,
    // matching whatever the final subcommand naming from Task 12 turned
    // out to be) then does bring it down.
    todo!("implement against the real detached-process harness in this file")
}
```

- [ ] **Step 2-5:** verify-fails, implement against the real harness, verify-passes, `cargo fmt`/`clippy`/commit — same shape as every prior task.

### Task 14: Persisted `enable`/`disable` policy

**Files:**
- Modify: `crates/infigraph-mcp/src/session_context.rs` (`WatchConfig`, new `WatchDocsConfig`)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`write_watch_policy_to_config`)
- Modify: every opportunistic auto-start call site to check the new policy (`should_auto_watch` in `main.rs`, `start_daemon_watcher_for_startup_dir` in `infigraph-mcp/src/recovery.rs`, `ensure_daemon_watcher` in `infigraph-mcp/src/tools/watch.rs`, `auto_start_doc_watch`)

**Interfaces:**
- Consumes: nothing from this plan's other tasks (independent of `Task<T>`/`WatchControl` internals).
- Produces: `watch_enabled(section: &str) -> bool`, generalizing `auto_start_watch_on_boot_enabled`'s existing precedence.

- [ ] **Step 1: Write the failing test**

Add to `crates/infigraph-mcp/tests/startup_watch.rs` (alongside the existing `auto_start_watch_on_boot_enabled_env_override_priority`):

```rust
#[test]
fn watch_enabled_env_override_priority() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // env var wins over config.toml
    std::env::set_var("INFIGRAPH_WATCH_ENABLED", "0");
    assert!(!infigraph_mcp::session_context::watch_enabled("watch"));
    std::env::remove_var("INFIGRAPH_WATCH_ENABLED");
}

#[test]
fn watch_enabled_defaults_to_true_with_nothing_set() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_WATCH_ENABLED");
    std::env::remove_var("INFIGRAPH_WATCH_DOCS_ENABLED");
    assert!(infigraph_mcp::session_context::watch_enabled("watch"));
    assert!(infigraph_mcp::session_context::watch_enabled("watch_docs"));
}
```

(Confirm `session_context.rs`'s existing test module's exact `ENV_LOCK` import path before using it verbatim — this plan captured `auto_start_watch_on_boot_enabled`'s doc comment mentioning env-var precedence but not its test file's exact `ENV_LOCK` definition location.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-mcp watch_enabled -- --nocapture`
Expected: FAIL (`watch_enabled` doesn't exist / isn't `pub`).

- [ ] **Step 3: Implement**

In `crates/infigraph-mcp/src/session_context.rs`, extend `WatchConfig`:

```rust
#[derive(Debug, Deserialize)]
struct WatchConfig {
    #[serde(default = "default_true")]
    auto_start_on_boot: bool,
    #[serde(default = "default_true")]
    enabled: bool,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            auto_start_on_boot: true,
            enabled: true,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WatchDocsConfig {
    #[serde(default = "default_true")]
    enabled: bool,
}

impl Default for WatchDocsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
```

Add `watch_docs: WatchDocsConfig` to `ConfigFile`:

```rust
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    compression: CompressionConfig,
    #[serde(default)]
    watch: WatchConfig,
    #[serde(default)]
    watch_docs: WatchDocsConfig,
}
```

Add the generalized function, alongside `auto_start_watch_on_boot_enabled`:

```rust
/// Generalizes `auto_start_watch_on_boot_enabled`'s exact precedence
/// (env var -> config.toml -> default true) to any watch section.
pub fn watch_enabled(section: &str) -> bool {
    let env_key = format!("INFIGRAPH_{}_ENABLED", section.to_uppercase());
    if let Ok(v) = std::env::var(&env_key) {
        return v != "0" && v.to_lowercase() != "false";
    }
    let config = load_config_file();
    match section {
        "watch" => config.watch.enabled,
        "watch_docs" => config.watch_docs.enabled,
        _ => true,
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p infigraph-mcp watch_enabled -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Wire every opportunistic auto-start call site**

Add a `if !watch_enabled("watch") { return ...; }` (or `"watch_docs"` for the doc equivalents) guard at the top of `should_auto_watch` (`main.rs`), `start_daemon_watcher_for_startup_dir` (`infigraph-mcp/src/recovery.rs`), `ensure_daemon_watcher` (`infigraph-mcp/src/tools/watch.rs`), `auto_start_doc_watch`. For each, write a regression test mirroring the existing `*_respects_daemon_mode_toggle` tests' shape but for the new `enabled` flag instead of the daemon-mode toggle (four new tests, one per call site) — copy each call site's existing daemon-mode-toggle test as the template for its enabled-flag counterpart.

- [ ] **Step 6: Implement `write_watch_policy_to_config` (CLI side, closes Task 12's TODO)**

In `crates/infigraph-cli/src/info_commands.rs`:

```rust
fn write_watch_policy_to_config(root: &Path, role: infigraph_core::daemon_protocol::WatchRole, enabled: bool) -> Result<()> {
    use infigraph_core::daemon_protocol::WatchRole;
    let config_path = root.join(".infigraph").join("config.toml");
    let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&config_path)
        .unwrap_or_default()
        .parse()
        .unwrap_or_default();
    let section = match role {
        WatchRole::Code => "watch",
        WatchRole::Docs => "watch_docs",
        WatchRole::Daemon => anyhow::bail!("enable/disable has no meaning for role: Daemon"),
    };
    doc[section]["enabled"] = toml_edit::value(enabled);
    std::fs::write(&config_path, doc.to_string())?;
    Ok(())
}
```

Confirm `toml_edit` is available (`infigraph-core` already depends on plain `toml = "0.8"`, which round-trips but doesn't preserve comments/formatting on a partial edit — check whether `toml_edit` is already a dependency anywhere in the workspace before adding it fresh; if not available and not wanted as a new dependency, fall back to: parse the whole file with `toml::from_str::<ConfigFile>`, mutate the relevant field, re-serialize the whole `ConfigFile` with `toml::to_string`, and overwrite — accepting that this loses any comments/manual formatting a user had in `config.toml`, which is a real, worth-flagging tradeoff versus `toml_edit`'s surgical edit).

- [ ] **Step 7: Run the full test suite, `cargo fmt`, `clippy`, commit**

```bash
cargo test -p infigraph-mcp -p infigraph-cli
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
git add -A
git commit -m "feat(cli,mcp): persisted watch/watch-docs enable/disable policy in config.toml"
```

---

## Phase 5: MCP Tool Surface

*(Spec: "Command surface" (MCP table); "Crossing the process boundary" (the three-case daemon-mode routing). Depends on Phase 3's `WatchControl` protocol and Phase 4's config.toml policy.)*

### Task 15: `enable_watch`/`disable_watch`/`restart_watch` MCP tools

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/watch.rs`
- Modify: the MCP tool-registration site (find it — likely `crates/infigraph-mcp/src/tools/mod.rs` or `crates/infigraph-mcp/src/lib.rs`; this plan did not re-confirm its exact location, do so before this task)

**Interfaces:**
- Consumes: `watch_enabled`/config-writing (Task 14), `WatchControl` submit helper (Task 11), `watcher_running`/`ensure_daemon_watcher`/`watch_daemon_mode_enabled` (existing).
- Produces: `pub fn enable_watch(args: &Value) -> Result<String>`, `pub fn disable_watch(args: &Value) -> Result<String>`, `pub fn restart_watch(args: &Value) -> Result<String>` — one generic, role-parameterized implementation each tool is a thin wrapper around, matching `tool_watch_project`'s three-case daemon-mode branching (in-process combined task / external daemon delegation / not-primary decline).

- [ ] **Step 1: Write the failing test**

Add to `crates/infigraph-mcp/tests/watcher_daemon_mode.rs`, mirroring `tool_watch_project_respects_daemon_mode_toggle`'s exact shape:

```rust
#[test]
fn tool_disable_watch_respects_daemon_mode_toggle() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let path = root.to_string_lossy().to_string();

    let args = serde_json::json!({ "path": path });
    let result = infigraph_mcp::tools::watch::disable_watch(&args);

    // Must never touch the in-process WATCHERS map -- must route through
    // the WatchControl request bridge to whatever external daemon is
    // running (or report cleanly if none is), exactly like
    // tool_stop_watch already does.
    assert!(
        !infigraph_mcp::tools::watch::is_watching(&path.replace('\\', "/")),
        "daemon mode must never populate the in-process WATCHERS map"
    );

    std::env::remove_var("INFIGRAPH_BACKEND");
}

#[test]
fn tool_enable_watch_in_process_mode_writes_config_and_does_not_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_BACKEND");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let path = root.to_string_lossy().to_string();

    let args = serde_json::json!({ "path": path });
    let result = infigraph_mcp::tools::watch::enable_watch(&args);
    assert!(result.is_ok(), "enable_watch should succeed even with no watcher currently running: {result:?}");

    let config_path = root.join(".infigraph").join("config.toml");
    let contents = std::fs::read_to_string(&config_path).unwrap_or_default();
    assert!(contents.contains("enabled"), "expected config.toml to record the enabled policy: {contents}");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-mcp tool_disable_watch tool_enable_watch -- --nocapture`
Expected: FAIL (functions don't exist).

- [ ] **Step 3: Implement, mirroring `tool_watch_project`'s three-case branching**

```rust
pub fn enable_watch(args: &Value) -> Result<String> {
    watch_control(args, WatchRole::Code, WatchCliAction::Enable)
}

pub fn disable_watch(args: &Value) -> Result<String> {
    watch_control(args, WatchRole::Code, WatchCliAction::Disable)
}

pub fn restart_watch(args: &Value) -> Result<String> {
    watch_control(args, WatchRole::Code, WatchCliAction::Restart)
}

/// Shared, role-parameterized implementation behind enable_watch/
/// disable_watch/restart_watch and their _docs counterparts (Task 16) --
/// per the spec's DRY-first command-surface requirement. Mirrors
/// tool_watch_project's three cases: not-primary decline, daemon-mode
/// delegation (no local task exists -- route through WatchControl), and
/// in-process (WATCHERS map / local producer state).
fn watch_control(args: &Value, role: WatchRole, action: WatchCliAction) -> Result<String> {
    let path = args.get("path").and_then(|p| p.as_str()).context("missing 'path'")?;
    let root = std::path::PathBuf::from(path).canonicalize().context("invalid path")?;
    let root_str = root.to_string_lossy().replace('\\', "/");

    if watchers_disabled() {
        return Ok(format!(
            "Not controlling watch for {root_str}: this MCP instance is not primary."
        ));
    }

    if matches!(action, WatchCliAction::Enable | WatchCliAction::Disable) {
        let section = match role {
            WatchRole::Code => "watch",
            WatchRole::Docs => "watch_docs",
            WatchRole::Daemon => anyhow::bail!("enable/disable has no meaning for role: Daemon"),
        };
        write_watch_policy_to_config(&root, section, action == WatchCliAction::Enable)?;
    }

    if infigraph_core::watch::daemon::watch_daemon_mode_enabled() {
        // Case 3 (spec): no local task exists in daemon mode -- route
        // through the same request bridge tool_stop_watch's daemon-alive
        // branch already uses.
        return submit_watch_control_and_await(&root, role, action.into());
    }

    // Case 2 (spec): in-process, no separate coordinator to defer to --
    // act on the same combined producer state tool_watch_project/
    // tool_stop_watch already manage via WATCHERS/is_watching.
    match (role, action) {
        (WatchRole::Code, WatchCliAction::Disable) => {
            tool_stop_watch(&serde_json::json!({ "path": path }))?;
            Ok(format!("Code watching disabled for {root_str}"))
        }
        (WatchRole::Code, WatchCliAction::Enable) => {
            if !infigraph_mcp::tools::watch::is_watching(&root_str) {
                tool_watch_project(&serde_json::json!({ "path": path }))?;
            }
            Ok(format!("Code watching enabled for {root_str}"))
        }
        (WatchRole::Code, WatchCliAction::Restart) => {
            let _ = tool_stop_watch(&serde_json::json!({ "path": path }));
            tool_watch_project(&serde_json::json!({ "path": path }))?;
            Ok(format!("Code watching restarted for {root_str}"))
        }
        _ => anyhow::bail!("unsupported role/action combination for the in-process case"),
    }
}
```

**A design decision this sketch surfaces that the implementer must confirm before finalizing:** `write_watch_policy_to_config` was written in Task 14 as a `fn` inside `crates/infigraph-cli/src/info_commands.rs` — MCP's `watch.rs` can't call a private CLI-crate function. Either (a) move `write_watch_policy_to_config` into `infigraph-core` as a shared, `pub` function both the CLI and MCP crates call, or (b) duplicate a small MCP-side version. Given this plan's Global Constraints (DRY-first), (a) is correct — revisit Task 14's Step 6 and relocate that function into `infigraph-core` (e.g. `crates/infigraph-core/src/watch/config.rs`, alongside or near `watch_enabled`'s eventual real home — note `watch_enabled` itself was placed in `infigraph-mcp/src/session_context.rs` in Task 14 because that's where `auto_start_watch_on_boot_enabled` already lives; since the CLI now also needs to read/write this same policy, consider moving both into `infigraph-core` as part of this task rather than leaving policy logic split across two crates with the MCP crate depending on CLI internals or vice versa). Resolve this relocation as this task's first concrete step, before writing the `watch_control` function above.

- [ ] **Step 4: Register the new tools**

At the tool-registration site located in Step 0 (re-confirm before this step), register `enable_watch`, `disable_watch`, `restart_watch` following the exact pattern existing tools like `watch_project`/`stop_watch` already use (tool name string, description, JSON schema for `args` — copy an existing single-`path`-argument tool's registration entry verbatim as the template, e.g. `stop_watch`'s).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p infigraph-mcp tool_disable_watch tool_enable_watch -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Run the full MCP test suite, `cargo fmt`, `clippy`, commit**

```bash
cargo test -p infigraph-mcp -- --test-threads=1
cargo fmt -p infigraph-mcp -- --check
cargo clippy -p infigraph-mcp --lib --tests -- -D warnings
git add -A
git commit -m "feat(mcp): enable_watch/disable_watch/restart_watch tools"
```

### Task 16: `_docs` counterparts

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/docs.rs`

**Interfaces:**
- Consumes: `watch_control` (Task 15) generalized to accept `WatchRole::Docs`.
- Produces: `pub fn enable_watch_docs`, `disable_watch_docs`, `restart_watch_docs`.

- [ ] **Step 1: Write the failing test**

Mirror Task 15's two tests exactly, substituting `enable_watch_docs`/`disable_watch_docs` and asserting against doc-watch state (`DOC_WATCHERS`/`is_watching_docs`-equivalent — confirm the exact doc-side counterpart function name in `docs.rs` before writing this test, since this plan's research did not read `docs.rs`'s watcher-tracking internals in full).

- [ ] **Step 2-6:** verify-fails, implement (thin wrappers calling `watch_control` from `watch.rs` — confirm it's `pub(crate)` visible to `docs.rs`, adjust visibility if not — with `WatchRole::Docs`, and the in-process case's arms calling `tool_watch_docs`/`tool_stop_watch_docs` instead of the code equivalents), register the tools, verify-passes, `cargo fmt`/`clippy`/commit.

```bash
git add -A
git commit -m "feat(mcp): enable_watch_docs/disable_watch_docs/restart_watch_docs tools"
```

### Task 17: Full workspace regression pass

**Files:** none (verification-only task)

- [ ] **Step 1: Full workspace test suite**

Run: `cargo test --workspace` (or per-crate if disk-constrained, per Task 10 Step 7's note)
Expected: PASS, zero regressions across all five phases.

- [ ] **Step 2: Full lint pass**

Run: `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Manual smoke test — the actual behavioral unlock**

```bash
cd /path/to/a/real/test/project
infigraph daemon &
sleep 2
infigraph watch stop        # should NOT kill the daemon process
ps aux | grep "infigraph daemon"   # confirm it's still alive
infigraph doctor            # confirm watch.lock still shows a live PID
touch some_file.py          # confirm no reindex happens (watch stopped)
infigraph watch start       # resume
touch some_file.py          # confirm reindex resumes
infigraph daemon stop       # NOW it should exit
```

Document the actual output in the PR description as evidence this plan's core goal (R2.4.4 in `docs/DESIGN-hardening.md`) is real, not just unit-tested.

- [ ] **Step 4: Update `docs/DESIGN-hardening.md`**

Move R2.4.4–R2.4.6 from "Not started" to "Shipped" in the Implementation Status section, following the existing convention (file/module references, brief description) — copy the style of the R2.1.3/R2.3.8 "Shipped" entry as the closest template (also a multi-file, multi-R-number landing).

- [ ] **Step 5: Final commit**

```bash
git add docs/DESIGN-hardening.md
git commit -m "docs(hardening): mark R2.4.4-R2.4.6 shipped"
```

### Task 18: Event-driven `.infigraph/requests/` detection (deferred out of Task 10)

**Files:** `crates/infigraph-core/src/watch/mod.rs`

Task 10's Step 4 explicitly deferred the spec's `.infigraph/requests/`-scoped `notify::Watcher`
and kept `run_write_coordinator`'s per-tick `std::fs::read_dir(&requests_dir)` poll. This is
that deferral written down as real work rather than left as a note — see the spec's
"**Deferred as implemented (Task 10).**" paragraph for the reasoning.

The blocker is structural, not incidental: the coordinator is deliberately synchronous (Global
Constraints), so a bridged `tokio::sync::mpsc` receiver has no `select!` arm to live in the way
`producer.rs`'s does. Any real fix has to pick one of:

- give the coordinator a small runtime and `block_on` a `select!` over
  {request-event, `COORDINATOR_TICK`, stop} at the top of each tick — smallest change, keeps the
  loop's synchronous body intact;
- or move request detection into its own `Task<()>` that feeds a `std::sync::mpsc` the
  synchronous loop `try_recv`s — no runtime in the coordinator, but adds a second thing that can
  die independently and needs its own restart/`WatcherDied` handling, mirroring `producer.rs`.

- [ ] **Step 1:** Pick an approach above and confirm it against the Global Constraints before writing code.
- [ ] **Step 2:** Keep the `read_dir` sweep as a slow-cadence backstop, not delete it — a missed
  or coalesced fsevent must not strand a `.request` file forever, and `submit_write_request`'s
  caller has a finite timeout.
- [ ] **Step 3:** Add a test that a request submitted between two `COORDINATOR_TICK`s is served
  materially faster than the tick cadence (that latency IS the feature; without it this task has
  no observable effect and should not be done at all).
- [ ] **Step 4:** Remove the spec's "Deferred as implemented (Task 10)" paragraph when this lands.

---

## Self-Review

**Spec coverage:**
- `Task<T>` primitive → Task 2. ✓
- `TaskRegistry`/dedup → Task 3 (in-process tier only — the cross-process tier is the pre-existing `watch.lock` trial-flock, correctly left untouched per the spec, not a gap). ✓
- Full-reindex build phase → `Task<T>` → Task 4. ✓
- SCIP enrichment → `Task<T>` → Task 5. ✓
- Async SCIP subprocess spawning → Task 6 (corrected against the real code — no `run_with_timeout` involvement, genuinely adds a timeout). ✓
- `WriteRequest::WatchControl` → Task 7. ✓
- Producer extraction (`watch/producer.rs`) → Task 8. ✓
- `route_or_serve_request`'s `WatchControl` arm → Tasks 9/10 (merged). ✓
- `daemon_token`/`code_token`/`docs_token` hierarchy, `cmd_daemon` rewiring, coordinator rename → Task 10. ✓
- Cross-process `WatchControl` submit helper → Task 11. ✓
- CLI `daemon start/stop/restart`, `watch`/`watch-docs enable/disable/start/stop/restart` → Tasks 12-13. ✓ (`daemon start` itself is unchanged — `Commands::Daemon { debounce }` already exists and is what gets spawned detached; this plan doesn't add a redundant new variant for it.)
- Persisted `enable`/`disable` policy (`config.toml`, `watch_enabled`) → Task 14. ✓
- MCP `enable_watch`/`disable_watch`/`restart_watch` + `_docs` → Tasks 15-16. ✓
- Three-case MCP daemon-mode routing → Task 15's `watch_control` function. ✓
- `daemon-process-vs-write-serving` swap-phase non-cancellability → enforced by Global Constraints + Task 4's explicit note that `finish_full_reindex` is untouched. ✓
- Event-driven `.infigraph/requests/` watch (replacing the coordinator's poll) → **explicitly deferred** in Task 10 Step 4's note, since the coordinator staying synchronous (a Global Constraint) makes a `tokio::select!`-based version impossible without violating that constraint — flagged as a real, intentional scope cut, not silently dropped. If this deferral isn't acceptable, it needs its own follow-up task making the coordinator loop itself poll-and-check the `.infigraph/requests/` directory via a still-synchronous but shorter-interval mechanism, or revisiting whether the coordinator can tolerate a bounded amount of async-ness for just this one concern.

**Placeholder scan:** Three `todo!()`s were deliberately left in this plan (Task 5 Step 1, Task 10 Step 2, Task 13 Step 1, Task 16) — each is explicitly labeled as "replace with real code copied from an existing test's harness pattern before treating the task as started," not a vague deferral. This is a known, bounded exception to "No Placeholders," made necessary because this plan was written without re-reading three specific existing test files in full (`watch_daemon.rs`'s SCIP-adjacent tests, `watch_daemon_docs.rs`'s real-process-spawning harness, `docs.rs`'s watcher-tracking internals) — each `todo!()` names exactly which file to read and which existing test to copy the pattern from, so it's an actionable pointer, not an open-ended gap.

**Type consistency:** `WatchRole`/`WatchAction` (Task 7) are used consistently as the CLI's `WatchCliAction` (Task 12, a separate clap-facing enum) maps onto them via `From`/explicit match in Task 12 Step 4 and Task 15 Step 3 — confirmed the two enums are deliberately distinct (one is the wire protocol type in `infigraph-core`, one is the clap-derive CLI type in `infigraph-cli`) rather than accidentally duplicated; `Task<T>`'s `spawn`/`spawn_blocking` signatures (Task 2, corrected mid-task from an initial flawed sketch to the final token-passing-closure shape) are used consistently in that final shape by every later task that calls them (Tasks 4, 5, 8, 10).

**Real gaps this plan does not paper over, restated for visibility:** the docs side's internal `Task<T>` conversion is explicitly out of scope (Task 10 Step 4) — only its external control surface unifies; `watch_project_auto_resolve`'s exact handling inside the producer-split isn't fully worked out (flagged at Phase 3's opening note); the event-driven `.infigraph/requests/` watch is deferred (above). These are real, bounded scope decisions an executor needs to either accept or escalate — not defects in the plan's rigor.

---

Plan complete and saved to `docs/superpowers/plans/2026-08-21-daemon-watch-command-split.md`. Two execution options:

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

**Which approach?**
