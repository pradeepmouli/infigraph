# PR7a: MCP Instance Registry + Orphan Reaping (R2.2.1, R2.2.2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every `infigraph-mcp` process registers itself at `~/.infigraph/instances/<pid>.json` on startup and removes it on clean shutdown (R2.2.1); on startup and every 10 minutes thereafter, each process scans that registry and reaps entries that are dead or belong to a reused PID (R2.2.2), closing the gap that self-termination alone (already-shipped R2.2.3) can't: an orphan whose own idle-check loop never runs (predates the fix, or is otherwise stuck) is now cleaned up *from the outside* by the next process that starts or by the periodic scan.

**Architecture:** A new `crates/infigraph-core/src/instances.rs` module owns the registry: the `InstanceInfo` record, file I/O (register/list), and orphan classification. Classification hinges on a **PID-reuse guard** — a registry entry records the OS-reported start-time of the process it names; reaping recomputes that PID's *current* start-time and treats any mismatch (including "no such process") as stale, so a brand-new unrelated process that happens to reuse a dead PID is never mistaken for the process the entry originally named. The classification and staleness-decision functions are pure (take an injectable start-time lookup), unit-tested without touching real processes; reaping itself (SIGTERM → grace → SIGKILL via `sysinfo`) and the register/deregister lifecycle are covered by an integration test using real files and the test's own process. Lives in `infigraph-core` (not `infigraph-mcp`) so the future R2.2.4 `infigraph ps`/`kill` CLI can read the same registry.

**Tech Stack:** Rust (edition 2021), new dependency: `sysinfo` (PID-liveness + start-time lookup + cross-platform SIGTERM/SIGKILL via `Process::kill_with`/`Process::kill`).

## Global Constraints

- **Scope is R2.2.1 + R2.2.2 only.** This is PR7a of the user-approved 7a/7b split (`docs/superpowers/specs/2026-07-21-remaining-hardening-design.md` §3, PR7 row). It does **not** touch `mcp.lock` identity/takeover/wedged-detection/heartbeat (R2.3.1/2.3.2/2.3.2a/2.3.3/2.3.5) — that's PR7b, planned separately, reusing this PR's PID-liveness building blocks. Do not attempt R2.3.x here.
- **Does not duplicate R2.2.3.** Idle self-termination (a process noticing *its own* stdin closed and exiting) already shipped this session (`docs/superpowers/plans/2026-07-21-mcp-idle-self-termination.md`, `crates/infigraph-mcp/src/idle.rs`). This plan's job is reaping orphans *from the outside* — dead-but-undetected-by-self entries — not touching that file.
- New dependency `sysinfo` goes in `crates/infigraph-core/Cargo.toml`'s `[dependencies]` directly (matching the pattern used for `rayon`, `sha2`, `notify`, etc. in that file — not hoisted to `[workspace.dependencies]`, which this crate reserves for deps shared with `infigraph-languages`).
- **Verify the `sysinfo` API surface before trusting the code snippets below verbatim.** `sysinfo`'s `refresh_processes`/`ProcessesToUpdate`/`Process::kill_with`/`Signal` API has changed shape across versions. Run `cargo add sysinfo -p infigraph-core` first (picks up whatever's current), then check `cargo doc -p sysinfo --no-deps --open` (or docs.rs) for the exact signatures before writing Task 1 Step 3 — adjust call shapes if they differ, keeping the semantics identical: refresh exactly one PID's data, don't drop dead entries from a shared `System` between refreshes, `kill_with(Signal::Term)` returns `Option<bool>` where `None` means the signal isn't supported on this platform.
- **No new branch.** Per the standing directive covering every PR since PR4 (PR4, PR6, PR9, mcp-idle-self-termination all stacked this way — see `.superpowers/sdd/progress.md`), commit PR7a's tasks directly onto the current branch, `feat/health-beacons`. Do not create or check out any other branch.
- **No push during this plan.** Matching PR4/PR6/PR9's pattern ("No push/PR per branch-stacking directive"), commits land locally only; pushing/PR-opening happens later, at a point the user decides across the whole stack, not per-PR.
- Commit with `--no-verify` only if the pre-commit hook fails *after* you've already run `cargo fmt` manually and the failure is unrelated to your change (repo pre-commit hook runs fmt-check + clippy across the whole workspace, not just changed files — pre-existing drift elsewhere can block a clean change). Otherwise let the hook run normally.
- Every cargo command runs with `CARGO_PROFILE_DEV_DEBUG=0` (hard rule for this repo — mixing debug settings spawns multi-GB duplicate lbug cmake trees / ENOSPC; this machine has hit that incident before and disk is currently tight, ~14GB free at last check).
- Fork-only, no upstream PR without asking (standing directive — user curates upstream submissions). Not directly relevant to this plan since no PR is opened here, but binds any later push/PR step.
- Env vars introduced here follow the established `INFIGRAPH_MCP_IDLE_GRACE_SECS`/`INFIGRAPH_SLOW_LOCK_MS` naming pattern (prefix `INFIGRAPH_`, `_SECS`/`_MS` suffix by unit), not invented fresh.

