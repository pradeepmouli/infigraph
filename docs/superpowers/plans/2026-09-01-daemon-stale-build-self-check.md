# Daemon Stale-Build Self-Check Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix [issue #134](https://github.com/pradeepmouli/infigraph/issues/134) — a running `infigraph daemon` process periodically checks whether the on-disk binary has changed since it started, and self-exits gracefully if so, instead of relying entirely on an external caller to lazily invoke `prune_stale_daemon` on its behalf.

**Architecture:** A new hidden CLI subcommand (`infigraph print-build-hash`) gives any process a way to read what a *freshly-exec'd* copy of the currently-installed binary reports as its build hash — this is the only way to learn "what's on disk right now," since `infigraph_core::build_hash()` is a compile-time constant baked into whatever binary is already running. The daemon's existing coordinator loop (`run_write_coordinator`) periodically spawns this subcommand via its own `std::env::current_exe()`, compares the result against its own in-process `build_hash()`, and on a mismatch logs and breaks the loop — exactly the same shutdown mechanism the loop's existing `!root.exists()` check already uses.

**Tech Stack:** Rust, `std::process::Command` (subprocess spawn, no new dependency), `clap` (hidden subcommand).

## Global Constraints

- Must not change `run_write_coordinator`'s function signature — the check is self-contained using only `std::env::current_exe()` and a raw env-var test hook, matching the existing `INFIGRAPH_TEST_DAEMON_PANIC` convention (intentionally undocumented, not migrated onto the `settings!` macro).
- Must not change `should_auto_watch`'s behavior — verified during planning that it's an allowlist (`matches!` on specific `Index*`/`ScipImport` variants), so the new hidden command is excluded by construction; no code change needed there.
- On a subprocess-spawn or non-zero-exit failure (as opposed to a successful-but-different build hash), log a warning and skip that interval — never crash or self-exit on a failed *check*, only on a confirmed *mismatch*.
- Env var names for the two test-only hooks: `INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE` (path to a file whose trimmed contents `print-build-hash` prints instead of the real `build_hash()`, when the file exists and is readable) and `INFIGRAPH_TEST_BUILD_HASH_CHECK_SECS` (overrides the 300s production interval). Both undocumented test-only escape hatches, not first-class settings.
- Verification per crate, one at a time (not concurrently): `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core -- --test-threads=1` and `-p infigraph-cli`.
- Reference `#134` in commit messages as plain text, not the `Closes #134` keyword — this branch (`feat/hardening`) lands via local merge + push, not a GitHub PR merge, so the closing keyword wouldn't auto-close anyway and closing the issue is a separate manual step once this reaches `main`.

---

### Task 1: `print-build-hash` hidden CLI subcommand

**Files:**
- Modify: `crates/infigraph-cli/src/main.rs`
- Test: `crates/infigraph-cli/tests/print_build_hash.rs` (new)

**Interfaces:**
- Produces: the `infigraph print-build-hash` subcommand — prints a single trimmed line to stdout: either the real `infigraph_core::build_hash()`, or (test-only) the trimmed contents of the file at `INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE` if that env var is set and the file exists and is readable. Consumed by Task 2's daemon self-check.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-cli/tests/print_build_hash.rs`:

```rust
use std::process::Command;

fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_infigraph")
}

#[test]
fn prints_the_real_build_hash_by_default() {
    let output = Command::new(cli_bin())
        .arg("print-build-hash")
        .env_remove("INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE")
        .output()
        .expect("failed to run infigraph print-build-hash");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), infigraph_core::build_hash());
}

#[test]
fn prints_the_override_file_contents_when_set() {
    let dir = tempfile::tempdir().unwrap();
    let override_path = dir.path().join("fake-hash.txt");
    std::fs::write(&override_path, "fake-build-hash-123\n").unwrap();

    let output = Command::new(cli_bin())
        .arg("print-build-hash")
        .env("INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE", &override_path)
        .output()
        .expect("failed to run infigraph print-build-hash");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "fake-build-hash-123");
}
```

Add `tempfile` to `crates/infigraph-cli/Cargo.toml`'s `[dev-dependencies]` if it isn't already there — check first with `mcp__infigraph__search_code` for `pattern: "^tempfile"`, `file_pattern: "crates/infigraph-cli/Cargo.toml"`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-cli --test print_build_hash`
Expected: FAIL to compile or run — `print-build-hash` isn't a recognized subcommand yet.

- [ ] **Step 3: Write minimal implementation**

In `crates/infigraph-cli/src/main.rs`, find the `Commands` enum (verify current exact content around the existing hidden commands with `mcp__infigraph__search_code` for `pattern: "ScipEnrich"` first — line numbers below are from planning-time research and may have drifted). Add a new hidden variant right after the existing `ScipEnrich` variant:

```rust
    /// Print this binary's build hash and exit (dev/internal use — lets a
    /// running daemon detect it's stale relative to whatever binary is
    /// currently on disk, since its own in-process build_hash() is a
    /// compile-time constant that can't observe a rebuild that happened
    /// after it started).
    #[command(hide = true)]
    PrintBuildHash,
