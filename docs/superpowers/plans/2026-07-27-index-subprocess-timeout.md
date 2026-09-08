# Index Subprocess Timeout/Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement R-NEW.3 + R-NEW.4 from `docs/superpowers/specs/2026-07-21-remaining-hardening-design.md` — make `tool_index_project`'s CLI-subprocess call fail fast on a hung/corrupt-graph subprocess instead of blocking forever, and expose the `full` reindex parameter through the MCP schema.

**Architecture:** Replace `tool_index_project`'s blocking, unbounded `Command::output()` call with a spawn + bounded-wait-with-timeout primitive (`run_with_timeout`), wrapped in a retry/escalate policy (`run_with_recovery`): attempt 1 (as requested) → timeout → kill + retry once (same flags) → timeout again → escalate to a forced `--full` attempt → timeout again → give up with a clear, actionable error. The low-level timeout mechanism and the retry/escalate policy are two separate, independently unit-testable functions — the policy is tested via an injected fake `attempt` closure so the 3-attempt sequence is provable without real multi-second waits in tests.

**Tech Stack:** Rust (edition 2021), `std::process::Command`/`Stdio::piped()`, no new dependencies.

## Global Constraints

- **Timeout value: 180 seconds (3 minutes) per attempt.** Anchored empirically: this repo's own reindex takes ~10s cold; 180s comfortably covers real large-repo indexing while catching a hang far short of the hour-plus incident this session lived through twice (once with a bogus ~8.4TB VSZ signature).
- **Retry policy is exactly 3 attempts**: [as-requested, as-requested (retry), forced `--full` (escalation)]. A fourth timeout gives up with an error — this is intentionally NOT infinitely self-healing; a graph that fails even a `--full` rebuild attempt needs human investigation, not more automated retries.
- This is fork-only work on `feat/health-beacons` directly (no new worktree needed — small, self-contained, two files) — actually, given precedent from the last two efforts this session, still use an isolated worktree so a broken intermediate state never touches the tree actively serving this session's own MCP connection. Worktree: `scratchpad/wt-index-timeout`, branch `fix/index-subprocess-timeout`, based on current `feat/health-beacons` HEAD.
- Every `cargo` invocation runs with `CARGO_PROFILE_DEV_DEBUG=0` (hard repo rule).
- The new logging calls use `crate::mcp_log("WARN", ...)` (this file, `crates/infigraph-mcp/src/tools/index.rs`, is a submodule of the `infigraph_mcp` library crate itself — confirmed via existing precedent, e.g. `mcp_log("WARN", "Another MCP instance holds mcp.lock...")` called unqualified from within the same crate elsewhere). Do NOT use `infigraph_mcp::mcp_log` (that qualified form is only correct from the separate `infigraph-mcp` *binary* crate, e.g. `main.rs`) or `eprintln!` (some other `tools/` files use bare `eprintln!` with a bracketed tag, but this plan follows `mcp_log` specifically since these are operationally significant recovery events worth structured logging, matching the `mcp.lock` precedent above).
- Commit with `--no-verify` only if the pre-commit hook fails on a confirmed pre-existing, unrelated environmental flake (this session's known list: `write_lock_perf::test_contended_lock_throughput`, `groups_watch_perf::test_groups_watch_perf`). Any other failure must be investigated as a real regression.

---

### Task 1: Timeout mechanism + retry/escalate policy (unit-tested, no wiring yet)

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/index.rs` (add `RunOutcome`, `run_with_timeout`, `run_with_recovery` + tests — not wired into `tool_index_project` yet, that's Task 2)

**Interfaces:**
- Produces: `enum RunOutcome { Completed { success: bool, output: String }, TimedOut }`, `fn run_with_timeout(cmd: &mut std::process::Command, timeout: std::time::Duration) -> Result<RunOutcome>`, `fn run_with_recovery(attempt: impl FnMut(bool) -> Result<RunOutcome>, full: bool, timeout: std::time::Duration) -> Result<(bool, String)>`. Task 2 wires `run_with_recovery` into `tool_index_project`.

- [ ] **Step 1: Write the failing tests for `run_with_timeout`**

Add this test module at the bottom of `crates/infigraph-mcp/src/tools/index.rs` (this file currently has no `#[cfg(test)] mod tests` block — confirmed via `get_symbols_in_file`, so this creates one). These tests use real short-lived subprocesses (`sh -c "..."`) rather than the real `infigraph` binary, so they're fast and don't depend on this crate's own build:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_completes_fast_command_successfully() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo hello; echo world 1>&2"]);
        let outcome = run_with_timeout(&mut cmd, Duration::from_secs(5)).unwrap();
        match outcome {
            RunOutcome::Completed { success, output } => {
                assert!(success);
                assert!(output.contains("hello"), "missing stdout: {output}");
                assert!(output.contains("world"), "missing stderr: {output}");
            }
            RunOutcome::TimedOut => panic!("expected completion, got TimedOut"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_reports_nonzero_exit_as_not_success() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "exit 1"]);
        let outcome = run_with_timeout(&mut cmd, Duration::from_secs(5)).unwrap();
        match outcome {
            RunOutcome::Completed { success, .. } => assert!(!success),
            RunOutcome::TimedOut => panic!("expected completion, got TimedOut"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_and_reports_timeout_for_a_hung_command() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let start = std::time::Instant::now();
        let outcome = run_with_timeout(&mut cmd, Duration::from_millis(200)).unwrap();
        let elapsed = start.elapsed();
        assert!(matches!(outcome, RunOutcome::TimedOut));
        // Must return close to the timeout, not wait for the full 30s sleep --
        // proves the child was actually killed, not merely abandoned.
        assert!(
            elapsed < Duration::from_secs(5),
            "took {elapsed:?}, should have returned shortly after the 200ms timeout"
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib tools::index::tests -- --nocapture`
Expected: compile error — `run_with_timeout`/`RunOutcome` not defined yet.

- [ ] **Step 3: Implement `RunOutcome` and `run_with_timeout`**

Add this above the `#[cfg(test)] mod tests` block (near the top of the file works too, right after the existing `use` statements — place it right before `pub fn tool_index_project`):

```rust
/// Outcome of a bounded-wait subprocess run.
enum RunOutcome {
    Completed { success: bool, output: String },
    TimedOut,
}

/// Run `cmd` to completion, killing it if it doesn't finish within `timeout`.
///
/// Uses `try_wait()` polling rather than the blocking `.output()`/`.wait()` so a hung
/// child (e.g. `infigraph index` stuck opening a corrupted graph DB) can be killed
/// instead of blocking this thread forever. stdout/stderr are drained continuously on
/// background threads while polling -- `Stdio::piped()` without draining would let the
/// child block on a full pipe buffer during a long, verbose index run.
fn run_with_timeout(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Result<RunOutcome> {
    use std::io::Read;

    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("failed to spawn subprocess")?;

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let start = std::time::Instant::now();
    let poll_interval = std::time::Duration::from_millis(50);
    let status = loop {
        match child.try_wait().context("failed to poll subprocess status")? {
            Some(status) => break Some(status),
            None => {
                if start.elapsed() >= timeout {
                    break None;
                }
                std::thread::sleep(poll_interval);
            }
        }
    };

    match status {
        Some(status) => {
            let stdout = stdout_thread.join().unwrap_or_default();
            let stderr = stderr_thread.join().unwrap_or_default();
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
            Ok(RunOutcome::Completed {
                success: status.success(),
                output: combined,
            })
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Ok(RunOutcome::TimedOut)
        }
    }
}
```

- [ ] **Step 4: Run tests to verify `run_with_timeout`'s tests pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib tools::index::tests -- --nocapture`
Expected: the 3 tests from Step 1 pass. (`run_with_recovery`'s tests, added next, will still fail to compile at this point — that's expected, continue to Step 5.)

- [ ] **Step 5: Write the failing tests for `run_with_recovery`**

Add these tests inside the same `mod tests` block (append after the 3 tests from Step 1). These use a fake `attempt` closure — no real subprocesses, no real waiting, so they run instantly regardless of the 180s production timeout value:

```rust
    #[test]
    fn run_with_recovery_returns_immediately_on_first_success() {
        let mut calls: Vec<bool> = Vec::new();
        let result = run_with_recovery(
            |full| {
                calls.push(full);
                Ok(RunOutcome::Completed {
                    success: true,
                    output: "ok".to_string(),
                })
            },
            false,
            Duration::from_secs(1),
        );
        assert!(result.is_ok());
        let (success, output) = result.unwrap();
        assert!(success);
        assert_eq!(output, "ok");
        assert_eq!(calls, vec![false], "only one attempt should have run");
    }

    #[test]
    fn run_with_recovery_retries_once_with_same_flags_before_escalating() {
        let mut calls: Vec<bool> = Vec::new();
        let mut call_count = 0;
        let result = run_with_recovery(
            |full| {
                calls.push(full);
                call_count += 1;
                if call_count == 1 {
                    Ok(RunOutcome::TimedOut)
                } else {
                    Ok(RunOutcome::Completed {
                        success: true,
                        output: "recovered".to_string(),
                    })
                }
            },
            false,
            Duration::from_millis(1),
        );
        assert!(result.is_ok());
        let (success, output) = result.unwrap();
        assert!(success);
        assert_eq!(output, "recovered");
        assert_eq!(
            calls,
            vec![false, false],
            "second attempt (the plain retry) must use the same `full` value as the first, not escalate yet"
        );
    }

    #[test]
    fn run_with_recovery_escalates_to_full_after_two_timeouts() {
        let mut calls: Vec<bool> = Vec::new();
        let mut call_count = 0;
        let result = run_with_recovery(
            |full| {
                calls.push(full);
                call_count += 1;
                if call_count <= 2 {
                    Ok(RunOutcome::TimedOut)
                } else {
                    Ok(RunOutcome::Completed {
                        success: true,
                        output: "healed by full reindex".to_string(),
                    })
                }
            },
            false,
            Duration::from_millis(1),
        );
        assert!(result.is_ok());
        let (success, _) = result.unwrap();
        assert!(success);
        assert_eq!(
            calls,
            vec![false, false, true],
            "third attempt must be escalated to full=true regardless of the originally requested value"
        );
    }

    #[test]
    fn run_with_recovery_escalates_even_when_full_was_already_requested() {
        // If the caller already asked for --full, attempt 3 stays full=true (no
        // meaningful distinction to escalate further into), and the sequence is
        // still exactly 3 attempts, not fewer or more.
        let mut calls: Vec<bool> = Vec::new();
        let result = run_with_recovery(
            |full| {
                calls.push(full);
                Ok(RunOutcome::TimedOut)
            },
            true,
            Duration::from_millis(1),
        );
        assert!(result.is_err());
        assert_eq!(calls, vec![true, true, true]);
    }

    #[test]
    fn run_with_recovery_gives_up_with_actionable_error_after_three_timeouts() {
        let result = run_with_recovery(
            |_full| Ok(RunOutcome::TimedOut),
            false,
            Duration::from_millis(1),
        );
        let err = result.expect_err("three consecutive timeouts must return Err");
        let msg = err.to_string();
        assert!(
            msg.contains("3 times") || msg.contains("three"),
            "error should mention the attempt count: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("graph") || msg.to_lowercase().contains("corrupt"),
            "error should point at the graph/corruption as the likely cause: {msg}"
        );
    }

    #[test]
    fn run_with_recovery_propagates_a_spawn_error_immediately_without_retrying() {
        let mut calls = 0;
        let result = run_with_recovery(
            |_full| {
                calls += 1;
                Err(anyhow::anyhow!("failed to spawn subprocess: no such file"))
            },
            false,
            Duration::from_secs(1),
        );
        assert!(result.is_err());
        assert_eq!(
            calls, 1,
            "a genuine spawn error (not a timeout) must not trigger the retry/escalate loop"
        );
    }
```

- [ ] **Step 6: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib tools::index::tests -- --nocapture`
Expected: compile error — `run_with_recovery` not defined yet.

- [ ] **Step 7: Implement `run_with_recovery`**

Add this right after `run_with_timeout` (before the `#[cfg(test)]` block):

```rust
/// Run an indexing attempt with automatic recovery from a hung/corrupted graph.
///
/// `attempt(full)` should run one bounded-timeout attempt (typically wrapping
/// `run_with_timeout`) and return its outcome. The sequence is exactly three
/// attempts: as originally requested, a plain retry with the same `full` value
/// (handles transient slowness), then one escalated attempt with `full` forced to
/// `true` (two consecutive timeouts is evidence of corruption, not slowness -- and
/// since indexing's whole job is already to rebuild the graph, self-healing via a
/// full reindex here doesn't cross the same line the general wipe-on-failure
/// caution warns about for other subsystems). A `Result::Err` from `attempt` itself
/// (e.g. a genuine spawn failure) propagates immediately without retrying -- only a
/// `TimedOut` outcome triggers the retry/escalate sequence.
fn run_with_recovery(
    mut attempt: impl FnMut(bool) -> Result<RunOutcome>,
    full: bool,
    timeout: std::time::Duration,
) -> Result<(bool, String)> {
    let attempts = [full, full, true];
    for (i, &attempt_full) in attempts.iter().enumerate() {
        match attempt(attempt_full)? {
            RunOutcome::Completed { success, output } => return Ok((success, output)),
            RunOutcome::TimedOut => {
                if i == 0 {
                    crate::mcp_log(
                        "WARN",
                        &format!(
                            "infigraph index timed out after {timeout:?}, killing and retrying once"
                        ),
                    );
                } else if i == 1 {
                    crate::mcp_log(
                        "WARN",
                        "infigraph index timed out twice in a row -- escalating to --full reindex",
                    );
                }
            }
        }
    }
    Err(anyhow::anyhow!(
        "infigraph index timed out 3 times in a row (including one --full attempt) after \
         {timeout:?} each -- likely unrecoverable graph corruption. Manually inspect/remove \
         .infigraph/graph and .infigraph/graph.wal, then retry."
    ))
}
```

- [ ] **Step 8: Run all of Task 1's tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib tools::index::tests -- --nocapture`
Expected: 9 tests pass (3 from Step 1 + 6 from Step 5), 0 failed.

- [ ] **Step 9: fmt + clippy**

```bash
cargo fmt --all -- --check
CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-mcp --all-targets -- -D warnings
```
Expected: both clean. `run_with_timeout`/`run_with_recovery`/`RunOutcome` are not yet called from `tool_index_project` (Task 2 does that), so clippy may warn `dead_code` at this point — if so, that's expected and Task 2 resolves it; do not add `#[allow(dead_code)]` as a workaround, just confirm the warning disappears once Task 2 wires them in (do not fail this task over it, but do verify Task 2 actually eliminates it in Task 2's own fmt+clippy step).

- [ ] **Step 10: Commit**

```bash
git add crates/infigraph-mcp/src/tools/index.rs
git commit -m "feat: add timeout+kill+retry/escalate primitives for the index subprocess

RunOutcome/run_with_timeout/run_with_recovery -- not yet wired into
tool_index_project (next commit). run_with_timeout replaces an unbounded
Command::output() with a spawn + try_wait() poll loop that kills the
child on timeout, draining stdout/stderr continuously via reader threads
so a verbose child can't deadlock on a full pipe buffer while polling.
run_with_recovery is a 3-attempt policy (as-requested, plain retry,
escalate to --full) decoupled from the subprocess mechanism via an
injected attempt closure, so the retry/escalate sequence is unit-tested
directly without needing to wait through real multi-second timeouts.

Part of R-NEW.3 (docs/superpowers/specs/2026-07-21-remaining-hardening-design.md)
-- this session hit the exact failure mode being fixed here twice: an
`infigraph index` subprocess stuck at ~100% CPU with a bogus ~8.4TB VSZ,
opening a corrupted graph file, blocking its MCP caller for over an hour
with zero feedback."
```

---

### Task 2: Wire recovery into `tool_index_project` + expose `full` in the MCP schema

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/index.rs:21-56` (CLI-subprocess branch of `tool_index_project` — replace `.output()` with `run_with_recovery`)
- Modify: `crates/infigraph-mcp/src/lib.rs` (`build_tools_list`'s `index_project` registration — add `full` to the schema)
- Test: `crates/infigraph-mcp/tests/tool_dispatch.rs` (extend — confirm the schema now advertises `full`)

**Interfaces:**
- Consumes: `RunOutcome`, `run_with_timeout`, `run_with_recovery` from Task 1 (all in the same file, no cross-file import needed for `index.rs`'s own use).

- [ ] **Step 1: Write the failing test for the schema exposure**

Confirmed exact shape by reading `tool_def` (`crates/infigraph-mcp/src/lib.rs:338-348`): it returns `json!({"name": name, "description": description, "inputSchema": {"type": "object", "properties": props, "required": required}})`. Confirmed the call convention by reading the existing precedent `tool_parity.rs::tool_schema_token_budget`, which calls `infigraph_mcp::build_tools_list()` (qualified — this is an integration test file, a separate compilation unit from the crate itself). Add this test to `crates/infigraph-mcp/tests/tool_dispatch.rs`:

```rust
#[test]
fn index_project_schema_exposes_full_param() {
    let tools = infigraph_mcp::build_tools_list();
    let index_project = tools
        .iter()
        .find(|t| t["name"] == "index_project")
        .expect("index_project tool must be registered");
    let props = &index_project["inputSchema"]["properties"];
    assert!(
        props.get("full").is_some(),
        "index_project schema must expose `full` so MCP clients can request a full reindex: {index_project}"
    );
    assert_eq!(
        props["full"]["type"], "boolean",
        "full must be typed as boolean"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test tool_dispatch index_project_schema_exposes_full_param -- --nocapture`
Expected: FAIL — `full` not present in the schema yet.

- [ ] **Step 3: Add `full` to the schema**

In `crates/infigraph-mcp/src/lib.rs`, current registration:

```rust
        tool_def("index_project", "REQUIRED FIRST STEP: Parse all source files and build the code knowledge graph. Must run before any other infigraph tool. Auto-indexes 60+ languages.",
            p(true,false,false,json!({})), &["path"]),
```

Change to:

```rust
        tool_def("index_project", "REQUIRED FIRST STEP: Parse all source files and build the code knowledge graph. Must run before any other infigraph tool. Auto-indexes 60+ languages.",
            p(true,false,false,json!({"full":{"type":"boolean","default":false,"description":"Force a full reindex from scratch instead of incremental (default: false)"}})), &["path"]),
```

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test tool_dispatch index_project_schema_exposes_full_param -- --nocapture`
Expected: PASS.

Also run the full `tool_dispatch` suite to confirm nothing else asserts on the exact shape of `index_project`'s schema in a way this change would break:
Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test tool_dispatch`
Expected: all pass.

- [ ] **Step 5: Replace `tool_index_project`'s blocking `.output()` call with `run_with_recovery`**

Current CLI-subprocess branch (`crates/infigraph-mcp/src/tools/index.rs:21-56` — line numbers may have shifted slightly from Task 1's additions above this function; find it by content, not line number):

```rust
    if let Some(cli) = find_infigraph_cli() {
        let mut cmd = std::process::Command::new(&cli);
        cmd.arg("index").current_dir(path);
        if full {
            cmd.arg("--full");
        }

        let output = cmd
            .output()
            .with_context(|| format!("Failed to run {}", cli.display()))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let combined = format!("{}{}", stdout, stderr);

        if !output.status.success() {
            return Err(anyhow::anyhow!("infigraph index failed:\n{}", combined));
        }
        let mut out = combined;

        // Register in global registry so watchers auto-start on next MCP init
        if let Ok(prism) = open_prism(args) {
            let project_name = std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string());
            let mut registry = infigraph_core::multi::Registry::load().unwrap_or_default();
            let _ = registry.register_repo(&project_name, &std::path::PathBuf::from(path), &prism);
        }

        if let Some(msg) = auto_start_watch(path) {
            out.push_str(&format!("\n{}", msg));
        }
        auto_start_doc_watch(path);
        return Ok(out);
    }
```

Change to:

```rust
    if let Some(cli) = find_infigraph_cli() {
        let build_cmd = |attempt_full: bool| {
            let mut cmd = std::process::Command::new(&cli);
            cmd.arg("index").current_dir(path);
            if attempt_full {
                cmd.arg("--full");
            }
            cmd
        };

        let (success, combined) = run_with_recovery(
            |attempt_full| run_with_timeout(&mut build_cmd(attempt_full), INDEX_SUBPROCESS_TIMEOUT),
            full,
            INDEX_SUBPROCESS_TIMEOUT,
        )?;

        if !success {
            return Err(anyhow::anyhow!("infigraph index failed:\n{}", combined));
        }
        let mut out = combined;

        // Register in global registry so watchers auto-start on next MCP init
        if let Ok(prism) = open_prism(args) {
            let project_name = std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string());
            let mut registry = infigraph_core::multi::Registry::load().unwrap_or_default();
            let _ = registry.register_repo(&project_name, &std::path::PathBuf::from(path), &prism);
        }

        if let Some(msg) = auto_start_watch(path) {
            out.push_str(&format!("\n{}", msg));
        }
        auto_start_doc_watch(path);
        return Ok(out);
    }
```

Add the timeout constant near the top of the file, right after the existing `use` statements (before `#[cfg(feature = "remote")] fn is_remote_mode()`):

```rust
/// Anchored empirically: this repo's own reindex takes ~10s cold, so 3 minutes
/// comfortably covers real large-repo indexing while still catching a hang far
/// short of the hour-plus incidents this session's own corrupted-graph events
/// caused (an `infigraph index` subprocess stuck at ~100% CPU with a bogus ~8.4TB
/// VSZ, blocking its MCP caller indefinitely with zero feedback).
const INDEX_SUBPROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
```

- [ ] **Step 6: Build and run the full `infigraph-mcp` test suite**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-mcp
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test tool_dispatch
```
Expected: builds clean, all lib tests pass (Task 1's 9 + any pre-existing), `tool_dispatch` suite passes including the new schema test.

Also run the integration suites that actually exercise `tool_index_project`'s CLI-subprocess path end-to-end (these spawn a real `infigraph` CLI subprocess, so they exercise the new `run_with_recovery` wiring for real, not just compile-check it):
```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_reindex -- --test-threads=1
```
Expected: all pass (18 tests per this session's own earlier verification of this suite) — confirms the happy path (fast, successful index) still works correctly through the new timeout-wrapped code path, not just the timeout/failure paths Task 1 unit-tested in isolation.

- [ ] **Step 7: Manual smoke test — a genuinely hung subprocess actually gets killed and recovered**

This is the actual behavior this whole plan exists to fix — worth confirming for real, not just via unit tests with fake closures:

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-mcp -p infigraph-cli
mkdir -p /tmp/index-timeout-smoke/.infigraph
# Corrupt the graph file in a way that hangs Kuzu's open rather than failing fast
# (a real corrupted-WAL-style hang is hard to construct deterministically outside
# a real incident; approximate it with a fake `infigraph` binary standing in for
# a hung subprocess instead, to prove the timeout/kill mechanism itself works end
# to end against this crate's real code path, not just Task 1's unit tests).
cat > /tmp/fake-infigraph <<'EOF'
#!/bin/sh
echo "pretending to hang forever..."
sleep 600
EOF
chmod +x /tmp/fake-infigraph
```

Then run a small Rust scratch test (or temporarily point `find_infigraph_cli` — do not modify it permanently, just reason about this manually) is impractical to fully automate here without changing production code; instead, directly unit-test-adjacent verification is already covered by Task 1's `run_with_timeout_kills_and_reports_timeout_for_a_hung_command` test (which uses `sh -c "sleep 30"` as a stand-in hung process and confirms it's killed within the timeout, not left running). Treat that existing Task 1 test as satisfying this manual-verification intent — note in your report that a fully live `infigraph index`-hangs-for-real smoke test was not separately performed beyond Task 1's equivalent-mechanism test, since deterministically reproducing the exact native Kuzu hang outside a real incident isn't practical, and say so explicitly rather than claiming an untested scenario as verified.

Clean up the scratch files:
```bash
rm -rf /tmp/index-timeout-smoke /tmp/fake-infigraph
```

- [ ] **Step 8: fmt + clippy**

```bash
cargo fmt --all -- --check
CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-mcp --all-targets -- -D warnings
```
Expected: both clean, and confirm the `dead_code` warning Task 1 may have left (if any) is now gone since `run_with_timeout`/`run_with_recovery` are wired in.

- [ ] **Step 9: Commit**

```bash
git add crates/infigraph-mcp/src/tools/index.rs crates/infigraph-mcp/src/lib.rs crates/infigraph-mcp/tests/tool_dispatch.rs
git commit -m "fix: tool_index_project fails fast on a hung/corrupt-graph subprocess

Replaces the unbounded, blocking Command::output() call with
run_with_recovery: attempt as requested, kill+retry once on a 180s
timeout (handles transient slowness), escalate to a forced --full
attempt if the retry also times out (two consecutive timeouts is
evidence of corruption, not slowness), give up with a clear, actionable
error if that also times out. Previously a hung child (e.g. a corrupted
graph file causing Kuzu's own open logic to spin) blocked the MCP
worker thread indefinitely -- this session hit that exact failure twice,
once for over an hour, with zero feedback to the caller.

Also exposes `full` as an optional boolean in index_project's MCP
schema (build_tools_list in lib.rs) -- the handler already read
args.get(\"full\") but no MCP client could ever set it, since the
schema never advertised the parameter.

Implements R-NEW.3 + R-NEW.4 from
docs/superpowers/specs/2026-07-21-remaining-hardening-design.md."
```

---

## Self-Review Notes

- **Spec coverage:** R-NEW.3 (timeout/kill/retry/escalate) is Task 1 (mechanism+policy) + Task 2 (wiring). R-NEW.4 (schema exposure) is Task 2 Steps 1-4, bundled per the spec doc's own instruction ("Bundle with R-NEW.3, same file, same PR" — actually different file, `lib.rs`, but same overall change set, which is what the spec meant).
- **No placeholders:** every step shows exact, complete code. The one exception (Task 2 Step 7's manual smoke test) explicitly explains why a fully live end-to-end reproduction isn't practical and points at the equivalent-mechanism unit test instead, rather than hand-waving a fake "TODO: verify manually."
- **Type/interface consistency:** `RunOutcome`, `run_with_timeout(cmd, timeout)`, and `run_with_recovery(attempt, full, timeout)` are defined once in Task 1 and consumed with identical signatures in Task 2 — no renaming or signature drift.
- **Timeout value sourcing:** 180s is stated and justified in Global Constraints, used consistently as `INDEX_SUBPROCESS_TIMEOUT` in Task 2, and Task 1's tests use their own short, independent timeouts (100-200ms for the real-subprocess test, 1ms for the fake-closure tests) — the production constant is never used in a test, so no test actually waits 180s.
