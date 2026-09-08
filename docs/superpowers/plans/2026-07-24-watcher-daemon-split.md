# Watcher Daemon Split (toggle-gated) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let MCP's watcher use the same detached, per-repo `infigraph watch` daemon process the CLI already uses, instead of an in-process thread tied to the MCP worker's own lifetime — gated behind an opt-in, off-by-default env var so today's behavior is unchanged unless explicitly enabled.

**Architecture:** The actual file-watching engine (`infigraph_core::watch::watch_project`) is already shared between the CLI and MCP; only the *coordination* differs — CLI spawns a real detached OS process (`spawn_watcher` in `infigraph-cli`), MCP spawns an in-process thread tracked in a `WATCHERS` map. This plan generalizes the CLI's daemon-spawn primitive into `infigraph-core` (fixing its hard-coded self-re-exec assumption so a non-CLI caller can use it), migrates the ad hoc `watch.lock` flock usage onto the existing `infigraph_core::lockfile` module (gaining cross-process holder identity for free), and wires MCP's watch tools to use the shared daemon primitive when `INFIGRAPH_WATCH_DAEMON=1` is set — while leaving today's in-process-thread path completely untouched when it isn't.

Two duplicate `INFIGRAPH_BACKEND`-checking functions (`is_neo4j_backend` in `infigraph-cli`, `is_remote_mode` in `infigraph-mcp`) are consolidated into one `infigraph_core::watch::daemon::is_remote_backend()`, and the CLI's `ensure_watcher_running`/`cmd_watch` — which currently have **no** remote-mode gate at all, unlike MCP's `tool_watch_project`/`auto_start_watch_inner`, which already both correctly call the single `is_remote_mode()` — pick up that gate as a side effect of routing through the shared primitive.

**Tech Stack:** Rust (edition 2021), `fs2` (already a dependency), `libc` (unix, already used by `infigraph-cli` for `setsid()` — added to `infigraph-core` as part of this plan), no new external dependencies.

## Global Constraints