---

### Task 1: `InstanceInfo` record + registry file I/O + pure staleness logic

**Files:**
- Create: `crates/infigraph-core/src/instances.rs`
- Modify: `crates/infigraph-core/src/lib.rs` (add `pub mod instances;` alphabetically — after `pub mod graph;`, before `pub mod lang;`, matching the file's existing alphabetical module list)
- Modify: `crates/infigraph-core/Cargo.toml` (add `sysinfo` to `[dependencies]`)
- Test: `crates/infigraph-core/tests/instance_registry.rs`

**Interfaces:**
- Produces: `pub struct InstanceInfo { pid: u32, started_at: u64, project_path: String, transport: String, host_agent_hint: Option<String> }` (all fields `pub`, `Serialize`/`Deserialize`/`Clone`/`Debug`/`PartialEq`), `InstanceInfo::current(project_path: &str, transport: &str) -> InstanceInfo`, `pub fn instances_dir() -> PathBuf`, `pub fn current_process_start_time(pid: u32) -> Option<u64>`, `pub struct InstanceGuard` (RAII, removes its file on `Drop`), `pub fn register_instance(info: &InstanceInfo) -> anyhow::Result<InstanceGuard>`, `pub fn list_instances() -> Vec<(PathBuf, InstanceInfo)>`, `pub fn is_stale(recorded_start: u64, actual_start: Option<u64>) -> bool`. Task 2 consumes `InstanceInfo::current`/`register_instance` from `main.rs`. Task 3 consumes `is_stale`/`list_instances`/`current_process_start_time`.

- [ ] **Step 1: Add the dependency and verify its API**

```bash
cd /Users/pmouli/GitHub.nosync/active/rust/infigraph
cargo add sysinfo -p infigraph-core
```

Then check the installed version's docs for `System::refresh_processes`, `ProcessesToUpdate`, `Process::start_time`, `Process::kill_with`, `Signal::Term` before Step 3 — adjust the exact call shapes there if they've drifted from what's shown, keeping the semantics described in the Global Constraints.

- [ ] **Step 2: Write the failing tests**

Create `crates/infigraph-core/tests/instance_registry.rs`:

```rust
use infigraph_core::instances::{
    current_process_start_time, instances_dir, is_stale, list_instances, register_instance,
    InstanceInfo,
};

/// Serializes tests that mutate the process-global INFIGRAPH_INSTANCES_DIR
/// env var — cargo runs this binary's tests on parallel threads, so one
/// test's override must not leak into another's window (same lesson as the
/// IDLE_ENV mutex in the R2.2.3 idle-self-termination test suite).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn is_stale_pure_cases() {
    // Same process, same recorded start time: live.
    assert!(!is_stale(1000, Some(1000)));
    // No such process anymore: dead.
    assert!(is_stale(1000, None));
    // A process exists at that PID, but its start time doesn't match what
    // was recorded: the original process is gone, PID was reused.
    assert!(is_stale(1000, Some(2000)));
}

#[test]
fn current_process_start_time_finds_self() {
    let pid = std::process::id();
    let first = current_process_start_time(pid);
    assert!(first.is_some(), "expected to find our own running process");
    let second = current_process_start_time(pid);
    assert_eq!(first, second, "a process's own start time must not change between two lookups");
}

#[test]
fn register_and_list_round_trip() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_INSTANCES_DIR", dir.path());

    assert_eq!(instances_dir(), dir.path());

    let info = InstanceInfo::current("/tmp/some-project", "stdio");
    let guard = register_instance(&info).expect("register_instance");

    let listed = list_instances();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].1, info);

    drop(guard);
    assert!(
        list_instances().is_empty(),
        "dropping the guard must remove the instance file (clean-shutdown path)"
    );

    std::env::remove_var("INFIGRAPH_INSTANCES_DIR");
}

#[test]
fn list_instances_skips_unparseable_entries() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_INSTANCES_DIR", dir.path());

    std::fs::write(dir.path().join("99999999.json"), b"not valid json").unwrap();
    let info = InstanceInfo::current("/tmp/some-project", "stdio");
    let _guard = register_instance(&info).expect("register_instance");

    let listed = list_instances();
    assert_eq!(listed.len(), 1, "the unparseable file must be skipped, not error the whole scan");
    assert_eq!(listed[0].1, info);

    std::env::remove_var("INFIGRAPH_INSTANCES_DIR");
}
```

Add `tempfile` as a dev-dependency check: `crates/infigraph-core/Cargo.toml` already lists `tempfile = "3"` under `[dependencies]` (not dev-only) — confirm it's usable from `tests/` (it is; regular dependencies are available to integration tests too), no `Cargo.toml` change needed for this.

- [ ] **Step 3: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test instance_registry`
Expected: COMPILE ERROR — `infigraph_core::instances` module does not exist.

- [ ] **Step 4: Implement**

Create `crates/infigraph-core/src/instances.rs`:

```rust
//! Instance registry (R2.2.1) — every `infigraph-mcp` process registers
//! itself at `~/.infigraph/instances/<pid>.json` on startup and removes it
//! on clean shutdown (RAII). Orphan detection (R2.2.2) reads this registry
//! to tell a live peer from a dead-or-reused-PID orphan, via a PID-reuse
//! guard: a registry entry records the process's OS-reported start time,
//! and a fresh lookup at classification time must match it exactly — a
//! bare PID match is not proof it's the same process the entry named.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstanceInfo {
    pub pid: u32,
    /// Unix epoch seconds this OS process itself started, per `sysinfo` —
    /// not when this registry file was written. The PID-reuse guard
    /// compares this against a fresh lookup of the same PID.
    pub started_at: u64,
    pub project_path: String,
    pub transport: String,
    pub host_agent_hint: Option<String>,
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Directory holding one JSON file per live-or-recently-live instance.
/// Overridable via `INFIGRAPH_INSTANCES_DIR` (tests).
pub fn instances_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("INFIGRAPH_INSTANCES_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".infigraph")
        .join("instances")
}

fn instance_path(pid: u32) -> PathBuf {
    instances_dir().join(format!("{pid}.json"))
}

/// Best-effort hint at which coding agent launched this process, from a
/// small set of well-known env vars. `None` if none are set — this is a
/// diagnostic aid, never load-bearing for orphan classification.
fn host_agent_hint() -> Option<String> {
    for (var, label) in [
        ("CLAUDECODE", "claude-code"),
        ("CURSOR_TRACE_ID", "cursor"),
        ("TERM_PROGRAM", "term"),
    ] {
        if std::env::var(var).is_ok() {
            return Some(label.to_string());
        }
    }
    None
}

/// Fresh lookup of a PID's OS-reported start time (Unix epoch seconds).
/// `None` if no such process exists right now.
pub fn current_process_start_time(pid: u32) -> Option<u64> {
    let spid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[spid]), true);
    sys.process(spid).map(|p| p.start_time())
}

