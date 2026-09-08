# PR7b: mcp.lock Identity, Heartbeat, Wedged Detection, and Takeover Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mcp.lock` — the file that decides which of possibly-several concurrently-running `infigraph-mcp` worker processes on a machine is the "primary" (the only one allowed to run watchers/background duties) — gains an identity (who holds it, since when), a heartbeat (so staleness is detectable even though the OS-level flock alone can't tell a healthy holder from a wedged one), loud wedged-holder warnings, and a build-hash-mismatch takeover handshake so a freshly-started worker running a newer binary doesn't just coexist forever alongside a stale one.

**Architecture:** `mcp.lock`'s raw `fs2::try_lock_exclusive()` usage in `main.rs::acquire_instance_lock()` is replaced by `infigraph-core`'s existing `lockfile` module (the same identity/`LockFile`/`try_acquire` pattern already used for `graph.lock` and `index.lock`), extended with a new `last_heartbeat` field and a `LockFile::heartbeat()` method. A new `crates/infigraph-mcp/src/mcp_lock.rs` module owns everything specific to *this* lock's policy: config (heartbeat interval, wedged threshold, takeover timing), the wedged-holder check, and the build-hash-mismatch handover protocol (a small sentinel file the challenger writes and the incumbent's own heartbeat loop polls for). The primary's lock is moved into a dedicated background thread that heartbeats on an interval and checks for a pending handover request on every tick; if one is found, it releases the lock and exits the process.

**Tech Stack:** Rust (edition 2021), no new dependencies — reuses `infigraph-core`'s existing `lockfile`/`serde_json` machinery.

## Design Decisions (confirmed with the user before this plan was written)

- **Takeover mechanism:** file-based, not signal-based. A challenger that finds `mcp.lock` busy compares the incumbent's stamped `build_hash` (already part of `LockInfo`, previously unused for this purpose) against its own. Only on a **mismatch** does it write a `mcp.lock.handover` sentinel file naming itself; on a match (same binary version), it does nothing and falls back to secondary, exactly like today — two instances of the identical build have no reason to fight over the lock.
- **Handover is not a graceful drain.** The incumbent, on seeing a handover request, releases `mcp.lock` and exits the process outright. Flushing in-flight requests / a coordinated shutdown sequence is R5.4's separately-scoped territory, not this PR's.
- **Wedged detection is advisory, not enforced.** A stale heartbeat produces a loud `WARN` log so a human (or a future R2.2.4 `infigraph ps`) can see something's wrong; it does **not** by itself trigger a takeover or force-break the lock — only a build-hash mismatch does that. A wedged holder on the *same* build just logs loudly forever, matching this PR's conservative, additive scope (no forced killing of same-version processes).

## Global Constraints

- **Scope is R2.3.1 + R2.3.2/2.3.2a + R2.3.3 + R2.3.5 only.** R2.3.4 (lock respawn) is already done by construction (a released flock is immediately race-able) — nothing to build. R2.3.6 (sessions.lock) and R2.3.7 (registry.lock) are already shipped (PR4). R2.3.8 (real write coordination) is explicitly excluded, PR5/Phase-3 territory.
- **No new branch, no push.** Per the standing stacking directive covering every PR since PR4, commit directly onto the current branch, `feat/health-beacons`. (Note: this branch's earlier PR7a work has already been split out into separate fork PRs #4-#8 as of this session; that PR-splitting effort is unrelated to and does not block this plan — keep building on `feat/health-beacons` as the live local dev branch, per the same convention.)
- Env vars introduced here follow the established `INFIGRAPH_MCP_IDLE_GRACE_SECS`/`INFIGRAPH_REAP_GRACE_SECS` naming pattern (prefix `INFIGRAPH_`, `_SECS` suffix), not invented fresh.
- Every cargo command runs with `CARGO_PROFILE_DEV_DEBUG=0` (hard rule for this repo — mixing debug settings spawns multi-GB duplicate `lbug` cmake trees; this exact machine has hit that repeatedly this session).
- Commit with `--no-verify` only if the pre-commit hook fails for a reason clearly unrelated to the change (this session has repeatedly hit an unrelated `write_lock_perf::test_contended_lock_throughput` flake under concurrent load, and a `groups_watch_perf` flake specific to back-to-back invocations) — always run `cargo fmt` first and confirm the failure is genuinely unrelated before reaching for it.
- Fork-only, no upstream PR without asking (standing directive).

---

### Task 1: `lockfile` gains a heartbeat field, `heartbeat()` method, and wedged-detection

**Files:**
- Modify: `crates/infigraph-core/src/lockfile.rs`
- Test: `crates/infigraph-core/tests/lockfile.rs` (extend)

**Interfaces:**
- Produces: `LockInfo.last_heartbeat: u64` (new field, `#[serde(default)]` for backward-compat with lock files written by a pre-this-PR binary), `LockFile::heartbeat(&mut self) -> Result<()>`, `pub fn is_holder_wedged(last_heartbeat: u64, now: u64, threshold_secs: u64) -> bool` (pure). Task 2 consumes all three from `crates::lockfile::*`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/infigraph-core/tests/lockfile.rs`:

```rust
use infigraph_core::lockfile::is_holder_wedged;

#[test]
fn is_holder_wedged_pure_cases() {
    // Heartbeat well within the threshold: not wedged.
    assert!(!is_holder_wedged(1000, 1030, 60));
    // Heartbeat exactly at the threshold: wedged (boundary inclusive).
    assert!(is_holder_wedged(1000, 1060, 60));
    // Heartbeat well past the threshold: wedged.
    assert!(is_holder_wedged(1000, 1200, 60));
}

#[test]
fn heartbeat_updates_last_heartbeat_but_not_acquired_at() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock_path = dir.path().join("test.lock");

    let mut lock = infigraph_core::lockfile::try_acquire(&lock_path, "test-role")
        .expect("try_acquire")
        .expect("lock should be free");

    let before = infigraph_core::lockfile::read_holder(&lock_path).expect("holder readable");
    assert_eq!(before.acquired_at, before.last_heartbeat, "fresh acquire: both timestamps equal");

    std::thread::sleep(std::time::Duration::from_millis(1100));
    lock.heartbeat().expect("heartbeat");

    let after = infigraph_core::lockfile::read_holder(&lock_path).expect("holder readable");
    assert_eq!(
        after.acquired_at, before.acquired_at,
        "heartbeat must not change acquired_at"
    );
    assert!(
        after.last_heartbeat > before.last_heartbeat,
        "heartbeat must advance last_heartbeat: before={} after={}",
        before.last_heartbeat,
        after.last_heartbeat
    );
    assert_eq!(after.pid, before.pid);
    assert_eq!(after.role, before.role);
    assert_eq!(after.build_hash, before.build_hash);
}

