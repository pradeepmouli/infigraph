# MCP Idle Self-Termination (R2.2.3) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An `infigraph-mcp --mcp` worker whose stdio MCP client has disconnected no longer runs forever — it self-terminates after a configurable idle grace period, closing the orphan-accumulation bug behind incident **I-5** ("7 orphaned infigraph-mcp processes found running simultaneously") and implementing requirement **R2.2.3** from `docs/DESIGN-hardening.md` §2.2 (Phase 1, P0).

**Architecture:** `crates/infigraph-mcp/src/main.rs::run()` has three "loop forever" sites. Two of them (`--ui`-only with no `--mcp`, and `--serve`-only with no `--mcp`) are legitimate standing daemons that never had a stdio client — they return before ever reading stdin and must be left untouched. The third — reached only after the stdin read loop actually ran and then hit EOF (the MCP client disconnected) — currently loops forever purely to keep serving the local web UI. That third site is replaced with a bounded poll loop: check elapsed time since stdin closed against a grace period every N seconds, exit cleanly once it elapses. The decision logic and config are pure functions in a new `crates/infigraph-mcp/src/idle.rs` module so they're unit-testable without spawning a real process; the actual process-exit behavior is verified with a real binary-spawn integration test.

**Tech Stack:** Rust (edition 2021), std `Duration`/`Instant`, no new dependencies.

## Global Constraints