impl InstanceInfo {
    /// Builds this process's own instance record, using a fresh lookup of
    /// our own PID so `started_at` matches exactly what a later PID-reuse
    /// check will compare against.
    pub fn current(project_path: &str, transport: &str) -> Self {
        let pid = std::process::id();
        let started_at = current_process_start_time(pid).unwrap_or_else(now_epoch_secs);
        Self {
            pid,
            started_at,
            project_path: project_path.to_string(),
            transport: transport.to_string(),
            host_agent_hint: host_agent_hint(),
        }
    }
}

/// RAII guard: removes this process's instance file on drop (the
/// clean-shutdown path). A crash or `kill -9` leaves the file behind —
/// that's exactly what orphan reaping exists to clean up from the outside.
#[derive(Debug)]
pub struct InstanceGuard {
    path: PathBuf,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Writes this process's instance file and returns a guard that removes it
/// on drop.
pub fn register_instance(info: &InstanceInfo) -> Result<InstanceGuard> {
    let dir = instances_dir();
    std::fs::create_dir_all(&dir)?;
    let path = instance_path(info.pid);
    let json = serde_json::to_string_pretty(info)?;
    let mut file = std::fs::File::create(&path)?;
    file.write_all(json.as_bytes())?;
    Ok(InstanceGuard { path })
}

/// Scans the instance registry directory. Skips entries that don't parse
/// (partial write mid-crash, or a schema from a different binary version)
/// rather than failing the whole scan.
pub fn list_instances() -> Vec<(PathBuf, InstanceInfo)> {
    let dir = instances_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|e| {
            let path = e.path();
            let content = std::fs::read_to_string(&path).ok()?;
            let info: InstanceInfo = serde_json::from_str(&content).ok()?;
            Some((path, info))
        })
        .collect()
}

