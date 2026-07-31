# Watcher Connection Yield Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The watcher's persistent `held_prism` connection (PR #43) proactively closes itself when another process is waiting on `graph.lock`, so it stops permanently starving other writers (upstream issue #46), while still keeping the connection open across batches in the common uncontended case (preserving PR #43's checkpoint-storm fix).

**Architecture:** A contending writer, while blocked in `GraphStore::open_with_lock_timeout`'s `graph.lock` acquire, also holds a second flock (`.infigraph/graph.lock.wanted`) — but only if `.infigraph/watch.lock` shows a watcher is actually running. The watcher's main loop peeks `.wanted` (non-blocking) at the same point it already checks the `watch.stop` sentinel; if held, it drops `held_prism`, closing its connection, and continues its loop unmodified otherwise. No new retry/reacquire logic is needed — the watcher's existing next-batch/periodic-tick path already re-acquires `graph.lock` before reopening `Database`.

**Tech Stack:** Rust, `infigraph_core::lockfile` (existing `fs2`-backed flock module), `notify` (existing watcher dependency, untouched by this plan).

## Global Constraints

- **Base branch dependency:** this plan's worktree must be based on `fix/watch-persistent-db-connection` (PR #43) **with Part 1's ordering fix also merged in** (`docs/superpowers/plans/2026-07-31-graph-lock-before-open-ordering.md`, specifically `GraphStore::open_with_lock_timeout` acquiring `graph.lock` before `Database::new()`). Without Part 1's fix already present, this plan's yield mechanism has nothing to protect: a contending writer's `Database::new()` would still be attempted before any lock check, colliding directly with Kuzu regardless of whether the watcher yields. Task 1 below handles combining the two branches.
- No public API signature changes to `GraphStore::open_with_lock_timeout`, `watch_project_with_periodic`, or any other existing public function.
- Use only `lockfile::acquire`/`try_acquire`/`read_holder` (existing) plus one small new pure helper, `lockfile::wanted_signal_path` — no new locking primitive types.
- `.wanted` signal path convention: `graph.lock.wanted`, derived from `graph.lock`'s own path (`<lock_path>.wanted`), not a fixed/hardcoded string, so it stays correct if the lock path ever changes.
- The gating check (`watch.lock` holder present) must run *before* touching `.wanted` at all — the common case (no watcher running) must not create or check any new file.
- Detection on the watcher side reuses the existing `watch.stop` polling idiom exactly (plain check at the top of the loop, same cadence) — no new `notify::Watcher` registration, no `ignore_dirs` exception.

---

### Task 1: Worktree setup — combine PR #43 + Part 1's fix

**Files:** none (setup task)

- [ ] **Step 1: Create the worktree, based on the existing PR #43 worktree**

The existing worktree `scratchpad/wt-upstream-watch-persistent-conn` (branch `fix/watch-persistent-db-connection`) already has PR #43's changes. From the main `feat/hardening` worktree:

```bash
cd scratchpad/wt-upstream-watch-persistent-conn
git fetch origin fix/graph-lock-before-open-ordering
git checkout -b fix/watcher-connection-yield
git merge origin/fix/graph-lock-before-open-ordering
```

