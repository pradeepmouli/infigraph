# PR 1: `lockfile` Module + WriteLock Adoption — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give infigraph's lock files identity payloads, bounded-wait acquisition, and structured `Busy` errors, adopted by the existing `WriteLock` — PR 1 of the write-safety spec (`docs/superpowers/specs/2026-07-20-write-safety-locks-design.md`).

**Architecture:** New `crates/infigraph-core/src/lockfile.rs` owns all lock mechanics (flock via `fs2` + JSON identity payload + bounded wait + `Busy` error). `WriteLock` in `graph/store.rs` becomes a thin wrapper over it. Kernel flock semantics remain the source of truth for liveness (flocks auto-release on process death), so the payload is diagnostics + stale-file adoption, never a liveness oracle.

**Tech Stack:** Rust, `fs2` 0.4 (already a dep), `serde`/`serde_json` (workspace deps), `build.rs` for compile-time build hash. No new dependencies.

## Global Constraints

- Branch: `feat/lockfile-identity` off fork `main` (v3.2.1 base). Commits go only to this branch.
- Commit with `--no-verify`: the repo pre-commit hook runs `cargo fmt --check` globally and fails on **pre-existing** drift in `crates/infigraph-core/src/scip/mod.rs` and `crates/infigraph-languages/tests/registry_integration.rs` (local rustfmt 1.97 vs upstream skew, incident I-11). Do NOT reformat those files — that churn must not enter this PR. DO run `cargo fmt` on files you create/modify (`git diff --name-only | xargs rustfmt` style, or format-on-save).
- Every commit message ends with:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
- Conservative rule (from spec): a held flock without readable identity is "unknown holder" — bounded-wait then `Busy`; never broken.
- Default bounded-wait timeout: 30 s (`Duration::from_secs(30)`); tests use millisecond timeouts.
- **Deviation from spec, intentional:** the spec's `LockInfo.pid_start_time` field is omitted. It existed to guard PID-based stale-breaking against PID reuse — but this design never breaks a *held* flock based on PID at all (the kernel releases flocks on holder death, making liveness authoritative), so the field has no consumer. YAGNI. Reintroduce only if a future lock consumer needs PID-based decisions.
- `cargo test -p infigraph-core` takes several minutes cold (Kuzu C++ build was cleaned; first build ~4 min). Prefer `cargo test -p infigraph-core --test lockfile` / `--test write_lock` for iteration; run the full package suite once at the end.

---

### Task 0: Branch setup

**Files:** none (git only)

- [ ] **Step 1: Create the branch**

```bash
cd /Users/pmouli/GitHub.nosync/active/rust/infigraph
git checkout main && git checkout -b feat/lockfile-identity
```

- [ ] **Step 2: Verify clean base**

Run: `git status -sb`
Expected: `## feat/lockfile-identity` — untracked `docs/DESIGN-hardening.md`, `tsconfig.json` and modified `Cargo.lock` may be present; leave them alone and never `git add` them.

---

### Task 1: Compile-time build hash (`build.rs`)

**Files:**
- Create: `crates/infigraph-core/build.rs`
- Modify: `crates/infigraph-core/src/lib.rs` (add `pub fn build_hash()`)
- Test: `crates/infigraph-core/tests/lockfile.rs` (new file, first test)

**Interfaces:**
- Produces: `infigraph_core::build_hash() -> &'static str` — short git SHA, `-dirty` suffix when applicable, `"unknown"` when git unavailable. Consumed by Task 2's `LockInfo::current(role)`.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/tests/lockfile.rs`:

```rust
use infigraph_core::build_hash;

#[test]
fn test_build_hash_is_nonempty() {
    let h = build_hash();
    assert!(!h.is_empty());
    // In a git checkout this is a short sha, possibly "-dirty"; outside git it's "unknown".
    assert!(h == "unknown" || h.len() >= 7, "unexpected build hash: {h}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test lockfile -- test_build_hash 2>&1 | tail -5`
Expected: FAIL — `cannot find function build_hash` (compile error).

- [ ] **Step 3: Implement**

Create `crates/infigraph-core/build.rs`:

```rust
use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let hash = match sha {
        Some(s) if dirty => format!("{s}-dirty"),
        Some(s) => s,
        None => "unknown".to_string(),
    };
    println!("cargo:rustc-env=INFIGRAPH_BUILD_HASH={hash}");
    // Re-run when HEAD moves so the hash stays honest in dev loops.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
```

