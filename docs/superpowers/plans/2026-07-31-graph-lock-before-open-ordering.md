# Lock-Before-Open Ordering (graph.lock + docs.lock) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every write-mode `Database::new()` call site (the code graph and the docs store) acquires its cross-process lock *before* opening, so two processes racing to write the same database surface infigraph's own bounded-wait `Busy` error instead of Kuzu's raw, undiagnosed `IO exception: Could not set lock on file`.

**Architecture:** Reorder `GraphStore::open_with_lock_timeout` (`crates/infigraph-core/src/graph/store.rs`) to call `WriteLock::acquire_with_timeout` before `Database::new()`, holding the guard through schema-init. Introduce the identical pattern for `DocStore::open()` (`crates/infigraph-docs/src/store.rs`), which today has zero cross-process protection (only a process-local `std::sync::Mutex`) — add `.infigraph/docs.lock` via the same `lockfile` module already used everywhere else, replacing the in-process mutex.

**Tech Stack:** Rust, `kuzu` (aliased `lbug` 0.16.0), `infigraph_core::lockfile` (existing `fs2`-backed flock module), `tempfile` for test isolation.

## Global Constraints

- Base branch: `upstream/main` (this PR opens directly against upstream, no dependency on PR #43).
- No public API signature changes to `GraphStore::open()`/`GraphStore::open_with_lock_timeout()`/`DocStore::open()` — all existing callers must be unaffected.
- Use only the existing `lockfile::acquire`/`try_acquire` functions — no new locking primitives.
- `docs.lock` path convention: `db_path.with_extension("lock")` where `db_path = tg_dir.join("docs.kuzu")`, i.e. `.infigraph/docs.lock` — matching the exact convention `GraphStore` already uses (`path.with_extension("lock")`).
- Every cross-process regression test in this plan must spawn a genuine second OS process (`std::process::Command::new(std::env::current_exe())` with an env-var role flag) — a same-process test does not exercise this bug (verified 2026-07-31: Kuzu's own lock is process-scoped and does not self-conflict within one process, so same-process opens never hit the collision this plan fixes).

---

### Task 1: Cross-process regression test for `GraphStore::open` ordering

**Files:**
- Create: `crates/infigraph-core/tests/graph_lock_before_open.rs`

**Interfaces:**
- Consumes: `infigraph_core::graph::GraphStore::open_with_lock_timeout(path: &Path, timeout: Duration) -> Result<GraphStore>` (existing), `infigraph_core::lockfile::Busy` (existing, `pub` struct with `pub lock_path: PathBuf`, `pub holder: Option<LockInfo>`, `pub waited: Duration`, implements `std::error::Error`).
- Produces: nothing consumed by later tasks — this is a standalone regression test.

- [ ] **Step 1: Write the failing test**

```rust
// crates/infigraph-core/tests/graph_lock_before_open.rs
//
// Regression test for the cross-process open-ordering bug: a second OS
// process's GraphStore::open() must surface infigraph's own Busy error
// (bounded wait, holder identity) rather than Kuzu's raw, undiagnosed
// "Could not set lock on file" -- which requires acquiring graph.lock
// BEFORE calling Database::new(), not after.
//
// Must be a genuine second process, not a second call within this
// process: Kuzu's own lock is process-scoped and does not self-conflict
// within one process (verified 2026-07-31), so a same-process variant of
// this test would pass today even without the fix, proving nothing.

use std::time::Duration;

#[test]
fn second_process_open_surfaces_busy_not_raw_kuzu_error() {
    if std::env::var("GRAPH_LOCK_TEST_CHILD_ROLE").is_ok() {
        let path = std::env::var("GRAPH_LOCK_TEST_DB_PATH").unwrap();
        let result = infigraph_core::graph::GraphStore::open_with_lock_timeout(
            std::path::Path::new(&path),
            Duration::from_millis(500),
        );
        match result {
            Ok(_store) => println!("CHILD_RESULT:OK"),
            Err(e) => {
                let is_busy = e.downcast_ref::<infigraph_core::lockfile::Busy>().is_some();
                println!("CHILD_RESULT:ERR:is_busy={is_busy}:{e}");
            }
        }
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cross_proc.db");

    // Parent opens and holds BOTH the store and its graph.lock write_lock
    // open for the whole test -- the long-lived side of the race
    // (analogous to the watcher, or any other process that keeps its
    // Infigraph/GraphStore alive and is mid-write). Holding write_lock()
    // explicitly is what makes the child's own acquire_with_timeout block
    // and time out into Busy, rather than racing Kuzu's raw error instead.
    let parent_store =
        infigraph_core::graph::GraphStore::open(&db_path).expect("parent open should succeed");
    let _parent_write_lock = parent_store.write_lock().expect("parent write_lock");

    let exe = std::env::current_exe().unwrap();
    let output = std::process::Command::new(&exe)
        .arg("--exact")
        .arg("second_process_open_surfaces_busy_not_raw_kuzu_error")
        .arg("--nocapture")
        .env("GRAPH_LOCK_TEST_CHILD_ROLE", "1")
        .env("GRAPH_LOCK_TEST_DB_PATH", &db_path)
        .output()
        .expect("failed to spawn child process");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("CHILD_RESULT:ERR:is_busy=true"),
        "expected the second process's open to fail with a downcastable \
         lockfile::Busy error; got stdout: {stdout}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test graph_lock_before_open -- --nocapture`
Expected: FAIL. The child process's `Database::new()` (inside `open_with_lock_timeout`, called before any lock is acquired) hits Kuzu's own cross-process exclusivity and returns a raw `anyhow` error wrapping `"failed to open kuzu db: ... Could not set lock on file"` — not a `lockfile::Busy`. The test's `assert!` fails because `stdout` contains `CHILD_RESULT:ERR:is_busy=false:...` instead of `is_busy=true`.

- [ ] **Step 3: Commit the failing test**

```bash
git add crates/infigraph-core/tests/graph_lock_before_open.rs
git commit -m "test: reproduce cross-process graph.lock open-ordering bug"
```

---

### Task 2: Fix `GraphStore::open_with_lock_timeout` ordering

**Files:**
- Modify: `crates/infigraph-core/src/graph/store.rs:117-130`

**Interfaces:**
- Consumes: `WriteLock::acquire_with_timeout(lock_path: &Path, timeout: Duration) -> Result<WriteLock>` (existing, `store.rs:31-33`).
- Produces: no signature changes; `GraphStore::open`/`open_with_lock_timeout` behave identically to callers on success, but now surface `Busy` instead of a raw Kuzu error on cross-process contention.

- [ ] **Step 1: Reorder the lock acquisition before `Database::new()`**

Current code (`store.rs:117-130`):

```rust
    pub fn open_with_lock_timeout(path: &Path, timeout: std::time::Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        validate_db_file(path)?;
        let lock_path = path.with_extension("lock");
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open kuzu db: {e}"))?;
        let store = Self { db, lock_path };
        let lock = WriteLock::acquire_with_timeout(&store.lock_path, timeout)?;
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
        // Acquire the lock BEFORE Database::new(): Kuzu's own cross-process
        // exclusivity check happens inside Database::new() itself, and it
        // fails hard with an undiagnosed "Could not set lock on file" error
        // rather than waiting. Acquiring graph.lock first means a
        // contending process blocks here instead, surfacing our own
        // bounded-wait Busy error (with holder identity) if the timeout
        // expires -- see docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md.
        let lock = WriteLock::acquire_with_timeout(&lock_path, timeout)?;
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open kuzu db: {e}"))?;
        let store = Self { db, lock_path };
        store.init_schema(&lock)?;
        drop(lock);
        Ok(store)
    }
```

- [ ] **Step 2: Run Task 1's test to verify it now passes**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test graph_lock_before_open -- --nocapture`
Expected: PASS. The parent holds `_parent_write_lock` (from Task 1's corrected test) for the whole test, so `graph.lock` stays held for as long as the parent process runs. With this fix in place, the child's `WriteLock::acquire_with_timeout` now runs *before* the child's own `Database::new()` — it blocks on the parent's held lock and correctly times out into `Busy` within the test's 500ms budget, instead of reaching `Database::new()` at all.

- [ ] **Step 3: Re-run full existing lock test suite to check for regressions**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test write_lock_wiring`
Expected: all existing tests still PASS, including `test_open_initializes_schema_under_write_lock` (same-process case, unaffected by this reordering since it was already passing via the app-level `WriteLock`).

- [ ] **Step 4: Commit**

```bash
git add crates/infigraph-core/src/graph/store.rs crates/infigraph-core/tests/graph_lock_before_open.rs
git commit -m "fix: acquire graph.lock before Database::new() to fix cross-process races"
```

---

### Task 3: Cross-process regression test + fix for `DocStore::open` (`docs.lock`)

**Files:**
- Modify: `crates/infigraph-docs/src/store.rs:59-77`
- Create: `crates/infigraph-docs/tests/docs_lock_before_open.rs`

**Interfaces:**
- Consumes: `infigraph_core::lockfile::{acquire, LockFile}` (existing, `pub fn acquire(path: &Path, role: &str, timeout: Duration) -> Result<LockFile>`).
- Produces: `DocStore::open(path: &Path) -> Result<Self>` — same signature, now cross-process safe.

- [ ] **Step 1: Write the failing test**

```rust
// crates/infigraph-docs/tests/docs_lock_before_open.rs
//
// Same shape as graph_lock_before_open.rs (infigraph-core): DocStore::open
// today only guards Database::new() with a process-local Mutex<()>, which
// provides zero cross-process protection. A genuine second OS process must
// surface a clean, bounded-wait error rather than either a raw Kuzu error
// or silent data corruption from two processes writing docs.kuzu at once.

use std::time::Duration;

#[test]
fn second_process_open_is_serialized_not_racing() {
    if std::env::var("DOCS_LOCK_TEST_CHILD_ROLE").is_ok() {
        let path = std::env::var("DOCS_LOCK_TEST_DB_PATH").unwrap();
        let result = infigraph_docs::store::DocStore::open(std::path::Path::new(&path));
        match result {
            Ok(_store) => println!("CHILD_RESULT:OK"),
            Err(e) => {
                let is_busy = e.downcast_ref::<infigraph_core::lockfile::Busy>().is_some();
                println!("CHILD_RESULT:ERR:is_busy={is_busy}:{e}");
            }
        }
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("docs.kuzu");

    let parent_store =
        infigraph_docs::store::DocStore::open(&db_path).expect("parent open should succeed");
    // Keep the parent's Database handle alive for the duration of the test.
    std::mem::forget(parent_store);

    let exe = std::env::current_exe().unwrap();
    let output = std::process::Command::new(&exe)
        .arg("--exact")
        .arg("second_process_open_is_serialized_not_racing")
        .arg("--nocapture")
        .env("DOCS_LOCK_TEST_CHILD_ROLE", "1")
        .env("DOCS_LOCK_TEST_DB_PATH", &db_path)
        .output()
        .expect("failed to spawn child process");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("CHILD_RESULT:ERR:is_busy=true"),
        "expected the second process's open to fail with a downcastable \
         lockfile::Busy error from docs.lock; got stdout: {stdout}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-docs --test docs_lock_before_open -- --nocapture`
Expected: FAIL. `DocStore::open`'s process-local `DB_LOCK` mutex does nothing across processes, so the child's `Database::new()` either hits Kuzu's raw cross-process error (`is_busy=false`) or — since the parent used `std::mem::forget` and never calls a write, timing may vary — the assertion fails one way or another because no `lockfile::Busy` is ever produced today.

- [ ] **Step 3: Add `docs.lock` to `DocStore::open`**

Current code (`crates/infigraph-docs/src/store.rs:59-77`):

```rust
pub struct DocStore {
    db: Database,
    _db_guard: std::sync::MutexGuard<'static, ()>,
}

static DB_LOCK: Mutex<()> = Mutex::new(());

impl DocStore {
    pub fn open(path: &Path) -> Result<Self> {
        let guard = DB_LOCK
            .lock()
            .map_err(|e| anyhow::anyhow!("doc store lock poisoned: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open docs kuzu db: {e}"))?;
        let store = Self {
            db,
            _db_guard: guard,
        };
        store.init_schema()?;
        Ok(store)
    }
```

Replace with:

```rust
pub struct DocStore {
    db: Database,
    _db_guard: std::sync::MutexGuard<'static, ()>,
    _cross_process_guard: infigraph_core::lockfile::LockFile,
}

static DB_LOCK: Mutex<()> = Mutex::new(());

/// Default wait budget for docs.lock, matching graph.lock's own default
/// (crates/infigraph-core/src/graph/store.rs's GRAPH_WRITE_TIMEOUT).
const DOCS_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DOCS_WRITE_ROLE: &str = "docs-write";

impl DocStore {
    pub fn open(path: &Path) -> Result<Self> {
        let guard = DB_LOCK
            .lock()
            .map_err(|e| anyhow::anyhow!("doc store lock poisoned: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock_path = path.with_extension("lock");
        // Cross-process guard, acquired BEFORE Database::new() -- the
        // in-process DB_LOCK mutex above only ever protected against
        // races within this one process; docs.kuzu had zero cross-process
        // protection until this lock. Same pattern as graph.lock, see
        // docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md.
        let cross_process_guard = infigraph_core::lockfile::acquire(
            &lock_path,
            DOCS_WRITE_ROLE,
            DOCS_WRITE_TIMEOUT,
        )?;
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open docs kuzu db: {e}"))?;
        let store = Self {
            db,
            _db_guard: guard,
            _cross_process_guard: cross_process_guard,
        };
        store.init_schema()?;
        Ok(store)
    }
```

Add `infigraph-core` as a dependency of `infigraph-docs` if it isn't already (check `crates/infigraph-docs/Cargo.toml` first — `infigraph-docs` almost certainly already depends on `infigraph-core` given it's downstream of the core graph types; if the `[dependencies]` entry is missing, add `infigraph-core = { path = "../infigraph-core" }` matching the existing version/path style used by sibling crates in the workspace).

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-docs --test docs_lock_before_open -- --nocapture`
Expected: PASS — the child's `lockfile::acquire` call now blocks on the parent's held `docs.lock`, times out at its own bounded wait, and returns `Busy`.

- [ ] **Step 5: Run the full `infigraph-docs` test suite to check for regressions**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-docs`
Expected: all existing tests still PASS — the `DB_LOCK` mutex behavior is unchanged for same-process callers, `docs.lock` is additive.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-docs/src/store.rs crates/infigraph-docs/tests/docs_lock_before_open.rs crates/infigraph-docs/Cargo.toml
git commit -m "fix: add cross-process docs.lock to DocStore::open"
```

---

### Task 4: Full workspace test pass + open upstream PR

**Files:** none (verification + delivery task)

- [ ] **Step 1: Run the full workspace test suite**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test --workspace`
Expected: PASS. If any pre-existing unrelated failure appears (e.g. the `groups_watch_perf` / `INFIGRAPH_WATCH_DAEMON` env leak documented earlier this session), verify it's pre-existing on unmodified `upstream/main` before proceeding — do not silently ignore a new failure.

- [ ] **Step 2: Run `cargo fmt` and `cargo clippy`**

Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings`
Expected: clean, no warnings.

- [ ] **Step 3: Push and open the upstream PR**

```bash
git push -u origin fix/graph-lock-before-open-ordering
```

Open a PR against `intuit/infigraph` `main`, referencing this plan's spec (`docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md` — note: since this doc lives only in the fork, summarize its "Part 1" section inline in the PR description rather than linking a fork-only path) and cross-linking upstream issue #46.

---

### Task 5: Port to `feat/hardening` fork

**Files:** none (delivery task — exact mechanics depend on whether the fork's `store.rs`/`docs/store.rs` have diverged since this plan was written)

- [ ] **Step 1: Attempt a clean cherry-pick**

From the main `feat/hardening` worktree:

```bash
git cherry-pick <task-2-commit-sha> <task-3-commit-sha>
```

- [ ] **Step 2: If conflicts occur, resolve manually**

The fork's `crates/infigraph-core/src/graph/store.rs` already differs from upstream (`open_with_lock_timeout` already exists as its own function, with an added `validate_db_file` preflight call — see `store.rs:117-130` on `feat/hardening`, read earlier this session). Reapply the same ordering change (`WriteLock::acquire_with_timeout` before `Database::new()`) directly against the fork's actual current text rather than forcing the cherry-pick; the docs.lock change should apply more cleanly since `infigraph-docs/src/store.rs` was not part of this session's fork-specific hardening work.

- [ ] **Step 3: Run the fork's test suite**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test graph_lock_before_open -p infigraph-core --test write_lock_wiring -p infigraph-docs --test docs_lock_before_open`
Expected: PASS on the fork, same as upstream.

- [ ] **Step 4: Commit and push to the fork**

```bash
git add -A
git commit -m "fix: port graph.lock/docs.lock open-ordering fix from upstream PR"
git push origin feat/hardening
```