- **Toggle: opt-in, off by default.** New env var `INFIGRAPH_WATCH_DAEMON` — daemon mode only activates when set to `"1"`. Unset (the default) preserves today's in-process-thread behavior byte-for-byte. This is a hard requirement confirmed with the user, not a suggestion — every task that branches on the toggle must default to the *existing* code path.
- **Branch: base directly on a freshly-fetched `upstream/main`**, not on `feat/health-beacons`, not on any of fork PRs #4-#8, not on PR7b (which hasn't started). This branch must be fully independent — no unmerged prerequisite commits — so it can be cherry-picked or ported anywhere (local `main`, the fork, or submitted upstream) without dependency entanglement. Remotes: `upstream` = `https://github.com/intuit/infigraph.git`, `origin` = your fork (`pradeepmouli/infigraph`) — confirmed via `git remote -v`. At execution time, run `git fetch upstream && git checkout -b feat/watcher-daemon-split upstream/main` before Task 1 — do not branch off a stale local `main` or off `origin`, since either could be missing recently-merged upstream commits (e.g. anything merged since the PR #4-#8 split).
- **Target: this is going to upstream directly** (`intuit/infigraph`, base `main`), per explicit user instruction — this overrides the repo's standing "fork-only, user curates upstream submissions" default for this one PR. Do not default back to fork-only without asking.
- Every cargo command runs with `CARGO_PROFILE_DEV_DEBUG=0` (hard rule — mixing debug settings spawns multi-GB duplicate build trees / ENOSPC).
- Commit with `--no-verify` only after running `cargo fmt` manually, and only if the pre-commit hook fails on the pre-existing, already-catalogued `write_lock_perf::test_contended_lock_throughput` environmental flake (same disclosed pattern used throughout this campaign) — any other failure must be fixed, not bypassed.
- No task in this plan touches `infigraph-cli`'s CLI argument parsing / subcommand dispatch (`main.rs`) — only the internal `pub(crate)` implementation functions `ensure_watcher_running`, `spawn_watcher`, `cmd_watch`, `cmd_watch_stop`, `watcher_is_alive`, `acquire_watch_lock` in `index.rs`/`info_commands.rs`. The `watch`/`watch-stop`/`watch-status` subcommand surface is unchanged.
- `pending_reindex`/cross-file-call visibility (`get_watch_status`'s "⚠ N file(s) changed with cross-file calls" report) has no cross-process equivalent yet. Out of scope for this plan — daemon mode's `get_watch_status` output for that specific field will report "unknown" rather than a real pending list; call this out explicitly in the PR description as a known, accepted gap, not something to silently drop.

---

### Task 1: Core daemon-spawn primitive + consolidated env checks (`infigraph-core`)

**Files:**
- Create: `crates/infigraph-core/src/watch/daemon.rs`
- Modify: `crates/infigraph-core/src/watch/mod.rs` (add `pub mod daemon;` near the top, alongside other `pub mod` / `pub use` lines — check current exact list before editing)
- Modify: `crates/infigraph-core/Cargo.toml` — add a unix-only `libc` dependency (check `crates/infigraph-cli/Cargo.toml` for its exact existing `[target.'cfg(unix)'.dependencies]` declaration and mirror the same version pin)
- Test: `crates/infigraph-core/tests/watch_daemon.rs`

**Interfaces:**
- Produces: `pub fn is_remote_backend() -> bool`, `pub fn is_ci_env() -> bool`, `pub fn watch_daemon_mode_enabled() -> bool`, `pub enum DaemonStartOutcome { AlreadyRunning, Spawned, Failed(String) }`, `pub fn ensure_daemon_running(root: &Path, watch_binary: &Path) -> DaemonStartOutcome`. Task 3 (CLI) and Task 4 (MCP) both consume all of these from `infigraph_core::watch::daemon::*`.

- [ ] **Step 1: Write the failing tests**

Create `crates/infigraph-core/tests/watch_daemon.rs`:

```rust
use infigraph_core::watch::daemon::{is_ci_env, is_remote_backend, watch_daemon_mode_enabled};

/// Serializes tests that mutate process-global env vars — cargo runs this
/// binary's tests on parallel threads, so a lowered override in one test
/// must not leak into another test's window (same lesson as PR6's lockfile
/// tests and PR5's idle-decision tests).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn is_remote_backend_only_true_for_explicit_neo4j() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert!(!is_remote_backend());
    std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
    assert!(!is_remote_backend());
    std::env::set_var("INFIGRAPH_BACKEND", "neo4j");
    assert!(is_remote_backend());
    std::env::remove_var("INFIGRAPH_BACKEND");
}

#[test]
fn is_ci_env_detects_any_known_ci_var() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for v in ["CI", "GITHUB_ACTIONS", "JENKINS_URL", "BUILDKITE", "GITLAB_CI", "INFIGRAPH_NO_WATCH"] {
        std::env::remove_var(v);
    }
    assert!(!is_ci_env());
    std::env::set_var("INFIGRAPH_NO_WATCH", "1");
    assert!(is_ci_env());
    std::env::remove_var("INFIGRAPH_NO_WATCH");
}

#[test]
fn watch_daemon_mode_is_opt_in_off_by_default() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
    assert!(!watch_daemon_mode_enabled());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "1");
    assert!(watch_daemon_mode_enabled());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "0");
    assert!(!watch_daemon_mode_enabled());
    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
}

#[test]
fn ensure_daemon_running_noops_under_ci() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("CI", "1");
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();
    let outcome = infigraph_core::watch::daemon::ensure_daemon_running(
        tmp.path(),
        std::path::Path::new("/nonexistent/infigraph"),
    );
    assert_eq!(outcome, infigraph_core::watch::daemon::DaemonStartOutcome::AlreadyRunning);
    std::env::remove_var("CI");
}
```

(`tempfile` is already a dev-dependency of `infigraph-core` — check `Cargo.toml`'s `[dev-dependencies]` before assuming; it's used by existing lockfile/watch tests in this crate.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test watch_daemon`
Expected: COMPILE ERROR — `infigraph_core::watch::daemon` module does not exist.

- [ ] **Step 3: Implement**

Create `crates/infigraph-core/src/watch/daemon.rs`:

```rust
//! Cross-process "ensure a watcher daemon is running for this repo"
//! primitive, shared by the CLI's `infigraph watch` auto-start and MCP's
//! opportunistic/bootstrap watch-start paths (toggle-gated — see
//! `watch_daemon_mode_enabled`). Generalizes what used to be CLI-only
//! (`spawn_watcher`/`ensure_watcher_running` in `infigraph-cli`), which
//! assumed the calling process *was* the binary to re-exec
//! (`std::env::current_exe()`) — that assumption breaks when the caller is
//! `infigraph-mcp`, which has no `watch` subcommand of its own. Callers now
//! pass the target binary path explicitly.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;

const CI_ENV_VARS: &[&str] = &[
    "CI",
    "GITHUB_ACTIONS",
    "JENKINS_URL",
    "BUILDKITE",
    "GITLAB_CI",
    "INFIGRAPH_NO_WATCH",
];

/// True under CI or when watching has been explicitly opted out of via
/// `INFIGRAPH_NO_WATCH`. Single source of truth — previously duplicated
/// verbatim as `infigraph-cli::index::is_ci()`.
pub fn is_ci_env() -> bool {
    CI_ENV_VARS.iter().any(|v| std::env::var_os(v).is_some())
}

/// Whether the active backend is remote (Neo4j/Postgres) rather than the
/// default local Kùzu. Single source of truth — previously duplicated as
/// `is_neo4j_backend()` (infigraph-cli) and `is_remote_mode()`
/// (infigraph-mcp), both checking the same `INFIGRAPH_BACKEND` env var.
/// File watching is meaningless under remote mode: reindexing there is
/// driven by webhooks, not local file-change events.
pub fn is_remote_backend() -> bool {
    std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false)
}

/// Opt-in toggle for the external-daemon watcher model. Off by default:
/// existing in-process-thread behavior (an MCP worker spawning a watcher
/// thread that dies with the worker) is unchanged unless this is set to
/// `"1"`.
pub fn watch_daemon_mode_enabled() -> bool {
    std::env::var("INFIGRAPH_WATCH_DAEMON")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Outcome of an `ensure_daemon_running` call.
#[derive(Debug, PartialEq, Eq)]
pub enum DaemonStartOutcome {
    /// A daemon is already alive for this repo (lock held), or watching is
    /// inapplicable (CI / remote backend) — no-op either way.
    AlreadyRunning,
    /// This call won the lock race and spawned a new daemon process.
    Spawned,
    /// Spawn was attempted but failed (e.g. binary not found).
    Failed(String),
}

/// Ensure a detached `infigraph watch` daemon is running for `root`,
/// re-exec'ing `watch_binary` (the CLI binary path — the CLI passes its own
/// `current_exe()`; MCP passes a resolved sibling binary path, since
/// `infigraph-mcp` has no `watch` subcommand). Coordinates through the same
/// `.infigraph/watch.lock` every watcher entry point already uses, so this
/// is safe to call redundantly from multiple processes — at most one spawn
/// wins. Note: there is a narrow, pre-existing race between the trial lock
/// probe below and the daemon's own lock acquisition at startup — this
/// mirrors the original CLI implementation's behavior exactly and is not
/// newly introduced here; closing it is out of scope for this plan.
pub fn ensure_daemon_running(root: &Path, watch_binary: &Path) -> DaemonStartOutcome {
    if is_ci_env() || is_remote_backend() {
        return DaemonStartOutcome::AlreadyRunning;
    }

    let tg_dir = root.join(".infigraph");
    if !tg_dir.exists() {
        return DaemonStartOutcome::Failed("not an indexed project (.infigraph missing)".into());
    }

    let lock_path = tg_dir.join("watch.lock");
    match crate::lockfile::try_acquire(&lock_path, "watch-daemon-probe") {
        Ok(Some(guard)) => {
            // We won the trial lock — nobody else is watching. Release it
            // immediately (the daemon process re-acquires its own
            // long-lived hold on startup) and spawn.
            drop(guard);
            spawn_daemon(root, &tg_dir, watch_binary)
        }
        Ok(None) => DaemonStartOutcome::AlreadyRunning,
        Err(e) => DaemonStartOutcome::Failed(e.to_string()),
    }
}

fn spawn_daemon(root: &Path, tg_dir: &Path, watch_binary: &Path) -> DaemonStartOutcome {
    let log_path = tg_dir.join("watch.log");
    let stderr_target = match std::fs::File::create(&log_path) {
        Ok(f) => Stdio::from(f),
        Err(_) => Stdio::null(),
    };

    let mut cmd = Command::new(watch_binary);
    cmd.arg("watch")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_target);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    match cmd.spawn() {
        Ok(_) => DaemonStartOutcome::Spawned,
        Err(e) => DaemonStartOutcome::Failed(e.to_string()),
    }
}

/// Locate the `infigraph` CLI binary as a sibling of the currently-running
/// executable (used by MCP, which has no `watch` subcommand of its own).
pub fn resolve_cli_binary_sibling_of(current_exe: &Path) -> Result<std::path::PathBuf> {
    let dir = current_exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent directory for {}", current_exe.display()))?;
    let name = if cfg!(windows) { "infigraph.exe" } else { "infigraph" };
    let candidate = dir.join(name);
    if candidate.exists() {
        Ok(candidate)
    } else {
        Err(anyhow::anyhow!(
            "expected infigraph CLI binary at {}",
            candidate.display()
        ))
    }
}
```

Add `pub mod daemon;` to `crates/infigraph-core/src/watch/mod.rs`'s module list.

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test watch_daemon`
Expected: PASS, 4/4.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/infigraph-core/src/watch/daemon.rs crates/infigraph-core/src/watch/mod.rs crates/infigraph-core/Cargo.toml crates/infigraph-core/tests/watch_daemon.rs
git commit -m "feat: shared, toggle-gated watcher-daemon spawn primitive in infigraph-core"
```

---

### Task 2: Migrate `watch.lock` onto `infigraph_core::lockfile`

**Files:**
- Modify: `crates/infigraph-cli/src/info_commands.rs:365-398` (`watcher_is_alive`, `acquire_watch_lock`)
- Modify: `crates/infigraph-mcp/src/tools/watch.rs:71-88, 134-158` (`watcher_running`, `acquire_project_watch_lock`)
- Test: extend `crates/infigraph-core/tests/watch_daemon.rs` (or a new `crates/infigraph-mcp/tests/watcher_lockfile.rs` if MCP-side behavior needs its own process-level test — follow whichever existing test file in `infigraph-mcp/tests/` already covers `watcher_running`/`acquire_project_watch_lock`, e.g. `watcher_concurrency.rs`)

**Interfaces:**
- Consumes: `infigraph_core::lockfile::{try_acquire, acquire, read_holder, LockFile}` (existing, from `crates/infigraph-core/src/lockfile.rs:181,189,211`).
- Produces: `watcher_running`/`acquire_watch_lock`/`acquire_project_watch_lock` keep their existing signatures and call sites — only their *internals* change, so Tasks 3-5 need no interface changes here. `read_holder(&lock_path)` becomes available to Task 5's cross-process status work.

- [ ] **Step 1: Replace raw `fs2` calls with `lockfile` calls**

In `crates/infigraph-cli/src/info_commands.rs`, replace `watcher_is_alive` (lines 365-383) and `acquire_watch_lock` (lines 385-398):

```rust
pub(crate) fn watcher_is_alive(lock_path: &Path) -> bool {
    infigraph_core::lockfile::try_acquire(lock_path, "watch-liveness-probe")
        .ok()
        .flatten()
        .is_none()
}

pub(crate) fn acquire_watch_lock(lock_path: &Path) -> Result<infigraph_core::lockfile::LockFile> {
    infigraph_core::lockfile::try_acquire(lock_path, "cli-watch")?
        .ok_or_else(|| anyhow::anyhow!("another watcher is already running"))
}
```

In `crates/infigraph-mcp/src/tools/watch.rs`, replace `watcher_running`'s lock probe (lines 76-87) and `acquire_project_watch_lock` (lines 134-158):

```rust
pub fn watcher_running(root: &std::path::Path) -> bool {
    let root_str = root.to_string_lossy().replace('\\', "/");
    if is_watching(&root_str) {
        return true;
    }
    let lock_path = root.join(".infigraph").join("watch.lock");
    infigraph_core::lockfile::try_acquire(&lock_path, "watch-liveness-probe")
        .ok()
        .flatten()
        .is_none()
}

fn acquire_project_watch_lock(lock_path: &std::path::Path) -> Result<infigraph_core::lockfile::LockFile> {
    const ATTEMPTS: u32 = 10;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
    let mut last_err = None;
    for _ in 0..ATTEMPTS {
        match infigraph_core::lockfile::try_acquire(lock_path, "mcp-watch") {
            Ok(Some(guard)) => return Ok(guard),
            Ok(None) => {
                last_err = Some(anyhow::anyhow!("lock held"));
                std::thread::sleep(RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap())
}
```

Update the one call site in `tool_watch_project` (`watch.rs:196-202`) that currently reads `.map_err(|_| ...)` on the old `Result<std::fs::File>` — the new `acquire_project_watch_lock` returns `Result<LockFile>`, and the surrounding code only needs the guard held for the watcher thread's lifetime (`let _watch_lock = watch_lock;` at line 233), so no other change is required there.

- [ ] **Step 2: Run existing watcher test suites to confirm no regressions**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_concurrency --test watcher_reindex -- --test-threads=1`
Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-cli`
Expected: all pass unchanged — this task is a pure internal swap, no behavior change (both old and new implementations are exclusive-flock-based; `lockfile::try_acquire` additionally stamps a JSON identity payload on success, which is new information, not a behavior change to lock semantics).

- [ ] **Step 3: Commit**

```bash
cargo fmt
git add crates/infigraph-cli/src/info_commands.rs crates/infigraph-mcp/src/tools/watch.rs
git commit -m "refactor: migrate watch.lock onto the shared lockfile module"
```

---

### Task 3: CLI wiring — route through the shared daemon primitive, add the missing remote-mode gate

**Files:**
- Modify: `crates/infigraph-cli/src/index.rs:416-511` (`CI_ENV_VARS`, `is_ci`, `ensure_watcher_running`, `spawn_watcher`)
- Modify: `crates/infigraph-cli/src/info_commands.rs:314-338` (`cmd_watch`)

**Interfaces:**
- Consumes: `infigraph_core::watch::daemon::{is_ci_env, is_remote_backend, ensure_daemon_running, DaemonStartOutcome}` from Task 1.

- [ ] **Step 1: Simplify `ensure_watcher_running`, remove `spawn_watcher` and the local `CI_ENV_VARS`/`is_ci`**

In `crates/infigraph-cli/src/index.rs`, delete `CI_ENV_VARS` (416-423), `is_ci` (425-427), and `spawn_watcher` (464-511) entirely. Replace `ensure_watcher_running` (429-462) with:

```rust
pub(crate) fn ensure_watcher_running(root: &Path) {
    if infigraph_core::watch::daemon::is_ci_env() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    match infigraph_core::watch::daemon::ensure_daemon_running(root, &exe) {
        infigraph_core::watch::daemon::DaemonStartOutcome::Spawned => {
            eprintln!("[auto-watch] Watcher started");
        }
        infigraph_core::watch::daemon::DaemonStartOutcome::Failed(e) => {
            eprintln!("[auto-watch] Failed to start watcher: {e}");
        }
        infigraph_core::watch::daemon::DaemonStartOutcome::AlreadyRunning => {}
    }
}
```

Check every remaining reference to `is_ci()`/`CI_ENV_VARS` elsewhere in `index.rs` (there were tests referencing `is_ci` per the module's existing test block) — update them to call `infigraph_core::watch::daemon::is_ci_env()` instead, or remove them if they were testing logic that moved to Task 1's own test file (avoid duplicate test coverage of the same logic in two crates).

- [ ] **Step 2: Add the missing remote-mode gate to `cmd_watch`**

In `crates/infigraph-cli/src/info_commands.rs`, at the top of `cmd_watch` (currently starts at line 314):

```rust
pub(crate) fn cmd_watch(root: &Path, debounce: u64) -> Result<()> {
    if infigraph_core::watch::daemon::is_remote_backend() {
        println!(
            "File watching is not supported in remote mode (Neo4j backend). \
             Reindexing is triggered via webhooks instead."
        );
        return Ok(());
    }
    // Hold exclusive lock for lifetime — signals liveness to ensure_watcher_running.
    let lock_path = root.join(".infigraph").join("watch.lock");
    let _lock = acquire_watch_lock(&lock_path)?;
    // ...rest of the function unchanged
```

This closes the gap found during design: MCP's `tool_watch_project`/`auto_start_watch_inner` already both refuse under remote mode via `is_remote_mode()`; the CLI's `ensure_watcher_running` picks up the same gate automatically in Step 1 (it now routes through `ensure_daemon_running`, which checks `is_remote_backend()`), but `cmd_watch` itself — reached directly via `infigraph watch`, bypassing `ensure_watcher_running` — had no check at all until this step.

- [ ] **Step 3: Run tests**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-cli`
Expected: all pass. If any test asserted on `spawn_watcher`'s removed self-reexec behavior directly (check `index.rs`'s existing `#[cfg(test)]` block, referenced test names include `watch_stop_creates_sentinel` at line ~1321), update it to call `infigraph_core::watch::daemon::ensure_daemon_running` instead, keeping the same assertions about sentinel/log-file creation.

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add crates/infigraph-cli/src/index.rs crates/infigraph-cli/src/info_commands.rs
git commit -m "refactor: CLI watcher auto-start routes through shared core primitive; cmd_watch gains remote-mode gate"
```

---

### Task 4: MCP start-path toggle wiring

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/watch.rs:98-127` (`auto_start_watch_inner`)
- Test: `crates/infigraph-mcp/tests/watcher_daemon_mode.rs`

**Interfaces:**
- Consumes: `infigraph_core::watch::daemon::{watch_daemon_mode_enabled, ensure_daemon_running, resolve_cli_binary_sibling_of, DaemonStartOutcome}` from Task 1.
- Produces: `auto_start_watch_inner` keeps its existing `fn auto_start_watch_inner(path: &str, skip_disabled_check: bool) -> Option<String>` signature — callers (`auto_start_watch`, `auto_start_watch_opportunistic`) are unchanged.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-mcp/tests/watcher_daemon_mode.rs`:

```rust
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// With the toggle OFF (default), auto_start_watch must behave exactly as
/// before: an in-process thread, tracked in the WATCHERS map, with no
/// external process spawned. This is the regression guard for "toggle
/// defaults to unchanged behavior".
#[test]
fn daemon_mode_off_by_default_uses_in_process_thread() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();

    let path = root.to_string_lossy().to_string();
    let result = infigraph_mcp::tools::watch::auto_start_watch(&path);
    assert!(result.is_some(), "expected in-process watcher to start");
    assert!(infigraph_mcp::tools::watch::is_watching(&path.replace('\\', "/")));

    // Cleanup
    let mut guard = infigraph_mcp::tools::watch::get_watchers();
    if let Some(map) = guard.as_mut() {
        map.clear();
    }
}
```

(Follow the exact setup/teardown pattern used by existing tests in `crates/infigraph-mcp/tests/watcher_reindex.rs`'s `stop_all_watchers` helper — reuse it here instead of the inline `map.clear()` above if it does more than that, e.g. sending real `stop_tx` signals; check that helper's current implementation before finalizing this test.)

- [ ] **Step 2: Run test to verify it passes against current (pre-Task-4) code**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_daemon_mode -- --test-threads=1`
Expected: PASS already (this test only exercises the toggle-off path, which is today's only path) — it's a regression guard for the next step, not new behavior yet.

- [ ] **Step 3: Implement the toggle branch**

Replace `auto_start_watch_inner` (`watch.rs:98-127`):

```rust
fn auto_start_watch_inner(path: &str, skip_disabled_check: bool) -> Option<String> {
    if is_remote_mode() {
        return None;
    }
    if !skip_disabled_check && watchers_disabled() {
        return None;
    }
    let root = std::path::PathBuf::from(path).canonicalize().ok()?;
    let root_str = root.to_string_lossy().replace('\\', "/");

    if is_watching(&root_str) {
        return None;
    }

    if infigraph_core::watch::daemon::watch_daemon_mode_enabled() {
        let mcp_exe = std::env::current_exe().ok()?;
        let cli_binary = match infigraph_core::watch::daemon::resolve_cli_binary_sibling_of(&mcp_exe) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[auto-watch] could not locate infigraph CLI binary: {e}");
                return None;
            }
        };
        return match infigraph_core::watch::daemon::ensure_daemon_running(&root, &cli_binary) {
            infigraph_core::watch::daemon::DaemonStartOutcome::Spawned => {
                eprintln!("[auto-watch] Started daemon watcher for {root_str}");
                Some(format!("Daemon watcher started for {root_str}"))
            }
            infigraph_core::watch::daemon::DaemonStartOutcome::AlreadyRunning => None,
            infigraph_core::watch::daemon::DaemonStartOutcome::Failed(e) => {
                eprintln!("[auto-watch] Failed to start daemon watcher: {e}");
                None
            }
        };
    }

    let args = serde_json::json!({
        "path": path,
        "auto_resolve": true,
        "debounce_ms": 500
    });
    match tool_watch_project(&args) {
        Ok(msg) => {
            eprintln!("[auto-watch] Started watcher for {root_str}");
            Some(msg)
        }
        Err(e) => {
            eprintln!("[auto-watch] Failed to start watcher: {e}");
            None
        }
    }
}
```

- [ ] **Step 4: Add a daemon-mode-on test**

Append to `crates/infigraph-mcp/tests/watcher_daemon_mode.rs`:

```rust
/// With the toggle ON, auto_start_watch must NOT create an in-process
/// thread — is_watching() (which only reflects the in-process WATCHERS
/// map) must stay false, since the watcher now lives in a separate
/// process. It should instead hold the same watch.lock a spawned daemon
/// would (proven indirectly via watcher_running(), which checks the lock
/// cross-process).
#[test]
fn daemon_mode_on_does_not_populate_in_process_watchers_map() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "1");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let path = root.to_string_lossy().to_string();

    // No infigraph CLI binary sibling exists next to the test binary, so
    // this exercises the "could not locate infigraph CLI binary" failure
    // path — proving daemon mode was taken (not the in-process path) even
    // though the actual spawn can't succeed in this test environment.
    let result = infigraph_mcp::tools::watch::auto_start_watch(&path);
    assert!(result.is_none());
    assert!(!infigraph_mcp::tools::watch::is_watching(
        &path.replace('\\', "/")
    ));

    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_daemon_mode -- --test-threads=1`
Expected: PASS, 2/2.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/infigraph-mcp/src/tools/watch.rs crates/infigraph-mcp/tests/watcher_daemon_mode.rs
git commit -m "feat: MCP watcher auto-start uses the shared daemon primitive under INFIGRAPH_WATCH_DAEMON=1"
```

---

### Task 5: MCP stop/status dual-mode + `handle_initialize` bootstrap skip under daemon mode

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/watch.rs:307-321` (`tool_stop_watch`), `:323-399` (`tool_get_watch_status`)
- Modify: `crates/infigraph-mcp/src/lib.rs:613-664` (`handle_initialize`)
- Test: extend `crates/infigraph-mcp/tests/watcher_daemon_mode.rs`

**Interfaces:**
- Consumes: `infigraph_core::watch::daemon::watch_daemon_mode_enabled` (Task 1), `infigraph_core::lockfile::read_holder` (existing, exposed to this crate since Task 2's migration).

- [ ] **Step 1: Dual-mode `tool_stop_watch`**

`tool_stop_watch` currently only knows in-process `watcher_id`s (from the `WATCHERS`/doc-watcher maps). Add a path-based variant for daemon mode — accept either a `watcher_id` (existing, in-process) or a `path` (new, daemon mode) in `args`:

```rust
pub fn tool_stop_watch(args: &Value) -> Result<String> {
    if let Some(watcher_id) = args.get("watcher_id").and_then(|v| v.as_str()) {
        let mut guard = get_watchers();
        if let Some(map) = guard.as_mut() {
            if let Some(entry) = map.remove(watcher_id) {
                let _ = entry.stop_tx.send(());
                return Ok(format!("Watcher {watcher_id} stopped."));
            }
        }
        return Ok(format!("No watcher found with ID: {watcher_id}"));
    }

    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        let root = std::path::PathBuf::from(path)
            .canonicalize()
            .context("invalid path")?;
        let sentinel = root.join(".infigraph").join("watch.stop");
        let lock_path = root.join(".infigraph").join("watch.lock");
        if !lock_path.exists() {
            return Ok("No watcher running.".to_string());
        }
        let alive = infigraph_core::lockfile::try_acquire(&lock_path, "watch-liveness-probe")
            .ok()
            .flatten()
            .is_none();
        if !alive {
            return Ok("No watcher running.".to_string());
        }
        std::fs::write(&sentinel, b"")?;
        return Ok("Stop signal sent. Watcher will exit within ~1 second.".to_string());
    }

    anyhow::bail!("missing 'watcher_id' or 'path'")
}
```

**CORRECTION (post-review, applied during Task 5 execution — see `.superpowers/sdd/task-5-review.md`):** the liveness check MUST determine "alive" via the flock ONLY (`lock_path.exists()` + pure `try_acquire`), matching the CLI's `watcher_is_alive` (`info_commands.rs:365-380`) exactly. The version originally drafted here also consulted `lockfile::read_holder` as part of the condition — but `lockfile.rs`'s own module doc explicitly says the JSON payload is diagnostics-only and "never trusted for liveness decisions": after an unclean watcher death (SIGKILL/OOM/crash), the flock releases automatically but the stale payload survives (since `Drop` never ran to clear it), so `read_holder` returning `Some` does NOT mean the watcher is still alive. The original code's `read_holder(...).is_none() && try_acquire(...).is_some()` condition falsely read a dead watcher as "alive" in exactly that case, writing a stop sentinel that nothing consumes — which then kills the *next* legitimately-spawned watcher on its first loop iteration (`watch/mod.rs`'s sentinel-poll sees it and exits within ~1s). The corrected code above is what actually shipped (commit `310d8d0` on `feat/watcher-daemon-split`) — use it, not the "straightforward translation" reasoning below, which was the source of the bug.

~~(The liveness check above is a straightforward translation of the CLI's `cmd_watch_stop` logic (`info_commands.rs:340-352`) onto `lockfile::try_acquire`/`read_holder` rather than `watcher_is_alive`, avoiding a second raw-flock helper — both approaches are equivalent since Task 2 made `watcher_is_alive` itself a thin `try_acquire` wrapper.)~~ **This reasoning was wrong — see correction above.**

- [ ] **Step 2: Dual-mode `tool_get_watch_status`**

Add a `path`-scoped branch before the existing `watcher_id`/list-all logic in `tool_get_watch_status` (`watch.rs:323-399`) — when `args` has `path` and no `watcher_id`, report cross-process status via the lock file instead of the in-process map:

```rust
pub fn tool_get_watch_status(args: &Value) -> Result<String> {
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        let root = std::path::PathBuf::from(path)
            .canonicalize()
            .context("invalid path")?;
        let lock_path = root.join(".infigraph").join("watch.lock");
        if !lock_path.exists() {
            return Ok(format!("No watcher running for {path}."));
        }
        let alive = infigraph_core::lockfile::try_acquire(&lock_path, "watch-liveness-probe")
            .ok()
            .flatten()
            .is_none();
        if !alive {
            return Ok(format!("No watcher running for {path}."));
        }
        return Ok(match infigraph_core::lockfile::read_holder(&lock_path) {
            Some(info) => format!(
                "Watcher active for {path}\nHeld by PID {} (role: {}) since epoch {}\n\
                 Note: pending-reindex tracking is not available across processes in \
                 daemon mode — use index_project if unsure whether a reindex is needed.",
                info.pid, info.role, info.acquired_at
            ),
            None => format!(
                "Watcher active for {path} (holder identity unavailable)\n\
                 Note: pending-reindex tracking is not available across processes in \
                 daemon mode — use index_project if unsure whether a reindex is needed."
            ),
        });
    }

    let watcher_id = args.get("watcher_id").and_then(|v| v.as_str());
    // ...rest of the existing function body (lines 326-399) unchanged
```

**CORRECTION (post-review, applied during Task 5 execution — see `.superpowers/sdd/task-5-review.md`):** the version originally drafted here used `read_holder` as the SOLE liveness signal — worse than `tool_stop_watch`'s bug above, since there was no flock check at all. After any unclean daemon death, this reported a dead watcher as "active" with a specific stale PID *indefinitely* (nothing ever clears a payload left by an unclean death, since clearing only happens on a process's own clean `Drop`). The corrected code above gates on the flock first (same pattern as `tool_stop_watch`) and uses `read_holder` only to enrich the message once liveness is already confirmed — never as the liveness determinant. This is what actually shipped (commit `310d8d0`).

- [ ] **Step 3: Skip `handle_initialize`'s bulk bootstrap for daemon mode**

In `crates/infigraph-mcp/src/lib.rs`, inside the `is_primary` thread body (lines 617-645), skip the per-repo `auto_start_watch`/`auto_start_doc_watch` loop when daemon mode is on — a persistent external daemon doesn't need re-arming every time a new primary comes up, since it doesn't die when the MCP worker that started it exits:

```rust
pub fn handle_initialize(id: &Value, is_primary: bool) -> Value {
    mcp_log("INFO", &format!("initialize called (primary={is_primary})"));
    if is_primary {
        std::thread::spawn(|| {
            mcp_log("DEBUG", "init_watchers start");
            tools::watch::init_watchers();
            mcp_log("DEBUG", "init_doc_watchers start");
            tools::docs::init_doc_watchers();

            let daemon_mode = infigraph_core::watch::daemon::watch_daemon_mode_enabled();
            if daemon_mode {
                mcp_log(
                    "INFO",
                    "watch daemon mode active — skipping bulk code-watcher re-arm \
                     (persistent daemons survive worker restarts, opportunistic \
                     per-call starts are sufficient); doc-watcher bootstrap still \
                     runs eagerly",
                );
            }

            let registry = match infigraph_core::multi::Registry::load() {
                Ok(r) => {
                    mcp_log(
                        "DEBUG",
                        &format!("registry loaded: {} repos", r.repos.len()),
                    );
                    r
                }
                Err(e) => {
                    mcp_log("ERROR", &format!("registry load failed: {e}"));
                    return;
                }
            };

            for entry in registry.repos.values() {
                if !entry.path.join(".infigraph").exists() {
                    continue;
                }
                let path = entry.path.to_string_lossy().to_string();
                if !daemon_mode {
                    tools::watch::auto_start_watch(&path);
                }
                tools::docs::auto_start_doc_watch(&path);
            }
        });
    } else {
        mcp_log("INFO", "Skipping watchers — not primary instance");
    }

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "infigraph",
                "version": "0.1.0"
            }
        }
    })
}
```

**CORRECTION (post-review, applied during Task 5 execution — see `.superpowers/sdd/task-5-review.md`):** the version originally drafted here had an early `return;` inside the `if daemon_mode` branch, which skipped the ENTIRE per-repo loop — including `auto_start_doc_watch`, contradicting this very plan's own Global Constraints scope note ("Doc watchers ... are unaffected by this plan's daemon model — still in-process regardless of the toggle"). `init_doc_watchers()` running unconditionally does NOT satisfy that requirement on its own — it only lazily initializes an empty map, it doesn't start any actual doc watcher; only `auto_start_doc_watch` does that. The corrected code above removes the early return and instead gates only `auto_start_watch` per-iteration, so doc-watcher bootstrap stays eager and unconditional as originally intended. This is what actually shipped (commit `310d8d0`). Note also: the `health::HEALTH.mark_initialized();` first line shown in earlier drafts of this plan does not exist in this codebase (no `health` module on this branch) — omit it, as Task 5's implementer correctly did.

(Doc watchers (`tools::docs::auto_start_doc_watch`) are untouched by this plan's daemon model — they keep using the in-process path regardless of `INFIGRAPH_WATCH_DAEMON`, since this plan scopes the daemon split to code watchers only. Note that explicitly in the PR description as a deliberate scope boundary, not an oversight.)

- [ ] **Step 4: Add regression tests**

Append to `crates/infigraph-mcp/tests/watcher_daemon_mode.rs`:

```rust
#[test]
fn stop_watch_by_path_reports_no_watcher_when_none_running() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::watch::tool_stop_watch(&args).unwrap();
    assert_eq!(result, "No watcher running.");
}

#[test]
fn get_watch_status_by_path_reports_no_watcher_when_none_running() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::watch::tool_get_watch_status(&args).unwrap();
    assert!(result.starts_with("No watcher running for"));
}

#[test]
fn get_watch_status_by_path_reports_holder_identity_when_lock_held() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let lock_path = root.join(".infigraph").join("watch.lock");
    let _held = infigraph_core::lockfile::try_acquire(&lock_path, "test-daemon")
        .unwrap()
        .unwrap();

    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::watch::tool_get_watch_status(&args).unwrap();
    assert!(result.contains("role: test-daemon"));
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_daemon_mode -- --test-threads=1`
Expected: PASS, 5/5.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/infigraph-mcp/src/tools/watch.rs crates/infigraph-mcp/src/lib.rs crates/infigraph-mcp/tests/watcher_daemon_mode.rs
git commit -m "feat: path-scoped stop/status for daemon-mode watchers; skip bulk re-arm when daemon mode is on"
```

---

### Task 6: Full verification (both toggle states) + open the upstream PR

**Files:**
- None created; runs suites, pushes, opens PR.

- [ ] **Step 1: Full suites with the toggle OFF (default)**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core -p infigraph-mcp -p infigraph-cli -- --test-threads=4 --no-fail-fast
```

Expected: green, modulo the already-catalogued pre-existing environmental flakes from this campaign. **Updated during this plan's own execution (Tasks 2-4 discovered two more not in this original list)** — the full current catalogue is: `write_lock_perf::test_contended_lock_throughput`, `groups_watch_perf::test_groups_watch_perf`, `tool_parity::advertised_tools_match_mcp_tool_names`, `watcher_concurrency::test_graph_tools_with_group_watchers`, `compression_eval::phase2_compression_eval`, `compression_eval::phase3_dedup_eval`. Any NEW failure is this branch's to resolve before proceeding.

- [ ] **Step 2: Full suites with the toggle ON**

```bash
INFIGRAPH_WATCH_DAEMON=1 CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core -p infigraph-mcp -p infigraph-cli -- --test-threads=4 --no-fail-fast
```

Expected: same result as Step 1 — the toggle must not change pass/fail status for any test not specifically written to exercise it (Tasks 4/5's new tests each explicitly set/unset the var themselves rather than relying on the outer env, so this run is mainly a sanity check that nothing else silently depends on it being unset).

- [ ] **Step 3: Clippy + fmt**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean.

- [ ] **Step 4: Manual smoke check — daemon mode actually spawns a real process**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-cli -p infigraph-mcp
mkdir -p /tmp/watch-smoke && cd /tmp/watch-smoke
target/debug/infigraph index .   # creates .infigraph/
INFIGRAPH_WATCH_DAEMON=1 target/debug/infigraph-mcp --worker --mcp --port=0 <<< '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' &
sleep 2
ps aux | grep "[i]nfigraph watch"   # expect one real "infigraph watch" process, pinned to /tmp/watch-smoke
cat .infigraph/watch.lock            # expect a JSON identity payload with role "cli-watch" or similar
kill %1 2>/dev/null
pkill -f "infigraph watch" 2>/dev/null
```

Confirm the daemon process is visible in `ps`, `watch.lock` carries the new identity payload, and it survives the MCP worker being killed (proving the decoupling actually works) — then clean up manually.

- [ ] **Step 5: Push and open the PR directly against upstream**

```bash
git push -u origin feat/watcher-daemon-split
gh pr create --repo intuit/infigraph --base main --head pradeepmouli:feat/watcher-daemon-split \
  --title "feat: toggle-gated watcher daemon split (opt-in via INFIGRAPH_WATCH_DAEMON)" \
  --body "Lets MCP's file watcher run as the same detached, per-repo \`infigraph watch\` daemon process the CLI already uses, instead of an in-process thread tied to the MCP worker's lifetime. Off by default — set INFIGRAPH_WATCH_DAEMON=1 to opt in; unset, behavior is unchanged. Also consolidates two duplicate INFIGRAPH_BACKEND checks (is_neo4j_backend/is_remote_mode) into one infigraph_core::watch::daemon::is_remote_backend(), and closes a gap where the CLI's ensure_watcher_running/cmd_watch had no remote-mode gate at all (MCP's watch tools already did). Known accepted gap: cross-file-call pending-reindex tracking (get_watch_status's 'files changed with cross-file calls' report) has no cross-process equivalent yet under daemon mode — reported as unknown rather than a real pending list; doc watchers are unaffected by this PR (still in-process regardless of the toggle)."
```

---

## Self-Review Notes

- **Spec coverage:** toggle-gated (Task 1), off-by-default preserved and tested (Task 4 Step 2, Task 6 Steps 1-2), full start/stop/status migration (Tasks 4-5), remote-mode gap closed on the CLI side while confirming MCP's was already correct (Task 3 Step 2), `watch.lock` DRY violation fixed (Task 2), `is_neo4j_backend`/`is_remote_mode` duplication consolidated (Task 1), `spawn_watcher`'s self-re-exec assumption generalized (Task 1's `watch_binary` parameter + `resolve_cli_binary_sibling_of`), `handle_initialize`'s bulk-bootstrap skipped under daemon mode (Task 5 Step 3), independent-of-everything-else branch base (Global Constraints).
- **Explicitly out of scope, called out rather than silently dropped:** cross-process `pending_reindex` visibility (Global Constraints + Task 5's status message + PR body), doc watchers (Task 5 Step 3's note), the narrow trial-lock-then-spawn race in `ensure_daemon_running` (documented as pre-existing, inherited from the original CLI code, not newly introduced).
- **No placeholders:** every step has complete code; CLI subcommand dispatch (`main.rs` arg parsing) is explicitly declared out of scope in Global Constraints rather than referenced vaguely.