Add to `crates/infigraph-core/src/lib.rs` (near the other top-level pub items):

```rust
/// Short git SHA this binary was built from ("-dirty" when the tree had
/// uncommitted changes; "unknown" outside a git checkout). Stamped into
/// lock-file identity payloads so stale-binary holders are identifiable.
pub fn build_hash() -> &'static str {
    env!("INFIGRAPH_BUILD_HASH")
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p infigraph-core --test lockfile -- test_build_hash 2>&1 | tail -5`
Expected: `test test_build_hash_is_nonempty ... ok`

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/build.rs crates/infigraph-core/src/lib.rs crates/infigraph-core/tests/lockfile.rs
git commit --no-verify -m "feat: stamp compile-time build hash into infigraph-core

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: `LockInfo` payload + `Busy` error types

**Files:**
- Create: `crates/infigraph-core/src/lockfile.rs`
- Modify: `crates/infigraph-core/src/lib.rs` (add `pub mod lockfile;`)
- Test: `crates/infigraph-core/tests/lockfile.rs` (extend)

**Interfaces:**
- Produces:
  - `lockfile::LockInfo { pid: u32, role: String, build_hash: String, acquired_at: u64 }` (serde Serialize/Deserialize), `LockInfo::current(role: &str) -> LockInfo`
  - `lockfile::Busy { lock_path: PathBuf, holder: Option<LockInfo>, waited: Duration }` implementing `std::error::Error` + `Display`; retrievable from `anyhow::Error` via `.downcast_ref::<Busy>()`

- [ ] **Step 1: Write the failing tests**

Append to `crates/infigraph-core/tests/lockfile.rs`:

```rust
use infigraph_core::lockfile::{Busy, LockInfo};
use std::path::PathBuf;
use std::time::Duration;

#[test]
fn test_lockinfo_current_and_roundtrip() {
    let info = LockInfo::current("test-role");
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.role, "test-role");
    assert_eq!(info.build_hash, build_hash());
    assert!(info.acquired_at > 1_700_000_000, "acquired_at should be epoch seconds");

    let json = serde_json::to_string(&info).unwrap();
    let back: LockInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(back.pid, info.pid);
    assert_eq!(back.role, info.role);
}

#[test]
fn test_busy_display_names_holder() {
    let busy = Busy {
        lock_path: PathBuf::from("/tmp/x.lock"),
        holder: Some(LockInfo {
            pid: 4242,
            role: "infigraph watch".into(),
            build_hash: "abc123".into(),
            acquired_at: 0,
        }),
        waited: Duration::from_secs(30),
    };
    let msg = busy.to_string();
    assert!(msg.contains("4242"), "message should name holder pid: {msg}");
    assert!(msg.contains("infigraph watch"), "message should name role: {msg}");
    assert!(msg.contains("30"), "message should mention wait: {msg}");
}

#[test]
fn test_busy_display_unknown_holder() {
    let busy = Busy {
        lock_path: PathBuf::from("/tmp/x.lock"),
        holder: None,
        waited: Duration::from_secs(5),
    };
    let msg = busy.to_string();
    assert!(msg.contains("unknown"), "unknown holder should be stated: {msg}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test lockfile 2>&1 | tail -5`
Expected: FAIL — `could not find lockfile in infigraph_core` (compile error).

- [ ] **Step 3: Implement**

Create `crates/infigraph-core/src/lockfile.rs`:

```rust
//! Lock-file mechanics shared by all infigraph locks (graph write lock,
//! and future operation/session/registry locks).
//!
//! Model: a kernel advisory flock (via `fs2`) is the source of truth for
//! "held" — flocks release automatically when the holder dies, so no
//! liveness polling is needed. The JSON identity payload written into the
//! lock file exists for diagnostics (who holds it, since when, built from
//! what) and is never trusted for liveness decisions. Conservative rule:
//! a held flock with an unreadable payload is an *unknown holder* —
//! bounded-wait then `Busy`, never broken.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

/// Identity payload stamped into a held lock file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub role: String,
    pub build_hash: String,
    /// Unix epoch seconds at acquisition.
    pub acquired_at: u64,
}

impl LockInfo {
    pub fn current(role: &str) -> Self {
        Self {
            pid: std::process::id(),
            role: role.to_string(),
            build_hash: crate::build_hash().to_string(),
            acquired_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

/// Returned when a lock could not be acquired within the wait budget.
#[derive(Debug)]
pub struct Busy {
    pub lock_path: PathBuf,
    pub holder: Option<LockInfo>,
    pub waited: Duration,
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.holder {
            Some(h) => {
                let held_for = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs().saturating_sub(h.acquired_at))
                    .unwrap_or(0);
                write!(
                    f,
                    "{} is locked by {} (PID {}), held {}s — waited {}s, giving up",
                    self.lock_path.display(),
                    h.role,
                    h.pid,
                    held_for,
                    self.waited.as_secs()
                )
            }
            None => write!(
                f,
                "{} is locked by an unknown holder — waited {}s, giving up",
                self.lock_path.display(),
                self.waited.as_secs()
            ),
        }
    }
}

impl std::error::Error for Busy {}
```

Add to `crates/infigraph-core/src/lib.rs` alongside the other `pub mod` lines:

```rust
pub mod lockfile;
```