/// Pure: is a registry entry stale (should be reaped)? `actual_start` is a
/// fresh lookup for the entry's `pid` — `None` means no such process
/// exists right now; `Some(t)` where `t != recorded_start` means the PID
/// was reused by an unrelated process; either case means the process the
/// entry originally named is gone.
pub fn is_stale(recorded_start: u64, actual_start: Option<u64>) -> bool {
    actual_start != Some(recorded_start)
}

#[allow(unused)]
fn unused_path_import_guard(_p: &Path) {}
```

Remove the trailing `unused_path_import_guard` stub once Task 3 adds real uses of `Path` in this same file (it exists only so this task's file compiles cleanly with the `use std::path::{Path, PathBuf};` import already in place for Task 3 to extend) — actually, simpler: change the import line in this step to `use std::path::PathBuf;` only (drop `Path`), since nothing in Task 1 needs `Path` by itself. Task 3 will add `Path` back when it needs `&Path` parameters. Do not add the stub function — use the corrected single-item import instead.

Add `pub mod instances;` to `crates/infigraph-core/src/lib.rs`, alphabetically after `pub mod graph;` and before `pub mod lang;`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test instance_registry`
Expected: PASS, 4/4.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/infigraph-core/src/instances.rs crates/infigraph-core/src/lib.rs crates/infigraph-core/Cargo.toml crates/infigraph-core/tests/instance_registry.rs Cargo.lock
git commit -m "feat: instance registry record, file I/O, PID-reuse staleness check (R2.2.1)"
```

---

### Task 2: Wire registration into `infigraph-mcp`'s `main.rs::run()`

**Files:**
- Modify: `crates/infigraph-mcp/src/main.rs`
- Test: `crates/infigraph-mcp/tests/instance_registration.rs`

**Interfaces:**
- Consumes: `infigraph_core::instances::{InstanceInfo, register_instance}` from Task 1.
- Produces: nothing new consumed by later tasks in this crate (Task 3/4 add orphan reaping as a separate, additive block in the same function).

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-mcp/tests/instance_registration.rs`:

```rust
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Spawns the real infigraph-mcp binary and asserts it writes its own
/// instance file under INFIGRAPH_INSTANCES_DIR while running, and removes
/// it again after a clean stdin-close shutdown (the same idle-grace path
/// R2.2.3 already exercises, with a short grace so this test stays fast).
#[test]
fn worker_registers_and_deregisters_instance_file() {
    let exe = env!("CARGO_BIN_EXE_infigraph-mcp");
    let dir = tempfile::tempdir().expect("tempdir");

    let mut child = Command::new(exe)
        .args(["--worker", "--ui", "--mcp", "--port=0"])
        .env("INFIGRAPH_INSTANCES_DIR", dir.path())
        .env("INFIGRAPH_MCP_IDLE_GRACE_SECS", "2")
        .env("INFIGRAPH_MCP_IDLE_POLL_SECS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infigraph-mcp");

    // Give it a moment to reach the registration point.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count = std::fs::read_dir(dir.path())
            .map(|d| d.flatten().count())
            .unwrap_or(0);
        if count == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "instance file was never written");
        std::thread::sleep(Duration::from_millis(100));
    }

    drop(child.stdin.take());

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(Instant::now() < deadline, "process did not self-terminate");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(status.success());

    let remaining = std::fs::read_dir(dir.path())
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert_eq!(remaining, 0, "instance file must be removed on clean shutdown");
}
```