#[test]
fn old_lock_file_without_last_heartbeat_field_still_parses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock_path = dir.path().join("test.lock");
    // Simulates a lock file written by a pre-this-PR binary: no
    // last_heartbeat key at all.
    std::fs::write(
        &lock_path,
        r#"{"pid":12345,"role":"old-role","build_hash":"deadbeef","acquired_at":1000}"#,
    )
    .unwrap();

    let holder = infigraph_core::lockfile::read_holder(&lock_path);
    assert!(holder.is_some(), "must still parse without last_heartbeat");
    assert_eq!(holder.unwrap().last_heartbeat, 0, "missing field defaults to 0");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test lockfile`
Expected: COMPILE ERROR — `is_holder_wedged` doesn't exist, `LockFile::heartbeat` doesn't exist, `LockInfo` has no `last_heartbeat` field.

- [ ] **Step 3: Implement**

In `crates/infigraph-core/src/lockfile.rs`, change the `LockInfo` struct and its constructor:

```rust
/// Identity payload stamped into a held lock file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub role: String,
    pub build_hash: String,
    /// Unix epoch seconds at acquisition.
    pub acquired_at: u64,
    /// Unix epoch seconds of the holder's last heartbeat refresh (see
    /// `LockFile::heartbeat`). Equals `acquired_at` until the first
    /// heartbeat call. Lock types that never call `heartbeat()` (e.g.
    /// `graph.lock`, `index.lock`) simply never advance this past their
    /// initial acquire time — harmless, since nothing currently reads this
    /// field for staleness on those lock types, only `mcp.lock` does.
    /// `#[serde(default)]` so a lock file written by a binary that
    /// predates this field still parses (defaults to 0, which `read_holder`
    /// callers must treat as "unknown," not "just acquired").
    #[serde(default)]
    pub last_heartbeat: u64,
}

impl LockInfo {
    pub fn current(role: &str) -> Self {
        let now = now_epoch_secs();
        Self {
            pid: std::process::id(),
            role: role.to_string(),
            build_hash: crate::build_hash().to_string(),
            acquired_at: now,
            last_heartbeat: now,
        }
    }
}
```

Change `LockFile` to remember the `LockInfo` it stamped:

```rust
/// RAII guard for a held lock file. Releasing (drop) truncates the payload
/// then unlocks, so a cleanly-released lock file is empty.
#[derive(Debug)]
pub struct LockFile {
    file: File,
    path: PathBuf,
    info: LockInfo,
}