Also add `use infigraph_core::build_hash;`-compatible import in the test file if missing (Step 1 already references `build_hash()` from Task 1's import).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test lockfile 2>&1 | tail -6`
Expected: 4 tests pass (Task 1's + these three).

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/lockfile.rs crates/infigraph-core/src/lib.rs crates/infigraph-core/tests/lockfile.rs
git commit --no-verify -m "feat: LockInfo identity payload and structured Busy error for lock files

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: `try_acquire` with payload stamping

**Files:**
- Modify: `crates/infigraph-core/src/lockfile.rs`
- Test: `crates/infigraph-core/tests/lockfile.rs` (extend)

**Interfaces:**
- Produces:
  - `lockfile::LockFile` — RAII guard; flock released + payload cleared on drop
  - `lockfile::try_acquire(path: &Path, role: &str) -> Result<Option<LockFile>>` — `None` when held by another *open file description* (cross-process or a second handle in-process); stamps payload on success
  - `lockfile::read_holder(path: &Path) -> Option<LockInfo>` — best-effort payload read (used by `Busy` construction and future health beacons)

- [ ] **Step 1: Write the failing tests**

Append to `crates/infigraph-core/tests/lockfile.rs`:

```rust
use infigraph_core::lockfile;
use tempfile::TempDir;

#[test]
fn test_try_acquire_stamps_identity() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.lock");
    let guard = lockfile::try_acquire(&path, "unit-test").unwrap().expect("free lock");
    let holder = lockfile::read_holder(&path).expect("payload written");
    assert_eq!(holder.pid, std::process::id());
    assert_eq!(holder.role, "unit-test");
    assert_eq!(holder.build_hash, build_hash());
    drop(guard);
}

#[test]
fn test_try_acquire_none_when_held() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("b.lock");
    let _guard = lockfile::try_acquire(&path, "first").unwrap().expect("free lock");
    let second = lockfile::try_acquire(&path, "second").unwrap();
    assert!(second.is_none(), "second handle must not acquire a held lock");
}

#[test]
fn test_release_clears_payload_and_reacquire_works() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("c.lock");
    {
        let _guard = lockfile::try_acquire(&path, "first").unwrap().expect("free lock");
    }
    // After clean release the payload is cleared (empty file), and the lock is free.
    assert!(lockfile::read_holder(&path).is_none(), "payload should clear on drop");
    let again = lockfile::try_acquire(&path, "second").unwrap();
    assert!(again.is_some(), "lock should be reacquirable after drop");
}

#[test]
fn test_stale_payload_without_flock_is_adopted() {
    // Simulates a holder that died without cleanup (kernel released the
    // flock; stale JSON remains). Acquisition must succeed and overwrite.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("d.lock");
    std::fs::write(
        &path,
        r#"{"pid":999999,"role":"dead","build_hash":"x","acquired_at":1}"#,
    )
    .unwrap();
    let guard = lockfile::try_acquire(&path, "adopter").unwrap();
    assert!(guard.is_some(), "free flock with stale payload must be adopted");
    let holder = lockfile::read_holder(&path).unwrap();
    assert_eq!(holder.role, "adopter");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test lockfile 2>&1 | tail -5`
Expected: FAIL — `cannot find function try_acquire` (compile error).

- [ ] **Step 3: Implement**

Append to `crates/infigraph-core/src/lockfile.rs`:

```rust
/// RAII guard for a held lock file. Releasing (drop) truncates the payload
/// then unlocks, so a cleanly-released lock file is empty.
pub struct LockFile {
    file: File,
    path: PathBuf,
}

impl LockFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        // Best-effort: clear payload before the flock releases so readers
        // never see a stale identity on a free lock we released cleanly.
        let _ = self.file.set_len(0);
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?)
}

fn stamp(file: &mut File, role: &str) -> Result<()> {
    let info = LockInfo::current(role);
    let json = serde_json::to_string(&info)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(json.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Best-effort read of the current holder's identity. `None` when the file
/// is missing, empty, or unparseable (old binary / mid-write).
pub fn read_holder(path: &Path) -> Option<LockInfo> {
    let mut buf = String::new();
    File::open(path).ok()?.read_to_string(&mut buf).ok()?;
    serde_json::from_str(buf.trim()).ok()
}

/// Non-blocking acquisition. `Ok(None)` when another open file description
/// holds the flock. On success the identity payload is stamped.
pub fn try_acquire(path: &Path, role: &str) -> Result<Option<LockFile>> {
    let mut file = open_lock_file(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            stamp(&mut file, role)?;
            Ok(Some(LockFile { file, path: path.to_path_buf() }))
        }
        Err(ref e)
            if e.kind() == std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(33) =>
        {
            Ok(None)
        }
        Err(e) => Err(anyhow::anyhow!("lock error on {}: {e}", path.display())),
    }
}
```

(The `Some(33)` arm preserves the existing Windows `ERROR_LOCK_VIOLATION` handling from `WriteLock::try_acquire`.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test lockfile 2>&1 | tail -6`
Expected: 8 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/lockfile.rs crates/infigraph-core/tests/lockfile.rs
git commit --no-verify -m "feat: lockfile try_acquire with identity stamping and stale-payload adoption

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: Bounded-wait `acquire` returning `Busy`

**Files:**
- Modify: `crates/infigraph-core/src/lockfile.rs`
- Test: `crates/infigraph-core/tests/lockfile.rs` (extend)

**Interfaces:**
- Produces: `lockfile::acquire(path: &Path, role: &str, timeout: Duration) -> Result<LockFile>` — polls with backoff (50 ms doubling to 500 ms cap); on expiry returns `Err` whose `.downcast_ref::<Busy>()` yields holder identity read best-effort from the payload.

- [ ] **Step 1: Write the failing tests**

Append to `crates/infigraph-core/tests/lockfile.rs`:

```rust
#[test]
fn test_acquire_waits_then_succeeds() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("e.lock");
    let guard = lockfile::try_acquire(&path, "short-holder").unwrap().expect("free");
    let path2 = path.clone();
    let t = std::thread::spawn(move || {
        // Holder releases after 200ms; waiter has a 5s budget.
        std::thread::sleep(Duration::from_millis(200));
        drop(guard);
    });
    let acquired = lockfile::acquire(&path2, "waiter", Duration::from_secs(5)).unwrap();
    t.join().unwrap();
    assert_eq!(lockfile::read_holder(&path2).unwrap().role, "waiter");
    drop(acquired);
}

#[test]
fn test_acquire_times_out_with_busy_naming_holder() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("f.lock");
    let _guard = lockfile::try_acquire(&path, "long-holder").unwrap().expect("free");
    let err = lockfile::acquire(&path, "impatient", Duration::from_millis(300))
        .expect_err("must time out while held");
    let busy = err.downcast_ref::<Busy>().expect("error must be Busy");
    let holder = busy.holder.as_ref().expect("holder identity readable");
    assert_eq!(holder.role, "long-holder");
    assert_eq!(holder.pid, std::process::id());
    assert!(busy.waited >= Duration::from_millis(300));
}

#[test]
fn test_acquire_timeout_unknown_holder_on_bare_flock() {
    // Old-binary compatibility: flock held but no payload (pre-identity
    // binaries never write one). Must time out as unknown holder, never break.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("g.lock");
    let bare = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    fs2::FileExt::lock_exclusive(&bare).unwrap();
    let err = lockfile::acquire(&path, "modern", Duration::from_millis(200))
        .expect_err("must time out");
    let busy = err.downcast_ref::<Busy>().expect("error must be Busy");
    assert!(busy.holder.is_none(), "bare flock has unknown holder");
    fs2::FileExt::unlock(&bare).unwrap();
}
```

Add `fs2 = "0.4.3"` and `tempfile` to `[dev-dependencies]` of `crates/infigraph-core/Cargo.toml` **only if missing** (tempfile already is a dev-dep — the existing `write_lock.rs` uses it; fs2 is a normal dep so `use fs2::FileExt` works in tests via the crate's re-export — if the test can't resolve `fs2`, add `fs2 = "0.4.3"` under `[dev-dependencies]`).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test lockfile 2>&1 | tail -5`
Expected: FAIL — `cannot find function acquire` (compile error).

- [ ] **Step 3: Implement**

Append to `crates/infigraph-core/src/lockfile.rs`:

```rust
/// Blocking acquisition with a wait budget. Polls `try_acquire` with
/// backoff (50ms doubling to a 500ms cap). On expiry returns a `Busy`
/// error carrying the holder identity when the payload is readable.
pub fn acquire(path: &Path, role: &str, timeout: Duration) -> Result<LockFile> {
    let start = Instant::now();
    let mut delay = Duration::from_millis(50);
    loop {
        if let Some(guard) = try_acquire(path, role)? {
            return Ok(guard);
        }
        if start.elapsed() >= timeout {
            return Err(anyhow::Error::new(Busy {
                lock_path: path.to_path_buf(),
                holder: read_holder(path),
                waited: start.elapsed(),
            }));
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        std::thread::sleep(delay.min(remaining));
        delay = (delay * 2).min(Duration::from_millis(500));
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test lockfile 2>&1 | tail -6`
Expected: 11 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/lockfile.rs crates/infigraph-core/tests/lockfile.rs crates/infigraph-core/Cargo.toml
git commit --no-verify -m "feat: bounded-wait lock acquisition with Busy error naming the holder

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: `WriteLock` delegates to `lockfile`

**Files:**
- Modify: `crates/infigraph-core/src/graph/store.rs:11-51` (the `WriteLock` struct + impl)
- Test: `crates/infigraph-core/tests/write_lock.rs` (one new test; existing tests unchanged)

**Interfaces:**
- Consumes: `lockfile::{acquire, try_acquire, LockFile, Busy}` from Tasks 3–4.
- Produces: `WriteLock` public API unchanged for callers (`store.write_lock()`, `store.try_write_lock()`), but `write_lock()` now has a 30 s wait budget and can fail with `Busy` instead of blocking forever. All existing call sites (`kuzu_backend.rs`, `store_write.rs`, `store_parquet.rs`, `scip/mod.rs`, `resolve/calls.rs`, `multi/combined.rs`, `store.rs` itself) compile unchanged because signatures are identical (`Result<WriteLock>` / `Result<Option<WriteLock>>`).

- [ ] **Step 1: Write the failing test**

Append to `crates/infigraph-core/tests/write_lock.rs`:

```rust
#[test]
fn test_write_lock_stamps_identity_and_busy_on_timeout() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("identity.db");
    let store = GraphStore::open(&db_path).unwrap();

    let _held = store.write_lock().unwrap();
    let holder = infigraph_core::lockfile::read_holder(&db_path.with_extension("lock"))
        .expect("write lock should stamp identity");
    assert_eq!(holder.pid, std::process::id());
    assert_eq!(holder.role, "graph-write");

    // A second store on the same path must time out with Busy, not hang.
    let store2 = GraphStore::open(&db_path); // may fail: kuzu holds its own db lock
    if let Ok(store2) = store2 {
        let err = store2
            .write_lock_with_timeout(std::time::Duration::from_millis(300))
            .expect_err("held lock must yield Busy");
        assert!(
            err.downcast_ref::<infigraph_core::lockfile::Busy>().is_some(),
            "expected Busy, got: {err}"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test write_lock -- stamps_identity 2>&1 | tail -5`
Expected: FAIL — `no method named write_lock_with_timeout` (compile error).

- [ ] **Step 3: Implement**

In `crates/infigraph-core/src/graph/store.rs`, replace the `WriteLock` struct and impl (current lines 11–51) with:

```rust
use crate::lockfile::{self, LockFile};

/// RAII guard for exclusive write access to the graph store.
/// Holds an advisory file lock on `<db_path>.lock` with an identity
/// payload (see `crate::lockfile`).
pub struct WriteLock {
    _guard: LockFile,
}

/// Role string stamped into the graph write lock's identity payload.
const GRAPH_WRITE_ROLE: &str = "graph-write";

/// Default wait budget for the graph write lock. Individual write calls
/// are short; 30s of waiting means something is wedged — surface it.
const GRAPH_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl WriteLock {
    fn acquire(lock_path: &Path) -> Result<Self> {
        Self::acquire_with_timeout(lock_path, GRAPH_WRITE_TIMEOUT)
    }

    fn acquire_with_timeout(lock_path: &Path, timeout: std::time::Duration) -> Result<Self> {
        let guard = lockfile::acquire(lock_path, GRAPH_WRITE_ROLE, timeout)?;
        Ok(Self { _guard: guard })
    }

    fn try_acquire(lock_path: &Path) -> Result<Option<Self>> {
        Ok(lockfile::try_acquire(lock_path, GRAPH_WRITE_ROLE)?.map(|guard| Self { _guard: guard }))
    }
}
```

And in the `GraphStore` impl, alongside the existing `write_lock`/`try_write_lock` (lines 85–93), add:

```rust
    /// Acquire the write lock with a caller-chosen wait budget.
    pub fn write_lock_with_timeout(&self, timeout: std::time::Duration) -> Result<WriteLock> {
        WriteLock::acquire_with_timeout(&self.lock_path, timeout)
    }
```

Remove the now-unused `use fs2::FileExt;` from `store.rs` **only if** nothing else in the file uses it (check first — `open_read_only`/other code may not; the compiler will say).

- [ ] **Step 4: Run the new test and the full existing write-lock suites**

Run: `cargo test -p infigraph-core --test write_lock --test write_lock_edge_cases --test write_lock_wiring --test lockfile 2>&1 | tail -8`
Expected: all tests pass — the existing suites prove behavioral compatibility (the old `test_write_lock_cross_thread_blocking` still passes because 30 s ≫ test hold times).

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/graph/store.rs crates/infigraph-core/tests/write_lock.rs
git commit --no-verify -m "feat: WriteLock adopts lockfile identity + bounded wait (30s default, Busy on expiry)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Full-suite verification + PR prep

**Files:** none new

- [ ] **Step 1: Format only the files this branch touched**

```bash
rustfmt crates/infigraph-core/src/lockfile.rs crates/infigraph-core/src/graph/store.rs crates/infigraph-core/build.rs crates/infigraph-core/tests/lockfile.rs
git diff --stat   # if formatting changed anything, amend it into the last commit:
git add -u && git commit --no-verify --amend --no-edit
```

- [ ] **Step 2: Run the full infigraph-core suite**

Run: `cargo test -p infigraph-core 2>&1 | tail -15`
Expected: all suites pass (several minutes; Kuzu-backed tests dominate). Any failure in a suite this branch didn't touch → check it fails on `main` too before investigating (pre-existing vs regression).

- [ ] **Step 3: Clippy on the touched crate**

Run: `cargo clippy -p infigraph-core --tests 2>&1 | tail -10`
Expected: no new warnings in `lockfile.rs`/`store.rs`/`build.rs`. Pre-existing warnings elsewhere are out of scope (I-11).

- [ ] **Step 4: Push and confirm branch state**

```bash
git push -u origin feat/lockfile-identity
git log --oneline main..feat/lockfile-identity
```

Expected: 5 commits (Tasks 1–5). Opening the upstream PR is a human step after review.

---

## Follow-on plans (not in this document)

Per the spec's dependency table, each subsequent PR gets its own plan authored when its turn comes, consuming this PR's interfaces:

- **PR 2** (`feat/write-lock-enforcement`): consumes `WriteLock`, adds witness params + locks `init_schema`/recovery paths + surfaces WAL-replay failures.
- **PR 3** (`feat/index-operation-lock`): consumes `lockfile::{acquire, try_acquire, read_holder}` for `.infigraph/index.lock` + coalescing.
- **PR 4** (`feat/shared-state-write-safety`): consumes `lockfile` for `sessions.lock`/`registry.lock` + atomic temp-rename saves.
- **PR 6** (`feat/health-beacons`): consumes `read_holder` + worker-start timestamp for degraded-only footers.
- **PR 5** (`docs/worker-architecture-rfc`): authoring task, references PR 1's identity format.