(If Part 1's branch hasn't been pushed to `origin` yet at this point, merge directly from Part 1's local worktree instead: `git merge <path-to-part-1-worktree-or-local-branch>/fix/graph-lock-before-open-ordering`.)

- [ ] **Step 2: Verify the merge brought in Part 1's fix correctly**

Run: `rg -n "WriteLock::acquire_with_timeout" crates/infigraph-core/src/graph/store.rs`
Expected: the `WriteLock::acquire_with_timeout` call appears **before** `Database::new()` in `open_with_lock_timeout` — confirms Part 1's ordering fix is present in this branch.

- [ ] **Step 3: Run the existing test suite on the merged branch**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test graph_lock_before_open --test write_lock_wiring`
Expected: PASS — both PR #43's own tests and Part 1's regression test pass together with no conflicts.

---

### Task 2: `wanted_signal_path` helper + writer-side signaling

**Files:**
- Modify: `crates/infigraph-core/src/lockfile.rs` (add helper)
- Modify: `crates/infigraph-core/src/graph/store.rs:117-130` (Part 1's already-fixed version)
- Create: `crates/infigraph-core/tests/wanted_signal.rs`

**Interfaces:**
- Produces: `pub fn lockfile::wanted_signal_path(lock_path: &Path) -> PathBuf` — used by both the writer side (this task) and the watcher side (Task 3).

- [ ] **Step 1: Write the failing unit test for the path helper**

```rust
// crates/infigraph-core/tests/wanted_signal.rs
use std::path::Path;

#[test]
fn wanted_signal_path_appends_wanted_suffix() {
    let lock_path = Path::new("/tmp/project/.infigraph/graph.lock");
    let wanted = infigraph_core::lockfile::wanted_signal_path(lock_path);
    assert_eq!(
        wanted,
        Path::new("/tmp/project/.infigraph/graph.lock.wanted")
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test wanted_signal`
Expected: FAIL with a compile error — `wanted_signal_path` doesn't exist yet.

- [ ] **Step 3: Add the helper to `lockfile.rs`**

Add near the top of `crates/infigraph-core/src/lockfile.rs`, after the existing `use` statements:

```rust
/// Path of the "someone is waiting" signal file for a given lock, e.g.
/// `graph.lock` -> `graph.lock.wanted`. A contending writer holds this
/// second flock only while it's blocked waiting on the primary lock; the
/// primary lock's long-lived holder (e.g. the watcher's persistent
/// connection) peeks it to detect contention and yield. See
/// docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md
/// Part 2.
pub fn wanted_signal_path(lock_path: &Path) -> PathBuf {
    let mut os_string = lock_path.as_os_str().to_owned();
    os_string.push(".wanted");
    PathBuf::from(os_string)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test wanted_signal`
Expected: PASS.

- [ ] **Step 5: Commit the helper**

```bash
git add crates/infigraph-core/src/lockfile.rs crates/infigraph-core/tests/wanted_signal.rs
git commit -m "feat: add wanted_signal_path helper for graph.lock.wanted"
```

- [ ] **Step 6: Write the failing writer-side signaling test**

```rust
// Append to crates/infigraph-core/tests/wanted_signal.rs
//
// A contending writer, while blocked in open_with_lock_timeout's
// graph.lock acquire, must hold .wanted -- but ONLY if watch.lock shows a
// watcher is running. This test simulates "a watcher is running" by
// writing a watch.lock file directly (via lockfile::acquire), then
// starts a real background thread contending for graph.lock (held by the
// main thread), and asserts .wanted becomes observably held during that
// wait -- then released once the wait ends.

use std::time::Duration;

#[test]
fn writer_signals_wanted_only_when_watcher_present() {
    let dir = tempfile::tempdir().unwrap();
    let infigraph_dir = dir.path().join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();
    let db_path = infigraph_dir.join("graph");
    let watch_lock_path = infigraph_dir.join("watch.lock");

    // Simulate a running watcher: hold watch.lock for this test's duration.
    let _watch_lock_guard =
        infigraph_core::lockfile::acquire(&watch_lock_path, "watch-daemon", Duration::from_secs(5))
            .unwrap();

    // Main thread: open the store and hold its write_lock, blocking any
    // other writer's own graph.lock acquire.
    let store = infigraph_core::graph::GraphStore::open(&db_path).unwrap();
    let _held = store.write_lock().unwrap();

    // Background thread: a second, contending "writer" tries to open the
    // same path -- it should block on graph.lock and, because watch.lock
    // shows a watcher present, hold .wanted for the duration of that wait.
    let db_path_clone = db_path.clone();
    let handle = std::thread::spawn(move || {
        let _ = infigraph_core::graph::GraphStore::open_with_lock_timeout(
            &db_path_clone,
            Duration::from_millis(800),
        );
    });

    // Poll for up to 500ms for .wanted to become held by someone other
    // than this test (a failed try_acquire from here means it's held).
    let lock_path = db_path.with_extension("lock");
    let wanted_path = infigraph_core::lockfile::wanted_signal_path(&lock_path);
    let start = std::time::Instant::now();
    let mut observed_wanted = false;
    while start.elapsed() < Duration::from_millis(500) {
        if let Ok(None) = infigraph_core::lockfile::try_acquire(&wanted_path, "test-observer") {
            observed_wanted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        observed_wanted,
        ".wanted was never observed held while a contending writer waited on graph.lock \
         with a watcher present"
    );

    handle.join().unwrap();
}
```

- [ ] **Step 7: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test wanted_signal -- --test-threads=1`
Expected: FAIL — `open_with_lock_timeout` doesn't yet touch `.wanted` at all, so `observed_wanted` stays `false`.

- [ ] **Step 8: Implement writer-side signaling in `store.rs`**

Current code (`store.rs:117-130`, after Part 1's fix):

```rust
    pub fn open_with_lock_timeout(path: &Path, timeout: std::time::Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        validate_db_file(path)?;
        let lock_path = path.with_extension("lock");
        let lock = WriteLock::acquire_with_timeout(&lock_path, timeout)?;
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open kuzu db: {e}"))?;
        let store = Self { db, lock_path };
        store.init_schema(&lock)?;
        drop(lock);
        Ok(store)
    }
```

Replace with:

```rust
    pub fn open_with_lock_timeout(path: &Path, timeout: std::time::Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        validate_db_file(path)?;
        let lock_path = path.with_extension("lock");

        // If a watcher is running for this project, signal contention by
        // holding graph.lock.wanted for the duration of the graph.lock
        // acquire attempt below -- the watcher's poll loop peeks this to
        // detect contention and yield its idle connection. Skipped
        // entirely when no watcher is running (the common case): ordinary
        // bounded-wait is sufficient there, since whoever else holds
        // graph.lock is also short-lived. See
        // docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md
        // Part 2.
        let watch_lock_path = path.parent().map(|p| p.join("watch.lock"));
        let watcher_present = watch_lock_path
            .as_deref()
            .is_some_and(|p| lockfile::read_holder(p).is_some());
        let wanted_guard = if watcher_present {
            let wanted_path = lockfile::wanted_signal_path(&lock_path);
            lockfile::acquire(&wanted_path, "graph-lock-waiter", timeout).ok()
        } else {
            None
        };

        let lock = WriteLock::acquire_with_timeout(&lock_path, timeout)?;
        // Signaling is only meaningful while genuinely waiting; drop it as
        // soon as the wait resolves so it doesn't linger through the
        // subsequent open/write.
        drop(wanted_guard);

        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open kuzu db: {e}"))?;
        let store = Self { db, lock_path };
        store.init_schema(&lock)?;
        drop(lock);
        Ok(store)
    }
```

**Known, accepted limitation (documented, not fixed by this plan):** `.wanted` uses a plain exclusive flock via the existing `lockfile::acquire`, so two *simultaneous* contenders each waiting on `graph.lock` will also serialize amongst themselves for `.wanted` (only one can hold it at a time). This does not add unbounded extra wait — a contender's `.wanted` hold is bounded by the same `timeout` as its `graph.lock` wait — but it means, in the rare multi-contender case, only one contender's wait is visibly signaled to the watcher at any instant. Acceptable for a first cut: the common case is a single contender against a long-idle watcher connection, and once the watcher yields, all contenders proceed to race for `graph.lock` fairly via the OS's own flock queuing, same as today.

- [ ] **Step 9: Run test to verify it passes**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test wanted_signal -- --test-threads=1`
Expected: PASS.

- [ ] **Step 10: Run the full lock test suite to check for regressions**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test write_lock_wiring --test graph_lock_before_open`
Expected: all PASS — `.wanted` signaling is additive and only activates when `watch.lock` shows a holder, so these watcher-free tests are unaffected.

- [ ] **Step 11: Commit**

```bash
git add crates/infigraph-core/src/graph/store.rs crates/infigraph-core/tests/wanted_signal.rs
git commit -m "feat: signal graph.lock.wanted while waiting, gated by watch.lock"
```

---

### Task 3: Watcher-side yield on contention

**Files:**
- Modify: `crates/infigraph-core/src/watch/mod.rs` (main loop, near `sentinel.exists()`)
- Create: `crates/infigraph-core/tests/watcher_yield.rs`

**Interfaces:**
- Consumes: `lockfile::try_acquire` (existing), `lockfile::wanted_signal_path` (Task 2).

- [ ] **Step 1: Write the failing test**

This test exercises the yield check in isolation (as a pure function extracted from the loop), rather than the full `watch_project_with_periodic` loop (which needs a real filesystem watcher and is exercised end-to-end in Task 4). Extract the check into a small, directly-testable function first.

```rust
// crates/infigraph-core/tests/watcher_yield.rs
use std::time::Duration;

#[test]
fn yield_check_detects_and_clears_contention() {
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join("graph.lock");
    let wanted_path = infigraph_core::lockfile::wanted_signal_path(&lock_path);

    // No contender yet: should_yield_connection must report false.
    assert!(!infigraph_core::watch::should_yield_connection(&lock_path));

    // A contender holds .wanted.
    let _contender_guard =
        infigraph_core::lockfile::acquire(&wanted_path, "test-contender", Duration::from_secs(5))
            .unwrap();
    assert!(infigraph_core::watch::should_yield_connection(&lock_path));

    // Contender releases: should_yield_connection must report false again.
    drop(_contender_guard);
    assert!(!infigraph_core::watch::should_yield_connection(&lock_path));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test watcher_yield`
Expected: FAIL with a compile error — `infigraph_core::watch::should_yield_connection` doesn't exist yet.

- [ ] **Step 3: Add `should_yield_connection` and wire it into the loop**

Add near the top of `crates/infigraph-core/src/watch/mod.rs`, after the existing `use` statements (must be `pub` — the test above calls it as `infigraph_core::watch::should_yield_connection`, and `watch` is already a public module per `crate::Infigraph`'s use of it):

```rust
/// True if another process is currently signaling contention on
/// `lock_path` (its companion `.wanted` file is held). A non-blocking
/// peek: attempts to acquire `.wanted` itself, releasing immediately if
/// successful (nobody was waiting) or reporting `true` if it's already
/// held (someone is). See
/// docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md
/// Part 2.
pub fn should_yield_connection(lock_path: &Path) -> bool {
    let wanted_path = crate::lockfile::wanted_signal_path(lock_path);
    matches!(crate::lockfile::try_acquire(&wanted_path, "watcher-peek"), Ok(None))
}
```

Then, in `watch_project_with_periodic`'s main loop, add the check right after the existing `sentinel.exists()` block (which is at the top of the `loop { ... }`):

```rust
        if sentinel.exists() {
            let _ = std::fs::remove_file(&sentinel);
            break;
        }

        // Someone wants graph.lock and we're holding an idle connection
        // open -- yield it so their Database::new() can succeed. See
        // docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md
        // Part 2. Only meaningful when held_prism is actually open; if
        // it's already None there's nothing to close.
        if held_prism.is_some() && should_yield_connection(&graph_lock_path) {
            held_prism = None;
        }
```

And add `let graph_lock_path = root.join(".infigraph").join("graph.lock");` right next to the existing `let sentinel = root.join(".infigraph").join("watch.stop");` line, so both are computed once before the loop starts.

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test watcher_yield`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/watch/mod.rs crates/infigraph-core/tests/watcher_yield.rs
git commit -m "feat: watcher yields its idle connection on graph.lock contention"
```

---

### Task 4: End-to-end integration test

**Files:**
- Create: `crates/infigraph-core/tests/watcher_yield_e2e.rs`

**Interfaces:**
- Consumes: `watch_project_with_periodic` (existing, exercised indirectly via a short-lived spawned watcher thread), `GraphStore::open_with_lock_timeout` (Task 2).

- [ ] **Step 1: Write the test**

This proves the full loop: a running watcher holding `held_prism` open, a contending writer blocked on `graph.lock`, the watcher yielding, and the contender succeeding — all without the contender ever needing longer than its normal bounded wait.

```rust
// crates/infigraph-core/tests/watcher_yield_e2e.rs
//
// Full-loop proof: simulates "a watcher is running and holds a
// long-lived connection open" without spinning up the real
// notify-based watch loop (that's covered by existing watcher tests) --
// this test isolates the yield mechanism's actual effect on a
// contending writer's wait time.

use std::time::Duration;

#[test]
fn contending_writer_succeeds_once_watcher_yields() {
    let dir = tempfile::tempdir().unwrap();
    let infigraph_dir = dir.path().join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();
    let db_path = infigraph_dir.join("graph");
    let watch_lock_path = infigraph_dir.join("watch.lock");
    let graph_lock_path = db_path.with_extension("lock");

    // Simulate a running watcher: hold watch.lock, and hold a store open
    // (analogous to held_prism) plus its own write_lock briefly, then
    // release the write_lock but KEEP the store's Database open --
    // mirroring the watcher's real steady state between batches (no
    // active write, but connection still open).
    let _watch_lock_guard =
        infigraph_core::lockfile::acquire(&watch_lock_path, "watch-daemon", Duration::from_secs(5))
            .unwrap();
    let watcher_store = infigraph_core::graph::GraphStore::open(&db_path).unwrap();

    // Spawn a background "watcher poll loop" thread: checks
    // should_yield_connection every 20ms (much faster than the real
    // ~200ms cadence, to keep this test fast) and drops the store when
    // contention is detected.
    let watcher_store = std::sync::Mutex::new(Some(watcher_store));
    std::thread::scope(|scope| {
        let poll_handle = scope.spawn(|| {
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_secs(2) {
                if infigraph_core::watch::should_yield_connection(&graph_lock_path) {
                    watcher_store.lock().unwrap().take(); // drop the Database
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });

        // Contending writer: opens the same path for write. Its own
        // graph.lock acquire succeeds quickly (watcher isn't mid-write),
        // but its Database::new() would collide with the watcher's still-open
        // handle unless the watcher yields in response to .wanted.
        let contender = infigraph_core::graph::GraphStore::open_with_lock_timeout(
            &db_path,
            Duration::from_secs(2),
        );
        assert!(
            contender.is_ok(),
            "contending writer should succeed once the watcher yields its \
             idle connection: {:?}",
            contender.err()
        );

        poll_handle.join().unwrap();
    });
}
```

- [ ] **Step 2: Run test**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test watcher_yield_e2e -- --nocapture`
Expected: PASS. If it fails, check timing first — the poll thread's 20ms interval and the contender's 2s timeout should comfortably avoid flakiness, but if it's ever flaky, the poll interval or timeout can be widened; do not skip or ignore the test.

- [ ] **Step 3: Commit**

```bash
git add crates/infigraph-core/tests/watcher_yield_e2e.rs
git commit -m "test: end-to-end proof of watcher connection yield under contention"
```

---

### Task 5: Full workspace test pass + open upstream PR (stacked on #43)

**Files:** none (verification + delivery task)

- [ ] **Step 1: Run the full workspace test suite**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test --workspace`
Expected: PASS.

- [ ] **Step 2: Run `cargo fmt` and `cargo clippy`**

Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings`
Expected: clean, no warnings.

- [ ] **Step 3: Push and open the upstream PR, targeting PR #43's branch**

```bash
git push -u origin fix/watcher-connection-yield
```

Open a PR against `intuit/infigraph`'s `fix/watch-persistent-db-connection` branch (PR #43) — not `main` — since this stacks on top of it. Reference this plan's spec (summarized inline, since the spec doc itself is fork-only) and cross-link upstream issue #46, noting it resolves the watcher-starves-other-writers complaint without regressing #43's checkpoint-storm fix.

---

### Task 6: Port to `feat/hardening` fork

**Files:** none (delivery task)

- [ ] **Step 1: Confirm prerequisites are already on the fork**

Both PR #43's changes and Part 1's ordering fix must already be ported to `feat/hardening` before this step (Part 1's own Task 5 covers its port; PR #43 itself is fork-originated, already present).

- [ ] **Step 2: Attempt a clean cherry-pick**

From the main `feat/hardening` worktree:

```bash
git cherry-pick <task-2-commits> <task-3-commit> <task-4-commit>
```

- [ ] **Step 3: If conflicts occur, resolve manually against the fork's actual current `store.rs`/`watch/mod.rs`**

The fork's `watch/mod.rs` may have diverged further (e.g. the doctor watcher-liveness work landed this session). Reapply the same `should_yield_connection` check and `graph_lock_path` computation against the fork's current loop structure rather than forcing the cherry-pick.

- [ ] **Step 4: Run the fork's test suite**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test wanted_signal --test watcher_yield --test watcher_yield_e2e`
Expected: PASS on the fork.

- [ ] **Step 5: Commit and push to the fork**

```bash
git add -A
git commit -m "fix: port watcher connection-yield mechanism from upstream PR"
git push origin feat/hardening
```