Add `tempfile` to `crates/infigraph-mcp/Cargo.toml`'s `[dev-dependencies]` if it isn't already present — check the file first; if `tempfile` already appears under `[dependencies]` or `[dev-dependencies]`, no change needed.

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test instance_registration -- --test-threads=1`
Expected: FAILS (times out waiting for the instance file — registration doesn't exist yet).

- [ ] **Step 3: Implement**

In `crates/infigraph-mcp/src/main.rs`, in `run()`, immediately after the existing instance-lock block:

```rust
fn run() -> Result<()> {
    let instance_lock = acquire_instance_lock();
    let is_primary = instance_lock.is_some();
    let _lock_guard = instance_lock;

    if !is_primary {
        infigraph_mcp::tools::watch::disable_watchers();
    }
```

add, right after that block (before the `let args: Vec<String> = ...` line that already follows):

```rust
    let args: Vec<String> = std::env::args().collect();
    let mcp_mode_for_registry = args.iter().any(|a| a == "--mcp");
    let transport = if mcp_mode_for_registry { "stdio" } else { "http" };
    let project_path = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let instance_info = infigraph_core::instances::InstanceInfo::current(&project_path, transport);
    let _instance_guard = match infigraph_core::instances::register_instance(&instance_info) {
        Ok(guard) => Some(guard),
        Err(e) => {
            mcp_log("WARN", &format!("Failed to register instance: {e:#}"));
            None
        }
    };
```

Note this duplicates the `let args: Vec<String> = args.collect();` that already exists a few lines below in the unmodified function — remove the *original* `let args: Vec<String> = std::env::args().collect();` line further down (the one immediately before `let ui_enabled = ...`) since this new block now declares `args` earlier in the same scope; keep every other line below it (`ui_enabled`, `port`, `mcp_mode`, etc.) exactly as-is, just reusing this earlier `args` binding instead of re-collecting it. `mcp_mode` (declared further down as `let mcp_mode = args.iter().any(|a| a == "--mcp");`) can then simply reuse `mcp_mode_for_registry` — rename `mcp_mode_for_registry` to `mcp_mode` directly at the point it's introduced above, and delete the later duplicate `let mcp_mode = ...` line entirely, so there's exactly one binding.

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test instance_registration -- --test-threads=1`
Expected: PASS, 1/1.

- [ ] **Step 5: Run the existing idle-shutdown suite to confirm no regression**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test idle_shutdown -- --test-threads=1`
Expected: PASS, 2/2 (unchanged — this task only adds a block, doesn't alter the idle-grace logic).

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/infigraph-mcp/src/main.rs crates/infigraph-mcp/tests/instance_registration.rs crates/infigraph-mcp/Cargo.toml Cargo.lock
git commit -m "feat: infigraph-mcp registers/deregisters itself in the instance registry (R2.2.1)"
```

---

### Task 3: Orphan classification + reaping logic (pure classify, real reap)

**Files:**
- Modify: `crates/infigraph-core/src/instances.rs`
- Test: `crates/infigraph-core/tests/instance_registry.rs` (extend from Task 1)

**Interfaces:**
- Consumes: `is_stale`, `list_instances`, `current_process_start_time`, `InstanceInfo` from Task 1.
- Produces: `#[derive(Debug, PartialEq)] pub enum InstanceStatus { LivePeer, Orphan }`, `pub fn classify_instances(entries: &[(PathBuf, InstanceInfo)], own_pid: u32, lookup_start_time: impl Fn(u32) -> Option<u64>) -> Vec<(PathBuf, InstanceInfo, InstanceStatus)>`, `pub fn reap_grace_period() -> Duration`, `pub fn reap_orphan(path: &Path, pid: u32)`, `pub fn reap_orphans_once(own_pid: u32) -> usize`. Task 4 consumes `reap_orphans_once`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/infigraph-core/tests/instance_registry.rs`:

```rust
use infigraph_core::instances::{classify_instances, InstanceInfo, InstanceStatus};
use std::collections::HashMap;
use std::path::PathBuf;

fn fake_entry(pid: u32, started_at: u64) -> (PathBuf, InstanceInfo) {
    (
        PathBuf::from(format!("/fake/{pid}.json")),
        InstanceInfo {
            pid,
            started_at,
            project_path: "/fake/project".to_string(),
            transport: "stdio".to_string(),
            host_agent_hint: None,
        },
    )
}

#[test]
fn classify_instances_distinguishes_live_dead_and_reused() {
    let entries = vec![
        fake_entry(100, 1000), // live: lookup returns matching start time
        fake_entry(200, 1000), // dead: lookup returns None
        fake_entry(300, 1000), // reused: lookup returns a different start time
        fake_entry(999, 1000), // own_pid: must be skipped entirely
    ];
    let mut actual_starts: HashMap<u32, Option<u64>> = HashMap::new();
    actual_starts.insert(100, Some(1000));
    actual_starts.insert(200, None);
    actual_starts.insert(300, Some(9999));
    actual_starts.insert(999, Some(1000));

    let classified = classify_instances(&entries, 999, |pid| {
        actual_starts.get(&pid).copied().flatten()
    });

    assert_eq!(classified.len(), 3, "own_pid entry must be excluded");
    let status_for = |pid: u32| {
        classified
            .iter()
            .find(|(_, info, _)| info.pid == pid)
            .map(|(_, _, status)| status)
    };
    assert_eq!(status_for(100), Some(&InstanceStatus::LivePeer));
    assert_eq!(status_for(200), Some(&InstanceStatus::Orphan));
    assert_eq!(status_for(300), Some(&InstanceStatus::Orphan));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test instance_registry`
Expected: COMPILE ERROR — `classify_instances`/`InstanceStatus` don't exist yet.

- [ ] **Step 3: Implement**

In `crates/infigraph-core/src/instances.rs`, change the import line to restore `Path`:

```rust
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
```

Append to the file:

```rust
/// One registry entry's classification against the current process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceStatus {
    LivePeer,
    Orphan,
}

/// Pure: classifies every entry except `own_pid` (a process never reaps
/// itself — self-termination is R2.2.3's job) via an injectable start-time
/// lookup, so this is fully testable without touching real processes.
pub fn classify_instances(
    entries: &[(PathBuf, InstanceInfo)],
    own_pid: u32,
    lookup_start_time: impl Fn(u32) -> Option<u64>,
) -> Vec<(PathBuf, InstanceInfo, InstanceStatus)> {
    entries
        .iter()
        .filter(|(_, info)| info.pid != own_pid)
        .map(|(path, info)| {
            let actual = lookup_start_time(info.pid);
            let status = if is_stale(info.started_at, actual) {
                InstanceStatus::Orphan
            } else {
                InstanceStatus::LivePeer
            };
            (path.clone(), info.clone(), status)
        })
        .collect()
}

/// Grace period between SIGTERM and SIGKILL when reaping an orphan.
/// Overridable via `INFIGRAPH_REAP_GRACE_SECS` (seconds) — kept small in
/// tests.
pub fn reap_grace_period() -> Duration {
    std::env::var("INFIGRAPH_REAP_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(5))
}

/// How often the periodic orphan scan runs. Overridable via
/// `INFIGRAPH_REAP_SCAN_SECS` (seconds).
pub fn reap_scan_interval() -> Duration {
    std::env::var("INFIGRAPH_REAP_SCAN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(600))
}

/// SIGTERM the orphan's PID, wait `reap_grace_period()`, SIGKILL if still
/// alive, then remove its registry file regardless (a removed file for an
/// already-dead PID is correct cleanup either way). Best-effort: a process
/// that exits on its own between the classify scan and this call is not an
/// error.
pub fn reap_orphan(path: &Path, pid: u32) {
    let spid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[spid]), true);
    if let Some(process) = sys.process(spid) {
        if process.kill_with(sysinfo::Signal::Term).is_none() {
            // SIGTERM not supported on this platform — skip straight to
            // an unconditional kill rather than waiting out a grace period
            // for a signal that was never actually sent.
            process.kill();
        } else {
            std::thread::sleep(reap_grace_period());
            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[spid]), true);
            if let Some(still_alive) = sys.process(spid) {
                still_alive.kill();
            }
        }
    }
    let _ = std::fs::remove_file(path);
}