- **Scope is R2.2.3 only.** This plan implements *self*-termination (a process noticing its own stdin closed and exiting after idle grace). It does **not** implement R2.2.1 (instance registry file), R2.2.2 (cross-process peer-vs-orphan discrimination at startup), or R2.2.4 (`infigraph ps`/`infigraph kill` CLI — that's explicitly Phase 2 per §9 Phasing). Those remain open P0/P1 work; do not attempt them here.
- **Only the third "loop forever" site is touched.** `crates/infigraph-mcp/src/main.rs::run()` has two other `loop { sleep(3600) }` sites — one gated on `ui_enabled && !mcp_mode && !serve_mode` (pure `--ui`-only daemon, no stdio client ever existed), one gated on `serve_mode && !mcp_mode` (pure `--serve` HTTP daemon, no stdio client ever existed). **Both are legitimate standing-daemon deployment modes and must keep running forever, unmodified.** Only the loop reached *after* `for line in stdin.lock().lines() {...}` completes (i.e., after `mcp_log("INFO", "stdin loop exited");`) gets the idle-grace treatment — that site is only reachable when `mcp_mode` was true and the stdin loop actually ran, meaning there genuinely was a stdio client and it's now gone.
- Default grace period: **300 seconds (5 minutes)**, exact value from `R2.2.3`'s spec text ("default 5 min with stdin closed"). Override via `INFIGRAPH_MCP_IDLE_GRACE_SECS` (seconds). Poll interval: 10s default, override via `INFIGRAPH_MCP_IDLE_POLL_SECS` (seconds) — exists so integration tests don't have to wait a full production-sized interval to observe behavior.
- Branch: `fix/mcp-idle-self-termination`. **Do not create this branch or touch the working tree until told to execute** — at plan-authoring time, `feat/health-beacons` (a different, unrelated PR) has in-flight background test/build activity in the same working tree. Base this branch off whatever the stable tip is at execution time (confirm with the user or check `git log`/`git status` first).
- Commit with `--no-verify` after running `cargo fmt` manually (repo pre-commit hook runs `cargo fmt --check`).
- Every cargo command runs with `CARGO_PROFILE_DEV_DEBUG=0` (hard rule for this repo — mixing debug settings spawns multi-GB duplicate lbug cmake trees / ENOSPC).
- Fork-only PR (standing directive: no upstream PRs without asking; user curates upstream submissions).

---

### Task 1: Pure idle-decision logic + config (`idle.rs`)

**Files:**
- Create: `crates/infigraph-mcp/src/idle.rs`
- Modify: `crates/infigraph-mcp/src/lib.rs` (add `pub mod idle;` to the module list near the top, alongside `compress`, `health`, `recovery`, `session_context`, `tools`, `web`)
- Test: `crates/infigraph-mcp/tests/idle_decision.rs`

**Interfaces:**
- Produces: `pub fn idle_grace_period() -> Duration` (default 300s, env `INFIGRAPH_MCP_IDLE_GRACE_SECS`), `pub fn idle_poll_interval() -> Duration` (default 10s, env `INFIGRAPH_MCP_IDLE_POLL_SECS`), `pub fn should_exit_idle(elapsed: Duration, grace: Duration) -> bool` (pure, boundary-inclusive: `elapsed >= grace`). Task 2 consumes all three from `main.rs` via `infigraph_mcp::idle::*`.

- [ ] **Step 1: Write the failing tests**

Create `crates/infigraph-mcp/tests/idle_decision.rs`:

```rust
use infigraph_mcp::idle::{idle_grace_period, idle_poll_interval, should_exit_idle};
use std::time::Duration;

/// Serializes tests that mutate the process-global INFIGRAPH_MCP_IDLE_*
/// env vars — cargo runs this binary's tests on parallel threads, so a
/// lowered override in one test must not leak into another test's window
/// (lesson from an identical env-var race caught in PR6's lockfile tests).
static IDLE_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn should_exit_idle_boundary() {
    assert!(!should_exit_idle(
        Duration::from_secs(299),
        Duration::from_secs(300)
    ));
    assert!(should_exit_idle(
        Duration::from_secs(300),
        Duration::from_secs(300)
    ));
    assert!(should_exit_idle(
        Duration::from_secs(301),
        Duration::from_secs(300)
    ));
}

#[test]
fn idle_grace_period_default_is_five_minutes() {
    let _env = IDLE_ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_IDLE_GRACE_SECS");
    assert_eq!(idle_grace_period(), Duration::from_secs(300));
}

#[test]
fn idle_grace_period_env_override() {
    let _env = IDLE_ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_MCP_IDLE_GRACE_SECS", "2");
    assert_eq!(idle_grace_period(), Duration::from_secs(2));
    std::env::remove_var("INFIGRAPH_MCP_IDLE_GRACE_SECS");
}

#[test]
fn idle_poll_interval_default_and_override() {
    let _env = IDLE_ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_IDLE_POLL_SECS");
    assert_eq!(idle_poll_interval(), Duration::from_secs(10));
    std::env::set_var("INFIGRAPH_MCP_IDLE_POLL_SECS", "1");
    assert_eq!(idle_poll_interval(), Duration::from_secs(1));
    std::env::remove_var("INFIGRAPH_MCP_IDLE_POLL_SECS");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test idle_decision`
Expected: COMPILE ERROR — `infigraph_mcp::idle` module does not exist.

- [ ] **Step 3: Implement**

Create `crates/infigraph-mcp/src/idle.rs`:

```rust
//! Idle self-termination for the MCP worker after its stdio client
//! disconnects — implements DESIGN-hardening.md R2.2.3 ("Live orphans
//! self-terminate after a configurable idle grace, default 5 min with
//! stdin closed"), closing incident I-5 (orphaned infigraph-mcp processes
//! found running indefinitely after their spawning client was gone).
//!
//! Scope: self-termination only. This does NOT implement the broader
//! instance registry (R2.2.1) or cross-process peer-vs-orphan
//! discrimination (R2.2.2) — those are separate, larger pieces of the
//! same P0 requirement, deliberately out of scope here.

use std::time::Duration;

const DEFAULT_GRACE_SECS: u64 = 300;
const DEFAULT_POLL_SECS: u64 = 10;

/// Grace period after the MCP client's stdio connection closes before this
/// process exits, if it's still alive only to serve the local UI.
/// Overridable via `INFIGRAPH_MCP_IDLE_GRACE_SECS` (seconds).
pub fn idle_grace_period() -> Duration {
    std::env::var("INFIGRAPH_MCP_IDLE_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_GRACE_SECS))
}

/// How often the post-EOF loop wakes to re-check the grace period.
/// Overridable via `INFIGRAPH_MCP_IDLE_POLL_SECS` (seconds) — kept small in
/// tests so they don't wait a full production-sized interval.
pub fn idle_poll_interval() -> Duration {
    std::env::var("INFIGRAPH_MCP_IDLE_POLL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_POLL_SECS))
}

/// Pure: has `elapsed` (time since the MCP client's stdin closed) reached
/// or exceeded `grace`? Boundary is inclusive.
pub fn should_exit_idle(elapsed: Duration, grace: Duration) -> bool {
    elapsed >= grace
}
```

Add `pub mod idle;` to `crates/infigraph-mcp/src/lib.rs`'s module list (alongside the existing `pub mod compress;`, `pub mod health;`, etc. — check current exact list before editing, it may have grown since PR6).

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test idle_decision`
Expected: PASS, 4/4.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/infigraph-mcp/src/idle.rs crates/infigraph-mcp/src/lib.rs crates/infigraph-mcp/tests/idle_decision.rs
git commit --no-verify -m "feat: idle self-termination config + pure decision logic (R2.2.3)"
```

---

### Task 2: Wire into `main.rs::run()` — replace the post-EOF infinite loop only

**Files:**
- Modify: `crates/infigraph-mcp/src/main.rs` — locate the block via the unique log line `mcp_log("INFO", "stdin loop exited");` (this task was planned without re-confirming the exact current line number; it was ~360-370 as of the last full read of this file, near the end of `run()`, well after the `ui_enabled`/`mcp_mode`/`serve_mode` derivation at the top of the function — do NOT touch those two other `loop { sleep(3600) }` sites earlier in the function).
- Test: `crates/infigraph-mcp/tests/idle_shutdown.rs`

**Interfaces:**
- Consumes: `infigraph_mcp::idle::{idle_grace_period, idle_poll_interval, should_exit_idle}` from Task 1.

- [ ] **Step 1: Write the failing tests**

Create `crates/infigraph-mcp/tests/idle_shutdown.rs`:

```rust
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Spawns the real infigraph-mcp binary in --worker --ui --mcp mode,
/// closes its stdin immediately (simulating the MCP client disconnecting
/// right after the handshake), and asserts the process exits on its own
/// within a bounded window once the idle grace period elapses. Uses env
/// overrides (grace=2s, poll=1s) instead of the 300s production default so
/// this test runs in seconds, not minutes.
#[test]
fn worker_exits_after_idle_grace_following_stdin_close() {
    let exe = env!("CARGO_BIN_EXE_infigraph-mcp");
    let mut child = Command::new(exe)
        .args(["--worker", "--ui", "--mcp", "--port=0"])
        .env("INFIGRAPH_MCP_IDLE_GRACE_SECS", "2")
        .env("INFIGRAPH_MCP_IDLE_POLL_SECS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infigraph-mcp");

    // Close stdin immediately — the equivalent of the MCP client
    // disconnecting right after the handshake.
    drop(child.stdin.take());

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "process did not self-terminate within the idle grace period + margin"
        );
        std::thread::sleep(Duration::from_millis(200));
    };

    assert!(status.success(), "expected clean exit, got {status:?}");
}

/// Negative control enforcing the plan's critical scoping constraint: a
/// pure --ui-only invocation (no --mcp flag) never enters the stdin read
/// loop at all — it's a legitimate standing daemon with no stdio client to
/// ever disconnect from, and must NOT be affected by the idle-grace logic
/// even though the same INFIGRAPH_MCP_IDLE_* env vars are set.
#[test]
fn ui_only_daemon_without_mcp_flag_is_unaffected_by_idle_grace() {
    let exe = env!("CARGO_BIN_EXE_infigraph-mcp");
    let mut child = Command::new(exe)
        .args(["--worker", "--ui", "--port=0"])
        .env("INFIGRAPH_MCP_IDLE_GRACE_SECS", "1")
        .env("INFIGRAPH_MCP_IDLE_POLL_SECS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infigraph-mcp");

    std::thread::sleep(Duration::from_secs(3));
    let status = child.try_wait().expect("try_wait");
    assert!(
        status.is_none(),
        "a --ui-only (no --mcp) daemon must keep running past the idle grace period \
         window — it never entered the stdin loop, so idle-grace logic must not apply"
    );

    child.kill().expect("cleanup: kill still-running daemon");
    let _ = child.wait();
}
```

If `--port=0` errors inside `web::start_ui_server` (OS-assigned free port not supported there — verify empirically, this was not confirmed while writing this plan), switch both tests to a fixed high port unlikely to collide with the real running instance on 9749 (e.g. `--port=19749`) instead.

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test idle_shutdown -- --test-threads=1`
Expected: `worker_exits_after_idle_grace_following_stdin_close` FAILS (times out / assertion at the 15s deadline) against the current unmodified code, since today it loops forever. `ui_only_daemon_without_mcp_flag_is_unaffected_by_idle_grace` PASSES already (current code already keeps that mode alive forever) — that's fine, it's asserting the invariant this task must not break, not new behavior.

- [ ] **Step 3: Implement**

In `crates/infigraph-mcp/src/main.rs`, replace:

```rust
    mcp_log("INFO", "stdin loop exited");

    // If UI mode is active, keep process alive after stdin EOF (web server still serving)
    if ui_enabled {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    Ok(())
}
```

with:

```rust
    mcp_log("INFO", "stdin loop exited");

    // Reaching here means the MCP client's stdio connection closed — the
    // two OTHER "loop forever" branches earlier in this function (pure
    // --ui-only, pure --serve-only, neither with --mcp) return before ever
    // entering the stdin read loop, since they never had a stdio client to
    // begin with; they are legitimate standing daemons and are untouched
    // by this block. If --ui is also active, someone might still have the
    // local web UI open, so don't exit instantly — but don't loop forever
    // either (DESIGN-hardening.md I-5 / R2.2.3): self-terminate after an
    // idle grace period.
    if ui_enabled {
        let grace = infigraph_mcp::idle::idle_grace_period();
        let poll = infigraph_mcp::idle::idle_poll_interval();
        mcp_log(
            "INFO",
            &format!(
                "MCP client disconnected; UI still serving — exiting after {}s idle unless reconnected",
                grace.as_secs()
            ),
        );
        let stdin_closed_at = std::time::Instant::now();
        loop {
            std::thread::sleep(poll);
            if infigraph_mcp::idle::should_exit_idle(stdin_closed_at.elapsed(), grace) {
                mcp_log(
                    "INFO",
                    &format!(
                        "Idle grace period ({}s) elapsed since MCP client disconnected — exiting",
                        grace.as_secs()
                    ),
                );
                break;
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test idle_shutdown -- --test-threads=1`
Expected: PASS, 2/2. (`--test-threads=1` because these spawn real child processes bound to specific ports and check exact timing — avoid any cross-test port contention.)

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/infigraph-mcp/src/main.rs crates/infigraph-mcp/tests/idle_shutdown.rs
git commit --no-verify -m "fix: MCP worker self-terminates after idle grace instead of looping forever post-EOF (I-5, R2.2.3)"
```

---

### Task 3: Full verification + standalone fork PR

**Files:**
- None created; runs suites, pushes, opens PR.

- [ ] **Step 1: Full test suites**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp -- --test-threads=4 --no-fail-fast
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core -- --test-threads=4 --no-fail-fast
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-cli -- --test-threads=4 --no-fail-fast
```

Expected: green, modulo the already-catalogued pre-existing failures from this campaign (mcp `tool_parity::advertised_tools_match_mcp_tool_names`, mcp `watcher_concurrency::test_graph_tools_with_group_watchers`, mcp `--lib compress::tests::test_compress_pipeline_safe_normal_path`, core `f16_quality::compare_f16_vs_int8_quality`). Any NEW failure is this fix's to resolve before proceeding.

- [ ] **Step 2: Clippy**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-mcp -- -D warnings`
Expected: clean on files touched by this branch.

- [ ] **Step 3: Manual smoke check (optional but cheap)**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-mcp
target/debug/infigraph-mcp --worker --ui --mcp --port=19749 &
PID=$!
sleep 1
kill -0 $PID && echo "alive as expected"
# simulate disconnect: this backgrounded shell's stdin isn't attached to
# the child in a way that reliably closes it via `kill`; the automated
# integration test in Task 2 is the real coverage. Just confirm no
# immediate crash, then clean up:
kill $PID 2>/dev/null
```

- [ ] **Step 4: Push and open the fork PR**

```bash
git push -u origin fix/mcp-idle-self-termination
gh pr create --repo pradeepmouli/infigraph --base main --head fix/mcp-idle-self-termination \
  --title "fix: MCP worker self-terminates after idle grace instead of orphaning forever (I-5, R2.2.3)" \
  --body "Implements DESIGN-hardening.md R2.2.3 (Phase 1, P0) — closes incident I-5, confirmed empirically: two real orphaned infigraph-mcp workers found running 5+ hours with PPID 1 (parent gone) after their MCP client disconnected, because the post-stdin-EOF branch looped forever unconditionally when --ui was active. Scope: self-termination only (default 5 min idle grace after stdin closes, env-overridable) — does NOT implement the broader instance registry (R2.2.1) or cross-process peer/orphan discrimination (R2.2.2), which remain open P0 follow-up work. The two other legitimate standing-daemon modes (--ui-only, --serve-only, neither with --mcp) are explicitly untouched and covered by a negative-control test."
```

Confirm base branch (`main` assumed here — verify against whatever's actually current at execution time; this is independent of the `feat/health-beacons`/PR1-6 stack, so it should NOT be stacked on that branch).

- [ ] **Step 5: Note follow-ups**

Record in the PR description or a tracking note: R2.2.1 (instance registry: `~/.infigraph/instances/<pid>.json`), R2.2.2 (peer-vs-orphan discrimination at startup, so a NEW process can safely reap OTHER dead orphans it finds, not just self-terminate), and R2.2.4 (`infigraph ps`/`infigraph kill` CLI, Phase 2) remain open — this fix closes only the empirically-observed "orphan runs forever" symptom via self-termination, not the full P0 requirement section.

---

## Self-Review Notes

- **Spec coverage:** R2.2.3 fully covered (self-termination, default 5 min, stdin-closed trigger, configurable). R2.2.1/R2.2.2/R2.2.4 explicitly out of scope and called out in three places (Global Constraints, PR body, Task 3 Step 5) so this doesn't get mistaken for closing all of R2.2.
- **Critical scoping constraint has a test, not just a comment:** `ui_only_daemon_without_mcp_flag_is_unaffected_by_idle_grace` directly enforces "don't touch the other two loop-forever sites" — this is the most likely regression an implementer or reviewer might introduce by generalizing the fix too far.
- **No new dependencies**, no changes to any other module's behavior — `idle.rs` is net-new, `main.rs`'s only change is the one block.
- **Env var naming** (`INFIGRAPH_MCP_IDLE_GRACE_SECS`, `INFIGRAPH_MCP_IDLE_POLL_SECS`) follows the established `INFIGRAPH_SLOW_LOCK_MS` pattern from the health-beacons work, not invented from scratch.