```

In the `run()` function's `match command { ... }` block, add the dispatch arm (find the existing `Commands::ScipEnrich { languages } => { ... }` arm first and add this near it):

```rust
        Commands::PrintBuildHash => {
            cmd_print_build_hash();
            Ok(())
        }
```

Add the handler function itself, near `cmd_daemon` or another small standalone command function in `crates/infigraph-cli/src/main.rs` (not `info_commands.rs` — this is main.rs-local, single-purpose, and doesn't need the library-crate visibility `cmd_daemon` needed for its own reasons):

```rust
/// See `Commands::PrintBuildHash`'s doc comment for why this exists.
fn cmd_print_build_hash() {
    if let Ok(path) = std::env::var("INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE") {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            println!("{}", contents.trim());
            return;
        }
    }
    println!("{}", infigraph_core::build_hash());
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p infigraph-cli --test print_build_hash`
Expected: PASS (both tests)

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-cli/src/main.rs crates/infigraph-cli/tests/print_build_hash.rs crates/infigraph-cli/Cargo.toml
git commit -m "feat(cli): add hidden print-build-hash subcommand

Prerequisite for #134's daemon stale-build self-check (Task 2) -- gives
any process a way to read what a freshly-exec'd copy of the
currently-installed binary reports as its build hash, since
infigraph_core::build_hash() is a compile-time constant a long-running
process can't use to observe a rebuild that happened after it started."
```

---

### Task 2: Daemon periodic stale-build self-check

**Files:**
- Modify: `crates/infigraph-core/src/daemon/mod.rs`
- Test: `crates/infigraph-core/tests/daemon_stale_build_self_check.rs` (new)

**Interfaces:**
- Consumes: `infigraph print-build-hash` (Task 1) via `std::env::current_exe()` — no library-level dependency, just the CLI binary being on disk at test/run time.
- Produces: no new public function — the check is inline in `run_write_coordinator`'s existing loop.

- [ ] **Step 1: Write the failing test**

First, re-read `crates/infigraph-core/src/daemon/mod.rs`'s `run_write_coordinator` in full (verify the exact current loop structure and the `!root.exists()` precedent's exact wording/location — this plan's research may have drifted) and `crates/infigraph-core/tests/watch_daemon.rs` for the existing pattern real tests use to spawn a coordinator loop on a background thread and observe its shutdown (several tests there already call `run_write_coordinator` directly).

Create `crates/infigraph-core/tests/daemon_stale_build_self_check.rs`:

```rust
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Uses a real `RawWatch`-independent tempdir project (no code needs to
/// actually parse) with a fake `.infigraph/watch.lock` home, since the
/// coordinator loop only needs `root` to exist and be watchable -- it
/// doesn't need a real indexed graph for this test.
#[test]
fn coordinator_self_exits_when_build_hash_check_detects_a_mismatch() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project_dir.path().join(".infigraph")).unwrap();

    let override_dir = tempfile::tempdir().unwrap();
    let override_path = override_dir.path().join("build-hash.txt");
    // Start with the REAL build hash so the daemon doesn't self-exit
    // immediately -- the test flips this file's contents after the daemon
    // is up and running, to prove the *next* check picks up the change.
    std::fs::write(&override_path, infigraph_core::build_hash()).unwrap();

    std::env::set_var(
        "INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE",
        &override_path,
    );
    std::env::set_var("INFIGRAPH_TEST_BUILD_HASH_CHECK_SECS", "1");

    let (_stop_tx, stop_rx) = mpsc::channel();
    let token = CancellationToken::new();
    let root = project_dir.path().to_path_buf();

    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            infigraph_languages::bundled_registry,
            50,
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            false,
            None,
            &token,
            None,
        )
    });

    // Let the daemon complete at least one "everything matches" check
    // cycle first (proves this isn't just "it happened to exit anyway").
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!handle.is_finished(), "coordinator exited before the mismatch was introduced");

    // Now introduce the mismatch: the daemon's OWN in-process build_hash()
    // stays the real one, but the next print-build-hash subprocess it
    // spawns will read this file and report something different.
    std::fs::write(&override_path, "totally-different-fake-hash").unwrap();

    // Wait past at least one more check interval.
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        handle.is_finished(),
        "coordinator should have self-exited after detecting the build-hash mismatch"
    );
    handle.join().unwrap().expect("coordinator loop returned an error instead of a clean shutdown");

    std::env::remove_var("INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE");
    std::env::remove_var("INFIGRAPH_TEST_BUILD_HASH_CHECK_SECS");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test daemon_stale_build_self_check`
Expected: FAIL — `handle.is_finished()` is false after both sleeps; the coordinator has no self-check yet so it never exits on its own (the test will hang until its own thread naturally times out under `cargo test`'s default per-test behavior, or fail the second assertion — either way, it doesn't pass).

- [ ] **Step 3: Write minimal implementation**