/// Runs one full scan-and-reap pass: lists the registry, classifies every
/// other entry, reaps every orphan found. Returns the count reaped, for
/// the caller to log.
pub fn reap_orphans_once(own_pid: u32) -> usize {
    let entries = list_instances();
    let classified = classify_instances(&entries, own_pid, current_process_start_time);
    let mut reaped = 0;
    for (path, info, status) in classified {
        if status == InstanceStatus::Orphan {
            reap_orphan(&path, info.pid);
            reaped += 1;
        }
    }
    reaped
}
```

Delete the `unused_path_import_guard` stub from Task 1 if it was added (per Task 1 Step 4's note it should not have been — confirm it's absent; if present, remove it now since `Path` is genuinely used above).

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test instance_registry`
Expected: PASS, 5/5.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/infigraph-core/src/instances.rs crates/infigraph-core/tests/instance_registry.rs
git commit -m "feat: orphan classification (PID-reuse guard) + SIGTERM/SIGKILL reaping (R2.2.2)"
```

---

### Task 4: Wire startup scan + periodic reaping into `infigraph-mcp`

**Files:**
- Modify: `crates/infigraph-mcp/src/main.rs`
- Test: `crates/infigraph-mcp/tests/instance_registration.rs` (extend from Task 2)

**Interfaces:**
- Consumes: `infigraph_core::instances::{reap_orphans_once, reap_scan_interval}` from Task 3.

- [ ] **Step 1: Write the failing test**

Append to `crates/infigraph-mcp/tests/instance_registration.rs`:

```rust
/// A second worker started while a stale (dead-PID) instance file already
/// exists in the shared registry dir must reap it on startup — proving the
/// scan-and-reap wiring runs, not just the pure logic it's built from.
#[test]
fn worker_reaps_stale_instance_file_on_startup() {
    let exe = env!("CARGO_BIN_EXE_infigraph-mcp");
    let dir = tempfile::tempdir().expect("tempdir");

    // A PID essentially guaranteed not to be a running process, with an
    // arbitrary recorded start time — current_process_start_time(999999)
    // will return None, so is_stale is unconditionally true regardless of
    // which PID the OS actually assigns.
    std::fs::write(
        dir.path().join("999999.json"),
        r#"{"pid":999999,"started_at":1,"project_path":"/dead","transport":"stdio","host_agent_hint":null}"#,
    )
    .unwrap();

    let mut child = Command::new(exe)
        .args(["--worker", "--ui", "--mcp", "--port=0"])
        .env("INFIGRAPH_INSTANCES_DIR", dir.path())
        .env("INFIGRAPH_MCP_IDLE_GRACE_SECS", "2")
        .env("INFIGRAPH_MCP_IDLE_POLL_SECS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infigraph-mcp");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let has_dead_entry = dir.path().join("999999.json").exists();
        if !has_dead_entry {
            break;
        }
        assert!(Instant::now() < deadline, "stale instance file was never reaped");
        std::thread::sleep(Duration::from_millis(100));
    }

    drop(child.stdin.take());
    let _ = child.wait();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test instance_registration -- --test-threads=1`
Expected: `worker_reaps_stale_instance_file_on_startup` FAILS (times out — nothing reaps yet). The two existing tests from Task 2 still PASS.

- [ ] **Step 3: Implement**

In `crates/infigraph-mcp/src/main.rs`'s `run()`, immediately after the `_instance_guard` block added in Task 2, add the startup scan and a periodic background thread:

```rust
    let reaped = infigraph_core::instances::reap_orphans_once(std::process::id());
    if reaped > 0 {
        mcp_log("INFO", &format!("Reaped {reaped} orphaned instance(s) on startup"));
    }

    std::thread::spawn(|| loop {
        std::thread::sleep(infigraph_core::instances::reap_scan_interval());
        let reaped = infigraph_core::instances::reap_orphans_once(std::process::id());
        if reaped > 0 {
            mcp_log("INFO", &format!("Reaped {reaped} orphaned instance(s) (periodic scan)"));
        }
    });