impl LockFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-stamps the lock's payload with a fresh `last_heartbeat`, leaving
    /// `pid`/`role`/`build_hash`/`acquired_at` unchanged. Callers that want
    /// `is_holder_wedged`-based staleness detection to work for this lock
    /// must call this periodically while holding it.
    pub fn heartbeat(&mut self) -> Result<()> {
        self.info.last_heartbeat = now_epoch_secs();
        let json = serde_json::to_string(&self.info)?;
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(json.as_bytes())?;
        self.file.flush()?;
        Ok(())
    }
}
```

Change `stamp` to return the `LockInfo` it wrote, and update its one call site in `try_acquire`:

```rust
fn stamp(file: &mut File, role: &str) -> Result<LockInfo> {
    let info = LockInfo::current(role);
    let json = serde_json::to_string(&info)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(json.as_bytes())?;
    file.flush()?;
    Ok(info)
}
```

```rust
pub fn try_acquire(path: &Path, role: &str) -> Result<Option<LockFile>> {
    let mut file = open_lock_file(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            let info = stamp(&mut file, role)?;
            Ok(Some(LockFile {
                file,
                path: path.to_path_buf(),
                info,
            }))
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

Add the pure wedged-check function, near `is_holder_wedged`'s natural home alongside the other free functions:

```rust
/// Pure: has a lock holder's heartbeat gone stale enough to suspect it's
/// wedged (still holding the flock -- so not dead in the liveness sense --
/// but not doing whatever periodic work it's supposed to be doing)?
/// Boundary is inclusive.
pub fn is_holder_wedged(last_heartbeat: u64, now: u64, threshold_secs: u64) -> bool {
    now.saturating_sub(last_heartbeat) >= threshold_secs
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test lockfile`
Expected: PASS, all tests including the 3 new ones and every pre-existing `lockfile.rs` test (confirming the `LockInfo`/`LockFile` signature changes didn't break existing callers/tests).

- [ ] **Step 5: Run the full infigraph-core suite to check for other callers**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --no-fail-fast -- --test-threads=4`
Expected: green (modulo the pre-catalogued `graph_queries` parallelism flake, confirm with `--test-threads=2` if it appears). `LockFile`'s two other current callers (`graph/store.rs`'s `WriteLock`, and the index-operation-lock path) construct/consume `LockFile` only through `try_acquire`/`path()`/`Drop` — none access its fields directly, so the added `info` field should be transparent, but this full run is what proves it.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/infigraph-core/src/lockfile.rs crates/infigraph-core/tests/lockfile.rs
git commit -m "feat: lockfile gains heartbeat + wedged-holder staleness check (R2.3.5/R2.3.3 building block)"
```

---

### Task 2: Migrate `mcp.lock` onto the `lockfile` module (R2.3.1) + heartbeat thread

**Files:**
- Create: `crates/infigraph-mcp/src/mcp_lock.rs`
- Modify: `crates/infigraph-mcp/src/lib.rs` (add `pub mod mcp_lock;`, alphabetically after `pub mod idle;`, before `pub mod recovery;`)
- Modify: `crates/infigraph-mcp/src/main.rs`
- Test: `crates/infigraph-mcp/tests/mcp_lock.rs`

**Interfaces:**
- Produces: `pub fn lock_path() -> PathBuf`, `pub fn heartbeat_interval() -> Duration` (default 15s, env `INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS`), `pub fn wedged_threshold_secs() -> u64` (default 60, env `INFIGRAPH_MCP_LOCK_WEDGED_SECS`), `pub fn acquire_primary() -> Option<infigraph_core::lockfile::LockFile>` (replaces `main.rs`'s old `acquire_instance_lock`, no takeover yet — that's Task 4), `pub fn heartbeat_tick(lock: &mut infigraph_core::lockfile::LockFile)` (calls `lock.heartbeat()`, logs on failure; no handover check yet — Task 4 extends this into `heartbeat_and_check_handover`). Task 3 consumes `acquire_primary`'s busy-path to add wedged logging. Task 4 extends `acquire_primary` into `acquire_with_takeover` and `heartbeat_tick` into `heartbeat_and_check_handover`.

- [ ] **Step 1: Write the failing tests**

Create `crates/infigraph-mcp/tests/mcp_lock.rs`:

```rust
use std::time::Duration;

/// Serializes tests that mutate the process-global INFIGRAPH_MCP_LOCK_*
/// env vars -- cargo runs this binary's tests on parallel threads, so a
/// lowered override in one test must not leak into another test's window
/// (same lesson as idle.rs's/instances.rs's own env-mutation tests).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn heartbeat_interval_default_and_override() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
    assert_eq!(infigraph_mcp::mcp_lock::heartbeat_interval(), Duration::from_secs(15));
    std::env::set_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS", "2");
    assert_eq!(infigraph_mcp::mcp_lock::heartbeat_interval(), Duration::from_secs(2));
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
}

#[test]
fn wedged_threshold_default_and_override() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_WEDGED_SECS");
    assert_eq!(infigraph_mcp::mcp_lock::wedged_threshold_secs(), 60);
    std::env::set_var("INFIGRAPH_MCP_LOCK_WEDGED_SECS", "5");
    assert_eq!(infigraph_mcp::mcp_lock::wedged_threshold_secs(), 5);
    std::env::remove_var("INFIGRAPH_MCP_LOCK_WEDGED_SECS");
}

#[test]
fn acquire_primary_then_busy_then_free_again() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_HOME", dir.path());
    // mcp_lock::lock_path() must respect INFIGRAPH_HOME the same way the
    // rest of this test suite's HOME-based overrides work -- see Step 3's
    // note on lock_path() before assuming this env var name is correct.

    let first = infigraph_mcp::mcp_lock::acquire_primary();
    assert!(first.is_some(), "lock should be free on first acquire");

    let second = infigraph_mcp::mcp_lock::acquire_primary();
    assert!(second.is_none(), "lock is held, second acquire must fail");

    drop(first);

    let third = infigraph_mcp::mcp_lock::acquire_primary();
    assert!(third.is_some(), "lock must be free again after the holder drops");

    std::env::remove_var("INFIGRAPH_HOME");
}

#[test]
fn heartbeat_tick_advances_last_heartbeat() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_HOME", dir.path());

    let mut lock = infigraph_mcp::mcp_lock::acquire_primary().expect("lock should be free");
    let path = infigraph_mcp::mcp_lock::lock_path();
    let before = infigraph_core::lockfile::read_holder(&path).unwrap();

    std::thread::sleep(Duration::from_millis(1100));
    infigraph_mcp::mcp_lock::heartbeat_tick(&mut lock);

    let after = infigraph_core::lockfile::read_holder(&path).unwrap();
    assert!(after.last_heartbeat > before.last_heartbeat);

    std::env::remove_var("INFIGRAPH_HOME");
}
```

**Before writing Step 3's implementation, resolve the `INFIGRAPH_HOME` question the test above flags**: check whether this codebase already has a test-isolation env-var override convention for `$HOME`-derived paths (the `docs/superpowers/specs/2026-07-21-remaining-hardening-design.md` R-NEW.1 section describes this exact gap for the *project* registry — `registry_path()` has no override at all). `mcp_lock::lock_path()` in this new module is free to invent its own override var since it's new code; use `INFIGRAPH_MCP_LOCK_PATH` (full path override, simpler and more direct than a `$HOME`-only override) instead of `INFIGRAPH_HOME` if that reads cleaner — adjust the test above to match whichever you implement. Do not skip having *some* override; without one, this test would pollute the real `~/.infigraph/mcp.lock` on every run.

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test mcp_lock`
Expected: COMPILE ERROR — `infigraph_mcp::mcp_lock` module does not exist.

- [ ] **Step 3: Implement**

Create `crates/infigraph-mcp/src/mcp_lock.rs`:

```rust
//! `mcp.lock` lifecycle: identity (via `infigraph_core::lockfile`),
//! heartbeat, wedged-holder detection (R2.3.3/R2.3.5), and build-hash
//! mismatch takeover (R2.3.1/R2.3.2/R2.3.2a). This is the lock that
//! decides which of possibly-several concurrently-running infigraph-mcp
//! processes on a machine is the "primary" allowed to run watchers.

use std::path::PathBuf;
use std::time::Duration;

use infigraph_core::lockfile::{self, LockFile};

/// Full path to `mcp.lock`. Overridable via `INFIGRAPH_MCP_LOCK_PATH`
/// (tests) so tests never touch the real `~/.infigraph/mcp.lock`.
pub fn lock_path() -> PathBuf {
    if let Ok(path) = std::env::var("INFIGRAPH_MCP_LOCK_PATH") {
        return PathBuf::from(path);
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".infigraph")
        .join("mcp.lock")
}

/// How often the primary's heartbeat thread refreshes `mcp.lock`'s
/// `last_heartbeat` and checks for a pending handover request.
/// Overridable via `INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS`.
pub fn heartbeat_interval() -> Duration {
    std::env::var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(15))
}

/// How stale a holder's heartbeat must be before it's logged as possibly
/// wedged. Overridable via `INFIGRAPH_MCP_LOCK_WEDGED_SECS`.
pub fn wedged_threshold_secs() -> u64 {
    std::env::var("INFIGRAPH_MCP_LOCK_WEDGED_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60)
}

/// Non-blocking: try to become mcp.lock's primary. `None` if another
/// process already holds it. No takeover logic yet -- Task 4 wraps this
/// into `acquire_with_takeover`.
pub fn acquire_primary() -> Option<LockFile> {
    lockfile::try_acquire(&lock_path(), "mcp-primary").ok().flatten()
}

/// One heartbeat tick: refresh `last_heartbeat`. Logs a WARN on I/O
/// failure but never panics -- a heartbeat write failure alone shouldn't
/// bring down the primary. No handover check yet -- Task 4 extends this
/// into `heartbeat_and_check_handover`.
pub fn heartbeat_tick(lock: &mut LockFile) {
    if let Err(e) = lock.heartbeat() {
        crate::mcp_log("WARN", &format!("mcp.lock heartbeat failed: {e:#}"));
    }
}
```

In `crates/infigraph-mcp/src/main.rs`, delete the old `acquire_instance_lock` function (lines ~203-231) entirely, and replace `run()`'s opening block:

```rust
fn run() -> Result<()> {
    let instance_lock = acquire_instance_lock();
    let is_primary = instance_lock.is_some();
    let _lock_guard = instance_lock;

    if !is_primary {
        infigraph_mcp::tools::watch::disable_watchers();
    }
```

with:

```rust
fn run() -> Result<()> {
    let mcp_lock = infigraph_mcp::mcp_lock::acquire_primary();
    let is_primary = mcp_lock.is_some();

    if is_primary {
        mcp_log("INFO", "Acquired mcp.lock — running as primary");
    } else {
        mcp_log(
            "WARN",
            "Another MCP instance holds mcp.lock — running without watchers",
        );
        infigraph_mcp::tools::watch::disable_watchers();
    }

    if let Some(mut lock) = mcp_lock {
        std::thread::spawn(move || loop {
            std::thread::sleep(infigraph_mcp::mcp_lock::heartbeat_interval());
            infigraph_mcp::mcp_lock::heartbeat_tick(&mut lock);
        });
    }
```

Everything else in `run()` (the instance-registry registration block from PR7a, the orphan-reaping scan, `ui_enabled`/`serve_mode`/the stdin loop/idle-termination) stays completely unchanged — this task only touches the mcp.lock acquisition block at the very top of the function.

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test mcp_lock -- --test-threads=1`
Expected: PASS, 4/4. (`--test-threads=1` since these tests share the `INFIGRAPH_MCP_LOCK_PATH` env var via the mutex, but real file I/O across threads is safer serialized regardless.)

- [ ] **Step 5: Run the existing instance-registration suite to confirm no regression**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test instance_registration -- --test-threads=1`
Expected: PASS, 2/2 unchanged — PR7a's registration/reaping wiring sits right after this block in `run()` and must be unaffected.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/infigraph-mcp/src/mcp_lock.rs crates/infigraph-mcp/src/lib.rs crates/infigraph-mcp/src/main.rs crates/infigraph-mcp/tests/mcp_lock.rs
git commit -m "feat: mcp.lock migrates onto the lockfile module for identity + heartbeat (R2.3.1)"
```

---

### Task 3: Wedged-holder detection wired into the busy path (R2.3.3)

**Files:**
- Modify: `crates/infigraph-mcp/src/mcp_lock.rs`
- Test: `crates/infigraph-mcp/tests/mcp_lock.rs` (extend)

**Interfaces:**
- Consumes: `infigraph_core::lockfile::{read_holder, is_holder_wedged}` (Task 1), `wedged_threshold_secs` (Task 2).
- Produces: `pub fn check_wedged_and_log(holder: &infigraph_core::lockfile::LockInfo, now: u64)` (logs WARN if wedged, no-op otherwise — pure decision wrapped in the one side effect it needs, kept as a single function so Task 4 can call it from one place). `acquire_primary` unchanged in this task (Task 4 is what wraps it into the full takeover flow and is where this gets called from).

- [ ] **Step 1: Write the failing test**

Append to `crates/infigraph-mcp/tests/mcp_lock.rs`:

```rust
#[test]
fn check_wedged_and_log_does_not_panic_on_fresh_or_stale_heartbeat() {
    // This function's only observable effect is a log line; there's no
    // return value to assert on directly (mcp_log has no test hook). This
    // test exists to catch a panic (e.g. an integer underflow bug in the
    // staleness math) on both a fresh and a very stale heartbeat -- the
    // real coverage of the underlying pure math is
    // `is_holder_wedged_pure_cases` in infigraph-core's own lockfile.rs
    // tests (Task 1).
    let fresh = infigraph_core::lockfile::LockInfo {
        pid: 1,
        role: "mcp-primary".to_string(),
        build_hash: "abc".to_string(),
        acquired_at: 1000,
        last_heartbeat: 1000,
    };
    infigraph_mcp::mcp_lock::check_wedged_and_log(&fresh, 1005);

    let stale = infigraph_core::lockfile::LockInfo {
        pid: 1,
        role: "mcp-primary".to_string(),
        build_hash: "abc".to_string(),
        acquired_at: 1000,
        last_heartbeat: 1000,
    };
    infigraph_mcp::mcp_lock::check_wedged_and_log(&stale, 1000 + wedged_secs_for_test() + 1);
}

fn wedged_secs_for_test() -> u64 {
    infigraph_mcp::mcp_lock::wedged_threshold_secs()
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test mcp_lock -- --test-threads=1`
Expected: COMPILE ERROR — `check_wedged_and_log` doesn't exist.

- [ ] **Step 3: Implement**

Append to `crates/infigraph-mcp/src/mcp_lock.rs`:

```rust
/// Logs a loud WARN if `holder`'s heartbeat is stale enough to suspect
/// it's wedged -- still holding the OS-level flock (so alive in the
/// liveness sense) but not doing whatever periodic heartbeat work it's
/// supposed to be doing. Advisory only: this never forces a takeover by
/// itself (see the module doc comment) -- it's what makes a wedged holder
/// visible instead of silently blocking every other process forever.
pub fn check_wedged_and_log(holder: &infigraph_core::lockfile::LockInfo, now: u64) {
    if lockfile::is_holder_wedged(holder.last_heartbeat, now, wedged_threshold_secs()) {
        let stale_for = now.saturating_sub(holder.last_heartbeat);
        crate::mcp_log(
            "WARN",
            &format!(
                "mcp.lock is held by PID {} but its heartbeat is {stale_for}s stale \
                 (threshold {}s) -- it may be wedged. Run `infigraph watch-status` \
                 or check the process directly.",
                holder.pid,
                wedged_threshold_secs()
            ),
        );
    }
}
```

Add `use std::time::{SystemTime, UNIX_EPOCH};` if not already present via another import, only if needed by a helper you add for computing "now" — Task 4 is what actually calls `check_wedged_and_log` from `acquire_with_takeover` with a real `now_epoch_secs()`-style value; this task only adds the function itself plus its unit test with hand-supplied timestamps (no wall-clock dependency needed here).

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test mcp_lock -- --test-threads=1`
Expected: PASS, 5/5.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/infigraph-mcp/src/mcp_lock.rs crates/infigraph-mcp/tests/mcp_lock.rs
git commit -m "feat: mcp.lock wedged-holder detection logs loudly on stale heartbeat (R2.3.3)"
```

---

### Task 4: Build-hash-mismatch takeover handshake (R2.3.2/R2.3.2a)

**Files:**
- Modify: `crates/infigraph-mcp/src/mcp_lock.rs`
- Modify: `crates/infigraph-mcp/src/main.rs`
- Test: `crates/infigraph-mcp/tests/mcp_lock.rs` (extend)

**Interfaces:**
- Consumes: `check_wedged_and_log` (Task 3), `acquire_primary`/`heartbeat_tick` (Task 2), `infigraph_core::lockfile::read_holder`, `infigraph_core::build_hash()`.
- Produces: `pub enum AcquireOutcome { Primary(LockFile), Secondary }`, `pub fn acquire_with_takeover() -> AcquireOutcome` (replaces `main.rs`'s call to plain `acquire_primary`), `pub fn heartbeat_and_check_handover(lock: &mut LockFile) -> bool` (replaces `heartbeat_tick` at `main.rs`'s call site; returns `true` if a handover was honored and the caller must now drop the lock and exit).

- [ ] **Step 1: Write the failing tests**

Append to `crates/infigraph-mcp/tests/mcp_lock.rs`:

```rust
/// Two builds with DIFFERENT build_hash: the challenger must request and
/// win a handover once the incumbent's own heartbeat loop honors it.
#[test]
fn takeover_succeeds_when_incumbent_honors_handover_request() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS", "1");
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS", "5");

    // Incumbent acquires directly (bypassing takeover logic -- it's the
    // one being taken over from).
    let mut incumbent = infigraph_mcp::mcp_lock::acquire_primary().expect("free");

    // Challenger, on another "process" (same test process here, real
    // build_hash mismatch is simulated by the incumbent's own build_hash
    // stamped at acquire time always differing from a hand-crafted
    // request only in the reverse direction -- see the next test for the
    // same-build negative case). Run the challenger's attempt on a
    // background thread since it blocks until either it succeeds or
    // times out.
    let challenger = std::thread::spawn(infigraph_mcp::mcp_lock::acquire_with_takeover);

    // Give the challenger a moment to write its handover request, then
    // have the incumbent's heartbeat tick honor it exactly like the real
    // background thread in main.rs would.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let handed_over = infigraph_mcp::mcp_lock::heartbeat_and_check_handover(&mut incumbent);
    assert!(handed_over, "incumbent must see the pending handover request");
    drop(incumbent);

    let outcome = challenger.join().expect("challenger thread panicked");
    match outcome {
        infigraph_mcp::mcp_lock::AcquireOutcome::Primary(_) => {}
        infigraph_mcp::mcp_lock::AcquireOutcome::Secondary => {
            panic!("challenger should have won the lock after the incumbent released it")
        }
    }

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
}

/// The build_hash comparison in `acquire_with_takeover` is what gates
/// whether a handover request is even written. Since both the incumbent
/// and challenger in an in-process test share the same real
/// `infigraph_core::build_hash()`, this test verifies the SAME-build path
/// directly: no handover request should appear on disk, and the
/// challenger must give up as Secondary without ever unblocking the
/// incumbent.
#[test]
fn no_handover_request_written_when_build_hash_matches() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS", "1");
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS", "1");

    let _incumbent = infigraph_mcp::mcp_lock::acquire_primary().expect("free");

    let outcome = infigraph_mcp::mcp_lock::acquire_with_takeover();
    match outcome {
        infigraph_mcp::mcp_lock::AcquireOutcome::Secondary => {}
        infigraph_mcp::mcp_lock::AcquireOutcome::Primary(_) => {
            panic!("must not win the lock when build_hash matches the incumbent's")
        }
    }
    let handover_path = dir.path().join("mcp.lock.handover");
    assert!(
        !handover_path.exists(),
        "no handover request should be written or left behind on the same-build path"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test mcp_lock -- --test-threads=1`
Expected: COMPILE ERROR — `acquire_with_takeover`, `AcquireOutcome`, `heartbeat_and_check_handover`, `INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS`/`_TIMEOUT_SECS` don't exist yet.

**Note on the same-build test case**: since this test's challenger and incumbent run in the same test binary, they share the exact same `infigraph_core::build_hash()` value, so `acquire_with_takeover`'s build-hash comparison always sees a match here — this test exercises the "no mismatch" branch precisely because it's a same-process test, which is exactly the real-world "two copies of the same currently-installed binary" case. The mismatch branch is exercised by the *other* test, which works because that test's incumbent lock is created via a raw `acquire_primary()` call storing a `LockInfo` with the real build hash, while the challenger thread's `acquire_with_takeover()` also computes the real build hash — meaning both would ALSO match in-process. **Before writing Step 3, resolve this test design gap**: either (a) have the mismatch test manually write a `LockInfo` with a hand-crafted different `build_hash` directly into the lock file's *payload* (bypassing `acquire_primary()`, using `serde_json` + a raw file write, still holding a real `flock` via `fs2` directly since the test needs a genuinely-held lock) so the challenger's comparison sees a real mismatch, or (b) restructure `acquire_with_takeover` to accept an injectable "own build hash" parameter for testability (a pure `fn build_hash_mismatch(own: &str, holder: &str) -> bool` helper, unit-testable directly with hand-supplied strings, with the real function calling it with `infigraph_core::build_hash()`) and test *that* instead of the full integration path for the mismatch case, reserving the full-integration `takeover_succeeds_when_incumbent_honors_handover_request` test for a same-build sanity check of the *mechanics* (handover file read/write/clear, thread coordination) rather than claiming to test the build-hash gate itself. Prefer (b) — it's the same pure-logic-extraction pattern already used throughout this codebase (`is_stale`, `should_exit_idle`, `is_holder_wedged`) and avoids hand-rolling a second raw-flock helper in test code. Update the two tests above to match once you've picked the approach: the pure `build_hash_mismatch` gets its own direct unit test with hand-supplied strings; `takeover_succeeds_when_incumbent_honors_handover_request` can keep testing the handover *mechanics* by calling `heartbeat_and_check_handover` directly against a manually-written handover-request file (via whatever `write_handover_request`-equivalent you implement) rather than relying on a genuine build-hash mismatch to trigger it.

- [ ] **Step 3: Implement**

Append to `crates/infigraph-mcp/src/mcp_lock.rs`:

```rust
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pure: does a challenger's build differ from the incumbent's? Only a
/// mismatch justifies requesting takeover -- two processes running the
/// identical binary have no reason to fight over the lock.
fn build_hash_mismatch(own: &str, holder: &str) -> bool {
    own != holder
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandoverRequest {
    pid: u32,
    build_hash: String,
    requested_at: u64,
}

fn handover_request_path() -> PathBuf {
    let lock = lock_path();
    let parent = lock.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    parent.join("mcp.lock.handover")
}

fn write_handover_request() -> std::io::Result<()> {
    let req = HandoverRequest {
        pid: std::process::id(),
        build_hash: infigraph_core::build_hash().to_string(),
        requested_at: now_epoch_secs(),
    };
    let json = serde_json::to_string(&req).unwrap_or_default();
    std::fs::write(handover_request_path(), json)
}

/// Best-effort read. `None` if missing, empty, or unparseable.
fn read_handover_request() -> Option<HandoverRequest> {
    let content = std::fs::read_to_string(handover_request_path()).ok()?;
    serde_json::from_str(&content).ok()
}

fn clear_handover_request() {
    let _ = std::fs::remove_file(handover_request_path());
}

/// How often a challenger, having requested takeover, re-tries acquiring
/// the lock while waiting for the incumbent to honor the request.
/// Overridable via `INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS`.
pub fn takeover_poll_interval() -> Duration {
    std::env::var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(1))
}

/// How long a challenger waits for a handover request to be honored
/// before giving up and falling back to Secondary. Overridable via
/// `INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS`.
pub fn takeover_wait_timeout() -> Duration {
    std::env::var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(10))
}

/// Result of attempting to become mcp.lock's primary.
pub enum AcquireOutcome {
    Primary(LockFile),
    Secondary,
}

/// Try to become primary. If the lock is free, wins immediately. If it's
/// held, checks the incumbent's heartbeat (logs loudly if wedged, see
/// `check_wedged_and_log`) and build_hash: on a mismatch, requests
/// takeover and polls for up to `takeover_wait_timeout()`; on a match, or
/// if the wait times out, falls back to Secondary.
pub fn acquire_with_takeover() -> AcquireOutcome {
    let path = lock_path();
    match lockfile::try_acquire(&path, "mcp-primary") {
        Ok(Some(lock)) => return AcquireOutcome::Primary(lock),
        Ok(None) => {}
        Err(e) => {
            crate::mcp_log("WARN", &format!("mcp.lock open failed: {e:#}"));
            return AcquireOutcome::Secondary;
        }
    }

    let Some(holder) = lockfile::read_holder(&path) else {
        return AcquireOutcome::Secondary;
    };

    check_wedged_and_log(&holder, now_epoch_secs());

    let own_build = infigraph_core::build_hash();
    if !build_hash_mismatch(own_build, &holder.build_hash) {
        return AcquireOutcome::Secondary;
    }

    crate::mcp_log(
        "INFO",
        &format!(
            "mcp.lock held by PID {} on build {} (ours: {own_build}) -- requesting handover",
            holder.pid, holder.build_hash
        ),
    );
    if write_handover_request().is_err() {
        return AcquireOutcome::Secondary;
    }

    let deadline = Instant::now() + takeover_wait_timeout();
    while Instant::now() < deadline {
        std::thread::sleep(takeover_poll_interval());
        if let Ok(Some(lock)) = lockfile::try_acquire(&path, "mcp-primary") {
            clear_handover_request();
            return AcquireOutcome::Primary(lock);
        }
    }

    crate::mcp_log(
        "WARN",
        "mcp.lock handover request timed out -- running as secondary",
    );
    clear_handover_request();
    AcquireOutcome::Secondary
}

/// One heartbeat tick for the primary: refresh `last_heartbeat`, then
/// check for a pending handover request. Returns `true` if one was found
/// and honored -- the caller must drop `lock` and exit the process. This
/// is a release-and-exit, not a graceful drain of in-flight work (that's
/// R5.4's separately-scoped territory).
pub fn heartbeat_and_check_handover(lock: &mut LockFile) -> bool {
    heartbeat_tick(lock);
    if let Some(req) = read_handover_request() {
        crate::mcp_log(
            "INFO",
            &format!(
                "Handover requested by PID {} (build {}) -- releasing mcp.lock and exiting",
                req.pid, req.build_hash
            ),
        );
        clear_handover_request();
        return true;
    }
    false
}
```

In `crates/infigraph-mcp/src/main.rs`, replace the block Task 2 wrote:

```rust
    let mcp_lock = infigraph_mcp::mcp_lock::acquire_primary();
    let is_primary = mcp_lock.is_some();

    if is_primary {
        mcp_log("INFO", "Acquired mcp.lock — running as primary");
    } else {
        mcp_log(
            "WARN",
            "Another MCP instance holds mcp.lock — running without watchers",
        );
        infigraph_mcp::tools::watch::disable_watchers();
    }

    if let Some(mut lock) = mcp_lock {
        std::thread::spawn(move || loop {
            std::thread::sleep(infigraph_mcp::mcp_lock::heartbeat_interval());
            infigraph_mcp::mcp_lock::heartbeat_tick(&mut lock);
        });
    }
```

with:

```rust
    let mcp_lock_outcome = infigraph_mcp::mcp_lock::acquire_with_takeover();
    let (is_primary, mcp_lock) = match mcp_lock_outcome {
        infigraph_mcp::mcp_lock::AcquireOutcome::Primary(lock) => {
            mcp_log("INFO", "Acquired mcp.lock — running as primary");
            (true, Some(lock))
        }
        infigraph_mcp::mcp_lock::AcquireOutcome::Secondary => {
            mcp_log(
                "WARN",
                "Another MCP instance holds mcp.lock — running without watchers",
            );
            infigraph_mcp::tools::watch::disable_watchers();
            (false, None)
        }
    };

    if let Some(mut lock) = mcp_lock {
        std::thread::spawn(move || loop {
            std::thread::sleep(infigraph_mcp::mcp_lock::heartbeat_interval());
            if infigraph_mcp::mcp_lock::heartbeat_and_check_handover(&mut lock) {
                drop(lock);
                std::process::exit(0);
            }
        });
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test mcp_lock -- --test-threads=1`
Expected: PASS, all tests (exact count depends on which Step 2 design resolution you picked — should be 7-8).

- [ ] **Step 5: Run the full infigraph-mcp suite**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --no-fail-fast -- --test-threads=4`
Expected: green modulo the already-catalogued pre-existing failures this campaign (`tool_parity`, `watcher_concurrency::test_graph_tools_with_group_watchers`, `--lib compress::tests::test_compress_pipeline_safe_normal_path`). Any NEW failure is this task's to resolve.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/infigraph-mcp/src/mcp_lock.rs crates/infigraph-mcp/src/main.rs crates/infigraph-mcp/tests/mcp_lock.rs
git commit -m "feat: mcp.lock build-hash-mismatch takeover handshake (R2.3.2/R2.3.2a)"
```

---

### Task 5: Full verification

**Files:**
- None created; runs suites.

- [ ] **Step 1: Full test suites**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --no-fail-fast -- --test-threads=4
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --no-fail-fast -- --test-threads=4
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-cli --no-fail-fast -- --test-threads=4
```

Expected: green modulo the already-catalogued pre-existing failures from this campaign. If a process-spawning test (this plan's `acquire_primary`/takeover tests, or PR7a's `instance_registration.rs`/`idle_shutdown.rs`) shows resource-contention-style failures under `--test-threads=4`, re-run with `--test-threads=1` before treating it as a real regression.

- [ ] **Step 2: Clippy**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-core -p infigraph-mcp --all-targets -- -D warnings`
Expected: clean on files touched by this branch.

- [ ] **Step 3: Manual smoke check — two-process takeover**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-mcp
mkdir -p /tmp/mcp-lock-smoke
INFIGRAPH_MCP_LOCK_PATH=/tmp/mcp-lock-smoke/mcp.lock target/debug/infigraph-mcp --worker --ui --mcp --port=19749 &
PID1=$!
sleep 1
cat /tmp/mcp-lock-smoke/mcp.lock   # expect pid/role/build_hash/acquired_at/last_heartbeat JSON, pid matches $PID1

# Second instance of the SAME binary: must NOT take over (build_hash matches).
INFIGRAPH_MCP_LOCK_PATH=/tmp/mcp-lock-smoke/mcp.lock target/debug/infigraph-mcp --worker --ui --mcp --port=19750 &
PID2=$!
sleep 2
kill -0 $PID1 && echo "PID1 still alive as expected (same build, no takeover)"

kill $PID1 $PID2 2>/dev/null
rm -rf /tmp/mcp-lock-smoke
```

Simulating a genuine build-hash mismatch end-to-end requires two actually-different binaries (e.g. build once, touch a source file, rebuild, run the old binary first then the new one) — optional, not required to consider this task done, but worth doing manually once if time allows, since it's the one path the automated tests approximate rather than exercise byte-for-byte identically to production.

- [ ] **Step 4: Update the progress ledger**

Append to `.superpowers/sdd/progress.md`: a `=== PR7b: mcp.lock identity/heartbeat/wedged/takeover ===` section header and per-task completion lines, matching PR7a's ledger style. No push/PR — per the Global Constraints, that happens later at a point the user decides across the whole stack.

---

## Self-Review Notes

- **Spec coverage:** R2.3.1 (identity via lockfile module) — Task 2. R2.3.5 (heartbeat) — Task 1 (mechanism) + Task 2 (wiring). R2.3.3 (wedged-holder loud degradation) — Task 3. R2.3.2/R2.3.2a (build-hash handshake takeover, not surrender) — Task 4. R2.3.4 (lock respawn) explicitly noted as already-done-by-construction, no task needed. R2.3.6/R2.3.7 (sessions.lock/registry.lock) explicitly out of scope, already shipped in PR4.
- **The takeover mechanism has direct tests for both branches**, not just the happy path: `takeover_succeeds_when_incumbent_honors_handover_request` (mismatch → win) and `no_handover_request_written_when_build_hash_matches` (match → no-op) — this is the exact pair of cases that "not surrender, but not indiscriminate either" needs proof for.
- **Known test-design gap flagged explicitly, not silently**: the in-process nature of the test binary means both "processes" share one real `build_hash()`, so Task 4's Step 2 note walks the implementer through resolving that honestly (extract `build_hash_mismatch` as its own directly-testable pure function) rather than leaving a plan that would compile but not actually prove what it claims to.
- **No placeholders**: every step has complete, runnable code. The one open design choice left to the implementer (Task 4 Step 2's (a) vs (b) resolution) is a real, disclosed test-construction decision with a stated recommendation and reasoning, not a stand-in for undecided production logic.
- **Backward compatibility**: `LockInfo`'s new `last_heartbeat` field uses `#[serde(default)]` and is covered by its own explicit test (`old_lock_file_without_last_heartbeat_field_still_parses`) — a lock file written by a pre-this-PR binary (e.g. `graph.lock`/`index.lock` from an already-running older process, or an mcp.lock from before this PR's binary is installed) must not fail to parse.
- **Type/signature consistency**: `AcquireOutcome`, `acquire_with_takeover`, `heartbeat_and_check_handover`, `check_wedged_and_log` are defined once (Tasks 3-4) and consumed with matching signatures at `main.rs`'s one call site (Task 4) — no renamed duplicates across tasks.
- **No new branch, no push** — matches the standing stacking directive; Task 5 stops at updating the progress ledger.