In `crates/infigraph-core/src/daemon/mod.rs`, add a helper function near the top of the file (after `COORDINATOR_TICK`'s declaration):

```rust
/// How often the coordinator loop re-checks whether the on-disk binary has
/// changed since this process started. Deliberately independent of
/// `periodic_secs` (which can be 0 for the plain `infigraph daemon` -- see
/// its call site) -- staleness detection must run even when no other
/// periodic pass is configured.
const BUILD_HASH_CHECK_INTERVAL: Duration = Duration::from_secs(300);

fn build_hash_check_interval() -> Duration {
    std::env::var("INFIGRAPH_TEST_BUILD_HASH_CHECK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(BUILD_HASH_CHECK_INTERVAL)
}

/// Spawns a fresh `infigraph print-build-hash` subprocess and returns its
/// trimmed stdout, or `None` if the spawn failed or it exited non-zero.
/// `None` means "couldn't check this time," not "confirmed stale" -- the
/// caller must not treat a failed check as a mismatch.
fn current_on_disk_build_hash() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let output = std::process::Command::new(exe)
        .arg("print-build-hash")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
```

Then, inside `run_write_coordinator`, immediately before the `let mut shutdown_requested = false;` line that precedes the main `loop {`, add:

```rust
    let build_hash_check_interval = build_hash_check_interval();
    let mut last_build_hash_check = std::time::Instant::now();
```

Inside the `loop { ... }` body, immediately after the existing `if !root.exists() { ... break; }` block, add:

```rust
        // Self-terminate if the on-disk binary has changed since this
        // process started (#134) -- prune_stale_daemon already handles
        // this correctly for a daemon someone is actively trying to
        // (re)start, but a long-idle project's daemon never gets that
        // lazy check triggered. This rides its own coarse interval rather
        // than every COORDINATOR_TICK, since it spawns a real subprocess.
        if last_build_hash_check.elapsed() >= build_hash_check_interval {
            last_build_hash_check = std::time::Instant::now();
            match current_on_disk_build_hash() {
                Some(current) if current != crate::build_hash() => {
                    eprintln!(
                        "[watch] running build {} but the current binary on disk is {} -- \
                         shutting down so the next request starts a fresh daemon",
                        crate::build_hash(),
                        current
                    );
                    break;
                }
                Some(_) => {}
                None => {
                    eprintln!(
                        "[watch] build-hash self-check couldn't run this interval, will retry"
                    );
                }
            }
        }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test daemon_stale_build_self_check`
Expected: PASS

- [ ] **Step 5: Run the existing daemon test suite to confirm no regression**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test watch_daemon --test daemon_protocol_watcher_wiring -- --test-threads=1`
Expected: PASS (these are `run_write_coordinator`'s other direct callers/exercisers — confirms the new unconditional check doesn't break any of them; none of them set the test-override env vars, so they all exercise the real 300s-interval, real-build_hash path, which will never fire during a short test run).

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/daemon/mod.rs crates/infigraph-core/tests/daemon_stale_build_self_check.rs
git commit -m "feat(core): daemon self-exits on detecting a stale build (#134)

prune_stale_daemon already correctly detects and terminates a
stale-build daemon holder, but only when something actively tries to
(re)start a daemon against that root. A long-idle project's daemon
never got that lazy check triggered -- confirmed twice this session
(the original sittir incident, and a fresh instance on this repo
itself). The coordinator loop now periodically (every 5 minutes,
overridable for tests) spawns a fresh print-build-hash subprocess of
its own current_exe() and self-exits if it disagrees with its own
in-process build_hash(), exactly mirroring the loop's existing
!root.exists() self-termination pattern."
```

---

### Task 3: Verification and docs wrap-up

**Files:**
- Modify: `docs/DESIGN-hardening.md`

- [ ] **Step 1: Full targeted test suites, one crate at a time**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core -- --test-threads=1`
Expected: PASS

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-cli -- --test-threads=1`
Expected: PASS

- [ ] **Step 2: Format and lint**

Run: `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 3: Update the design doc**

In `docs/DESIGN-hardening.md`, under the `### Shipped` section, add a new bullet (check the exact current list first with `mcp__infigraph__search_code` for `pattern: "R2.2.4"` to confirm the surrounding style/order hasn't changed):

```
- [x] R2.2.5 — daemon self-detects a stale build ([#134](https://github.com/pradeepmouli/infigraph/issues/134)) — `crates/infigraph-core/src/daemon/mod.rs`: `run_write_coordinator`'s loop periodically spawns `infigraph print-build-hash` (a new hidden subcommand, `crates/infigraph-cli/src/main.rs`) via its own `current_exe()` and self-exits on a mismatch against its own compile-time `build_hash()`, closing the gap where `prune_stale_daemon`'s existing lazy check never fires for a long-idle project nobody is actively touching.
```

- [ ] **Step 4: Commit**

```bash
git add docs/DESIGN-hardening.md
git commit -m "docs: record #134's daemon stale-build self-check as shipped (R2.2.5)"
```