```

This thread is intentionally not joined or tracked — it's a fire-and-forget background scan for the lifetime of the process, the same pattern the codebase already uses for its watcher threads.

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test instance_registration -- --test-threads=1`
Expected: PASS, 2/2.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/infigraph-mcp/src/main.rs crates/infigraph-mcp/tests/instance_registration.rs
git commit -m "feat: startup + periodic orphan reaping wired into infigraph-mcp (R2.2.2)"
```

---

### Task 5: Full verification + push to fork

**Files:**
- None created; runs suites, pushes.

- [ ] **Step 1: Full test suites**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core -- --test-threads=4 --no-fail-fast
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp -- --test-threads=4 --no-fail-fast
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-cli -- --test-threads=4 --no-fail-fast
```

Expected: green, modulo the already-catalogued pre-existing failures from this campaign (`infigraph-mcp`: `tool_parity::advertised_tools_match_mcp_tool_names`, `watcher_concurrency::test_graph_tools_with_group_watchers`, `--lib compress::tests::test_compress_pipeline_safe_normal_path`; `infigraph-core`: `f16_quality::compare_f16_vs_int8_quality`). Any NEW failure is this branch's to resolve before proceeding. If any of the process-spawning tests in `instance_registration.rs`/`idle_shutdown.rs` show resource-contention-style failures under `--test-threads=4`, re-run with `--test-threads=1` before treating it as a real regression (established false-positive pattern on this machine, per this repo's own `CLAUDE.md`).

- [ ] **Step 2: Clippy**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-core -p infigraph-mcp -- -D warnings`
Expected: clean on files touched by this branch.

- [ ] **Step 3: Manual smoke check**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-mcp
mkdir -p /tmp/instance-smoke
INFIGRAPH_INSTANCES_DIR=/tmp/instance-smoke target/debug/infigraph-mcp --worker --ui --mcp --port=19749 &
PID=$!
sleep 1
ls /tmp/instance-smoke   # expect one <pid>.json file
cat /tmp/instance-smoke/*.json   # expect pid/started_at/project_path/transport/host_agent_hint fields
kill $PID
sleep 1
ls /tmp/instance-smoke   # expect empty — clean shutdown removed it
rm -rf /tmp/instance-smoke
```

- [ ] **Step 4: Update the progress ledger**

Append to `.superpowers/sdd/progress.md` a line noting PR7a (R2.2.1 instance registry + R2.2.2 orphan reaping) complete, commits `<first-short-sha>..<last-short-sha>`, review status once run — matching the existing entries' style for PR4/PR6/PR9 (`=== PR7a ... stacked on feat/health-beacons ===` section header, per-task complete lines, no push/PR note). Do not push or open a PR — per the Global Constraints, that happens later at a point the user decides across the whole stack.

- [ ] **Step 5: Note PR7b follow-up**

Record in the ledger: PR7b (`mcp.lock` identity migration onto the `lockfile` module, build-hash takeover handshake, wedged-holder detection, lock-holder heartbeat — R2.3.1/2.3.2/2.3.2a/2.3.3/2.3.5) remains open, planned to reuse this PR's `current_process_start_time`/PID-reuse-guard building blocks directly rather than re-deriving PID-liveness logic.

---

## Self-Review Notes

- **Spec coverage:** R2.2.1 (register on startup, remove on clean shutdown, pid/start-time/project-path/transport/host-agent-hint fields) — Tasks 1-2. R2.2.2 (peer-vs-orphan classification, SIGTERM→grace→SIGKILL reaping, startup scan, 10-min periodic timer) — Tasks 3-4. R2.2.3 explicitly out of scope (already shipped separately) and not touched by any task. R2.3.x explicitly out of scope (PR7b).
- **PID-reuse guard has a dedicated test, not just a comment:** `classify_instances_distinguishes_live_dead_and_reused`'s pid-300 case directly targets "a different process now happens to have this PID" — the failure mode plain PID-liveness (`kill(pid, 0)`) can't distinguish from a genuinely live peer, which is the whole reason `sysinfo`/start-time comparison is in this plan instead of a simpler check.
- **No placeholders:** every step has complete, runnable code; the one explicit "verify before trusting" caveat (Task 1 Step 1, `sysinfo`'s exact API shape) is a real external-dependency-version risk being flagged honestly, not a stand-in for undecided logic — the semantics it must preserve are stated precisely.
- **Type/signature consistency:** `InstanceInfo`, `InstanceStatus`, `classify_instances`, `reap_orphan`, `reap_orphans_once` are defined once in Task 1/3 and consumed with matching signatures in Task 2/4 — no renamed duplicates.
- **No new branch, no push** — matches the standing stacking directive covering every PR since PR4; Task 5 stops at updating the progress ledger, no `git push`/`gh pr create`.
