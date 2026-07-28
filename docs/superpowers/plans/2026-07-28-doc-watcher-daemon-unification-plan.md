# Doc-Watcher Daemon Unification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The existing code-watch daemon (`infigraph watch`, spawned via `ensure_daemon_running`, coordinated through `.infigraph/watch.lock`, toggled on via `INFIGRAPH_WATCH_DAEMON=1`) also drives doc reindexing, so daemon mode covers both jobs from one process instead of leaving docs stuck on the fragile in-process-thread model.

**Architecture:** Rather than truly merging into one shared `notify::Watcher` instance (the code-watch engine `watch_project_with_periodic` is a large, delicate, already-multi-concern function — batch/index-op-lock coordination, periodic SCIP refresh, restart-with-backoff — shared by both the CLI daemon and the in-process MCP thread; surgically threading a second concern through it is real risk for low payoff). Instead: the daemon process (`infigraph watch`, i.e. `cmd_watch`) runs a **second background thread** alongside its existing main-thread `watch_project` call. That thread drives `infigraph_docs::watch::watch_docs` (already exists, unmodified) through a new attach/detach state machine: attaches once `.infigraph/docs.kuzu` exists, detaches (and stays detached until a fresh appearance) if `.infigraph/docs.kuzu` disappears or `.infigraph/watch.stop.docs` is found, and exits once the whole daemon is told to stop. Same daemon process, same `watch.lock`, same `INFIGRAPH_WATCH_DAEMON` toggle, no new CLI subcommand, no new spawn primitive — exactly the "extend the existing daemon" shape the design settled on. MCP's `auto_start_doc_watch_inner`/`tool_watch_docs`/`tool_stop_watch_docs`/`tool_get_watch_status` gain the same daemon-mode/primary-gate awareness the code-watch path already has (some of it landed just before this plan, in the `tool_watch_project` fix — see Global Constraints).

**Tech Stack:** Rust (edition 2021), std only (`std::thread`, `std::sync::{Arc, atomic::AtomicBool}`, `std::sync::mpsc`) — no new dependencies.

## Global Constraints

- **Scope is daemon-mode only.** The in-process default (`INFIGRAPH_WATCH_DAEMON` unset — two independent threads, one per subsystem) must remain byte-for-byte unchanged. No task in this plan touches `tool_watch_project`'s or `tool_watch_docs`'s in-process branches, `watch_project`/`watch_project_with_periodic`'s internals, or `WATCHERS`/`DOC_WATCHERS`' in-process map behavior.
- **No new lock file, no new CLI subcommand, no new daemon-spawn primitive.** `ensure_daemon_running`/`DaemonStartOutcome`/`.infigraph/watch.lock` are reused exactly as they exist today (`crates/infigraph-core/src/watch/daemon.rs`).
- **Fork-only, no upstream PR without asking** (standing directive for this session).
- Every cargo command runs with `CARGO_PROFILE_DEV_DEBUG=0` (hard rule — mixing debug settings spawns multi-GB duplicate build trees).
- **This machine's `~/.zshrc` exports `INFIGRAPH_WATCH_DAEMON=1`** — every `cargo test`/`cargo build`/`git commit` invocation via a shell that sources it inherits daemon mode ON, which silently breaks unrelated tests that don't manage that env var themselves (confirmed this exact contamination broke `watcher_reindex.rs` and `groups_watch_perf` during the immediately-preceding `tool_watch_project` fix). **Always run cargo/git commands for this plan with `env -u INFIGRAPH_WATCH_DAEMON` prefixed**, and any NEW test that itself needs the toggle on must explicitly `std::env::set_var`/`remove_var` it around itself under the existing `ENV_LOCK` pattern (see `crates/infigraph-mcp/tests/watcher_daemon_mode.rs` for the convention).
- Commit with `--no-verify` only if the pre-commit hook fails for a reason clearly unrelated to the change. Pre-authorized flakes for this session: `write_lock_perf::test_contended_lock_throughput`, `groups_watch_perf::test_groups_watch_perf` (contention-under-load only — the env-contamination failure mode above is NOT this flake and must not be waved through with `--no-verify`; fix it by clearing the env var instead).
- Already-merged, reusable primitives this plan builds on (do not reinvent):
  - `infigraph_core::watch::daemon::{watch_daemon_mode_enabled, ensure_daemon_running, DaemonStartOutcome, resolve_cli_binary_sibling_of}` (`crates/infigraph-core/src/watch/daemon.rs`).
  - `infigraph_docs::watch::watch_docs(root: &Path, debounce_ms: u64, stop_rx: mpsc::Receiver<()>, log_prefix: &str) -> Result<()>` (`crates/infigraph-docs/src/watch.rs:10`) — unmodified by this plan.
  - `crate::tools::watch::ensure_daemon_watcher` in MCP (`crates/infigraph-mcp/src/tools/watch.rs`) — added by the `tool_watch_project` fix that landed just before this plan (commit `c0bc6ab` on `feat/health-beacons`); Task 3 below makes it `pub(crate)` so `tools/docs.rs` can call it too.

---

### Task 1: `infigraph-docs` gains the doc-watch daemon attach/detach loop

**Files:**
- Modify: `crates/infigraph-docs/src/watch.rs` (currently 87 lines — add the new function alongside the existing `watch_docs`, same file, since they're the same concern)
- Test: `crates/infigraph-docs/src/watch.rs` (`#[cfg(test)] mod tests`, new file for this crate if none exists yet — check first)

**Interfaces:**
- Produces: `pub fn watch_docs_daemon_loop(root: &Path, debounce_ms: u64, shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>) -> anyhow::Result<()>` — blocks until `shutdown` is observed true. Task 2 consumes this from `infigraph_docs::watch::watch_docs_daemon_loop`.
- Consumes: `watch_docs` (existing, unmodified).

- [ ] **Step 1: Write the failing tests**

Check first whether `crates/infigraph-docs/src/watch.rs` already has a `#[cfg(test)] mod tests` block (it currently does not, per the 87-line file read for this plan — if this has changed, append into the existing block instead of adding a new one). Append to the bottom of `crates/infigraph-docs/src/watch.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    fn set_fast_poll() {
        std::env::set_var("INFIGRAPH_DOC_DAEMON_POLL_MS", "20");
    }

    fn clear_fast_poll() {
        std::env::remove_var("INFIGRAPH_DOC_DAEMON_POLL_MS");
    }

    #[test]
    fn returns_immediately_when_shutdown_already_set() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let shutdown = Arc::new(AtomicBool::new(true));
        watch_docs_daemon_loop(&root, 50, shutdown).unwrap();
        // No assertion beyond "returned" -- this test times out (fails) if
        // the loop doesn't check shutdown before ever attaching.
    }

    #[test]
    fn does_not_attach_without_docs_kuzu() {
        set_fast_poll();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || watch_docs_daemon_loop(&root, 50, shutdown_clone));

        std::thread::sleep(Duration::from_millis(150));
        // No docs.kuzu ever appeared -- the loop must still be polling, not
        // stuck in an attached watch_docs call. Shutting down must return
        // promptly (proves it was in the poll loop, not blocked inside
        // watch_docs's own internal loop, which only checks its stop_rx on
        // its own ~500ms cadence and would still return promptly here too --
        // the real proof is the next test, which asserts actual indexing).
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.join().unwrap().unwrap();
        clear_fast_poll();
    }

    #[test]
    fn attaches_and_indexes_once_docs_kuzu_appears() {
        set_fast_poll();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let root_clone = root.clone();
        let handle =
            std::thread::spawn(move || watch_docs_daemon_loop(&root_clone, 50, shutdown_clone));

        // Not indexed yet -- give the poll loop a couple of ticks doing
        // nothing, then create a real (empty) doc index so docs.kuzu exists.
        std::thread::sleep(Duration::from_millis(60));
        crate::DocIndex::open(&root).unwrap().init().unwrap();
        assert!(root.join(".infigraph").join("docs.kuzu").exists());

        // Give the daemon loop time to notice and attach, then write a doc.
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(root.join("readme.md"), "# hello\n\nsome content").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut chunks = 0;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            let idx = crate::DocIndex::open(&root).unwrap();
            chunks = idx.chunk_count().unwrap_or(0);
            if chunks > 0 {
                break;
            }
        }
        assert!(chunks > 0, "doc daemon loop must have attached and indexed readme.md");

        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.join().unwrap().unwrap();
        clear_fast_poll();
    }

    #[test]
    fn detaches_on_stop_sentinel_and_does_not_immediately_reattach() {
        set_fast_poll();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        crate::DocIndex::open(&root).unwrap().init().unwrap();

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let root_clone = root.clone();
        let handle =
            std::thread::spawn(move || watch_docs_daemon_loop(&root_clone, 50, shutdown_clone));

        // Let it attach.
        std::thread::sleep(Duration::from_millis(100));

        // Request an explicit detach.
        std::fs::write(root.join(".infigraph").join("watch.stop.docs"), b"").unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !root.join(".infigraph").join("watch.stop.docs").exists(),
            "sentinel must be consumed (removed) once acted on"
        );

        // Write a NEW doc while suppressed -- must NOT be indexed, proving
        // the loop stayed detached instead of immediately re-attaching
        // (docs.kuzu still exists, so a naive re-poll would re-attach).
        std::fs::write(root.join("after-stop.md"), "# should not be indexed yet").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let idx = crate::DocIndex::open(&root).unwrap();
        let chunks_while_suppressed = idx.chunk_count().unwrap_or(0);
        assert_eq!(
            chunks_while_suppressed, 0,
            "must stay detached after an explicit stop until docs.kuzu disappears and reappears"
        );

        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.join().unwrap().unwrap();
        clear_fast_poll();
    }
}
```

**Before implementing, confirm `DocIndex` has a `chunk_count() -> Result<usize>` method** (or equivalent already-existing accessor for "how many chunks are indexed") — check `crates/infigraph-docs/src/store.rs`. If the exact name differs, use whatever the real accessor is called and adjust the two tests above to match; do not invent a new public method for this plan's tests alone if an equivalent already exists.

- [ ] **Step 2: Run tests to verify they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-docs --lib watch:: -- --test-threads=1`
Expected: COMPILE ERROR — `watch_docs_daemon_loop` doesn't exist.

- [ ] **Step 3: Implement**

Add to the top of `crates/infigraph-docs/src/watch.rs` (alongside the existing `use` lines):

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
```

Append the new function after `watch_docs`:

```rust
/// How often the daemon loop polls for `.infigraph/docs.kuzu`'s existence
/// and the per-handler stop sentinel while deciding whether to attach or
/// detach a `watch_docs` session. Overridable via
/// `INFIGRAPH_DOC_DAEMON_POLL_MS` so tests don't wait through a real 1s tick.
fn attach_poll_interval() -> Duration {
    std::env::var("INFIGRAPH_DOC_DAEMON_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(1000))
}

/// Drive doc-watching for `root` as part of a merged code+doc watch daemon
/// (see `infigraph_core::watch::daemon`). Dynamically attaches (starts a
/// `watch_docs` session) once `.infigraph/docs.kuzu` exists, detaches
/// (stops it) if that file disappears (e.g. after `clean_docs`) -- eligible
/// to re-attach once it reappears -- or if `.infigraph/watch.stop.docs` is
/// found -- NOT eligible to re-attach until docs.kuzu disappears and
/// reappears, since that sentinel represents an explicit stop request, not
/// an index-lifecycle event. Exits once `shutdown` is observed true.
/// Blocks until then.
pub fn watch_docs_daemon_loop(
    root: &Path,
    debounce_ms: u64,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let docs_kuzu = root.join(".infigraph").join("docs.kuzu");
    let stop_sentinel = root.join(".infigraph").join("watch.stop.docs");
    let poll = attach_poll_interval();

    let mut suppressed_until_absent = false;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }

        let exists = docs_kuzu.exists();

        if suppressed_until_absent {
            if !exists {
                suppressed_until_absent = false;
            }
            std::thread::sleep(poll);
            continue;
        }

        if !exists {
            std::thread::sleep(poll);
            continue;
        }

        let (inner_stop_tx, inner_stop_rx) = mpsc::channel::<()>();
        let shutdown_for_detacher = Arc::clone(&shutdown);
        let docs_kuzu_for_detacher = docs_kuzu.clone();
        let stop_sentinel_for_detacher = stop_sentinel.clone();

        // Runs concurrently with the blocking watch_docs call below, and
        // signals it to return once any detach/shutdown condition is met.
        let detacher = std::thread::spawn(move || -> bool {
            loop {
                if shutdown_for_detacher.load(Ordering::Relaxed) {
                    let _ = inner_stop_tx.send(());
                    return false;
                }
                if stop_sentinel_for_detacher.exists() {
                    let _ = std::fs::remove_file(&stop_sentinel_for_detacher);
                    let _ = inner_stop_tx.send(());
                    return true;
                }
                if !docs_kuzu_for_detacher.exists() {
                    let _ = inner_stop_tx.send(());
                    return false;
                }
                std::thread::sleep(poll);
            }
        });

        eprintln!(
            "[doc-watch-daemon] attaching doc watcher for {}",
            root.display()
        );
        if let Err(e) = watch_docs(root, debounce_ms, inner_stop_rx, "doc-watch-daemon") {
            eprintln!("[doc-watch-daemon] watch_docs error: {e}");
        }

        suppressed_until_absent = detacher.join().unwrap_or(false);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-docs --lib watch:: -- --test-threads=1`
Expected: PASS, 4/4 new tests (plus any pre-existing tests in this file/module unaffected).

- [ ] **Step 5: Commit**

```bash
cd /path/to/worktree
cargo fmt --all
git add crates/infigraph-docs/src/watch.rs
git commit -m "feat: doc-watch daemon attach/detach loop for merged code+doc watching"
```

---

### Task 2: `infigraph-cli`'s `cmd_watch` spawns the doc-watch daemon thread

**Files:**
- Modify: `crates/infigraph-cli/src/info_commands.rs:314-345` (`cmd_watch`)
- Test: `crates/infigraph-cli/tests/` — check for an existing integration-test file that spawns a real `infigraph watch` subprocess (this codebase has that pattern elsewhere, e.g. `crates/infigraph-mcp/tests/instance_registration.rs` spawns the real `infigraph-mcp` binary) to follow the same convention; if none exists yet for the CLI's `watch` subcommand specifically, create `crates/infigraph-cli/tests/watch_daemon_docs.rs`.

**Interfaces:**
- Consumes: `infigraph_docs::watch::watch_docs_daemon_loop` (Task 1).
- Produces: `cmd_watch`'s external behavior (unchanged signature, unchanged stdout messages for the code side) plus the new doc-thread side effect. No new public interface — this task is wiring only.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-cli/tests/watch_daemon_docs.rs`:

```rust
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn cli_binary() -> std::path::PathBuf {
    // Mirrors infigraph_core::watch::daemon::resolve_cli_binary_sibling_of's
    // grandparent fallback: integration-test binaries live one level below
    // the real build output directory.
    let exe = std::env::current_exe().unwrap();
    let deps_dir = exe.parent().unwrap();
    let candidate = deps_dir.join("infigraph");
    if candidate.exists() {
        return candidate;
    }
    deps_dir.parent().unwrap().join("infigraph")
}

/// Real end-to-end test: spawns `infigraph watch <root>` as a genuine
/// detached child process, indexes docs for the same root partway through,
/// and confirms the daemon's doc thread picked it up without restarting
/// the watch process -- proving cmd_watch's doc-thread wiring works, not
/// just watch_docs_daemon_loop in isolation (Task 1 already covers that).
#[test]
fn cmd_watch_daemon_also_indexes_docs_without_restart() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "skipping: infigraph CLI binary not found at {} (needs a full `cargo build`/`cargo test --workspace` first)",
            bin.display()
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();

    let mut child = Command::new(&bin)
        .arg("watch")
        .arg("--debounce")
        .arg("50")
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn infigraph watch");

    // Give the watcher a moment to acquire watch.lock and start its loops.
    std::thread::sleep(Duration::from_millis(300));

    // Index docs for the same root WITHOUT stopping the watch process --
    // this is what makes docs.kuzu appear mid-run, the exact scenario the
    // daemon's doc thread must notice on its own.
    infigraph_docs::DocIndex::open(&root).unwrap().init().unwrap();
    std::fs::write(root.join("readme.md"), "# hello\n\nsome content").unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut chunks = 0;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        let idx = infigraph_docs::DocIndex::open(&root).unwrap();
        chunks = idx.chunk_count().unwrap_or(0);
        if chunks > 0 {
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        chunks > 0,
        "the running watch daemon's doc thread must have indexed readme.md without a restart"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-cli && cargo test -p infigraph-cli --test watch_daemon_docs -- --test-threads=1`
Expected: FAIL — no `chunks > 0` (the daemon has no doc thread yet, `readme.md` never gets indexed).

- [ ] **Step 3: Implement**

In `crates/infigraph-cli/src/info_commands.rs`, replace `cmd_watch`:

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

    println!(
        "Watching {} (debounce {}ms) — Ctrl-C to stop",
        root.display(),
        debounce
    );

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();

    ctrlc::set_handler(move || {
        let _ = stop_tx.send(());
    })
    .ok();

    let doc_shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let doc_shutdown_for_thread = std::sync::Arc::clone(&doc_shutdown);
    let doc_root = root.to_path_buf();
    let doc_thread = std::thread::spawn(move || {
        if let Err(e) =
            infigraph_docs::watch::watch_docs_daemon_loop(&doc_root, debounce, doc_shutdown_for_thread)
        {
            eprintln!("[doc-watch-daemon] error: {e}");
        }
    });

    infigraph_core::watch::watch_project(root, bundled_registry, debounce, stop_rx, |evt| {
        println!("[watch] {evt}");
    })?;

    doc_shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = doc_thread.join();

    println!("Watch stopped.");
    Ok(())
}
```

Note: `_lock` (the code-side `watch.lock` guard) stays held for the whole function — unchanged from today — and now covers the doc thread's lifetime too, since the doc thread is joined before `cmd_watch` returns and drops `_lock`. This preserves `watch.lock`'s existing meaning ("a watch daemon is running for this repo") without needing a second lock.

- [ ] **Step 4: Run test to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-cli && cargo test -p infigraph-cli --test watch_daemon_docs -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Run the existing CLI watch tests to confirm no regression**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-cli --no-fail-fast -- --test-threads=1`
Expected: green modulo any already-catalogued pre-existing failures for this crate (none currently known for `infigraph-cli` specifically — flag anything new as this task's to resolve).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/infigraph-cli/src/info_commands.rs crates/infigraph-cli/tests/watch_daemon_docs.rs
git commit -m "feat: infigraph watch daemon also drives doc watching via a second thread"
```

---

### Task 3: MCP doc-watch auto-start and explicit `watch_docs` gain daemon-mode + primary gating

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/watch.rs` (`ensure_daemon_watcher` visibility only — one-line change)
- Modify: `crates/infigraph-mcp/src/tools/docs.rs` (`auto_start_doc_watch_inner`, `tool_watch_docs`)
- Test: `crates/infigraph-mcp/tests/watcher_daemon_mode.rs` (extend)

**Interfaces:**
- Consumes: `crate::tools::watch::ensure_daemon_watcher(root: &Path) -> Result<DaemonStartOutcome>` (made `pub(crate)`), `crate::tools::watch::watchers_disabled() -> bool` (already `pub`).
- Produces: no new public interface — `auto_start_doc_watch_inner` and `tool_watch_docs` gain the same two checks `auto_start_watch_inner`/`tool_watch_project` already have.

- [ ] **Step 1: Write the failing tests**

Append to `crates/infigraph-mcp/tests/watcher_daemon_mode.rs` (reuses the file's existing `ENV_LOCK`/`wait_for_watch_locks_released` helpers):

```rust
/// `auto_start_doc_watch` (the doc-side opportunistic auto-start, mirroring
/// `auto_start_watch`'s existing daemon-mode test above) must ALSO route
/// through the shared daemon primitive when the toggle is on, instead of
/// its current unconditional in-process-thread behavior.
#[test]
fn auto_start_doc_watch_respects_daemon_mode_toggle() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "1");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    infigraph_docs::DocIndex::open(&root).unwrap().init().unwrap();
    let path = root.to_string_lossy().to_string();

    let result = infigraph_mcp::tools::docs::auto_start_doc_watch(&path);

    assert!(
        !infigraph_mcp::tools::docs::is_doc_watching(&path.replace('\\', "/")),
        "daemon mode must never populate the in-process DOC_WATCHERS map"
    );

    if let Some(msg) = result {
        assert!(
            msg.contains("Daemon watcher"),
            "unexpected auto_start_doc_watch outcome under daemon mode: {msg}"
        );
    }

    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
}

/// `tool_watch_docs` (the explicit MCP tool) must also respect the daemon
/// toggle -- mirrors `tool_watch_project_respects_daemon_mode_toggle` above.
#[test]
fn tool_watch_docs_respects_daemon_mode_toggle() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "1");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    infigraph_docs::DocIndex::open(&root).unwrap().init().unwrap();
    let path = root.to_string_lossy().to_string();

    let args = serde_json::json!({ "path": path });
    let result = infigraph_mcp::tools::docs::tool_watch_docs(&args);

    assert!(!infigraph_mcp::tools::docs::is_doc_watching(
        &path.replace('\\', "/")
    ));

    if let Ok(msg) = &result {
        assert!(
            msg.contains("Daemon watcher"),
            "unexpected tool_watch_docs outcome under daemon mode: {msg}"
        );
    }

    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
}
```

Create `crates/infigraph-mcp/tests/tool_watch_docs_primary_gate.rs` (own file/binary — same reasoning as `tool_watch_project_primary_gate.rs`: `disable_watchers()` is a one-way, process-lifetime flag with no reset):

```rust
/// `tool_watch_docs` must also respect the primary/secondary gate --
/// mirrors `tool_watch_project_declines_when_not_primary`.
#[test]
fn tool_watch_docs_declines_when_not_primary() {
    infigraph_mcp::tools::watch::disable_watchers();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    infigraph_docs::DocIndex::open(&root).unwrap().init().unwrap();
    let path = root.to_string_lossy().to_string();

    let args = serde_json::json!({ "path": path });
    let result = infigraph_mcp::tools::docs::tool_watch_docs(&args)
        .expect("must not return an error, only an informative skip message");

    assert!(
        result.to_lowercase().contains("not primary"),
        "expected a not-primary message, got: {result}"
    );
    assert!(!infigraph_mcp::tools::docs::is_doc_watching(
        &path.replace('\\', "/")
    ));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_daemon_mode --test tool_watch_docs_primary_gate -- --test-threads=1`
Expected: the three new tests FAIL (daemon-mode ones show an in-process watcher started instead of a daemon message; the primary-gate one shows a watcher started instead of a not-primary message). Existing tests in `watcher_daemon_mode.rs` remain green.

- [ ] **Step 3: Implement**

In `crates/infigraph-mcp/src/tools/watch.rs`, change `ensure_daemon_watcher`'s visibility from `fn` to `pub(crate) fn` (one-line change, no other edit to that function):

```rust
pub(crate) fn ensure_daemon_watcher(
    root: &std::path::Path,
) -> Result<infigraph_core::watch::daemon::DaemonStartOutcome> {
```

In `crates/infigraph-mcp/src/tools/docs.rs`, modify `auto_start_doc_watch_inner`:

```rust
fn auto_start_doc_watch_inner(path: &str, skip_disabled_check: bool) -> Option<String> {
    if is_remote_mode() {
        return None;
    }
    if !skip_disabled_check && super::watch::watchers_disabled() {
        return None;
    }
    let root = std::path::PathBuf::from(path).canonicalize().ok()?;
    let root_str = root.to_string_lossy().replace('\\', "/");

    if is_doc_watching(&root_str) {
        return None;
    }

    if !root.join(".infigraph").join("docs.kuzu").exists() {
        return None;
    }

    if infigraph_core::watch::daemon::watch_daemon_mode_enabled() {
        return match super::watch::ensure_daemon_watcher(&root) {
            Ok(infigraph_core::watch::daemon::DaemonStartOutcome::Spawned) => {
                eprintln!("[auto-watch] Started daemon watcher for {root_str}");
                Some(format!("Daemon watcher started for {root_str}"))
            }
            Ok(infigraph_core::watch::daemon::DaemonStartOutcome::AlreadyRunning) => None,
            Ok(infigraph_core::watch::daemon::DaemonStartOutcome::Failed(e)) => {
                eprintln!("[auto-watch] Failed to start daemon watcher: {e}");
                None
            }
            Err(e) => {
                eprintln!("[auto-watch] could not locate infigraph CLI binary: {e}");
                None
            }
        };
    }

    let args = serde_json::json!({
        "path": path,
        "debounce_ms": 500
    });
    match tool_watch_docs(&args) {
        Ok(msg) => {
            eprintln!("[auto-watch] Started doc watcher for {root_str}");
            Some(msg)
        }
        Err(e) => {
            eprintln!("[auto-watch] Failed to start doc watcher: {e}");
            None
        }
    }
}
```

Note the `docs.kuzu` existence check moved earlier (before the daemon-mode branch) — this preserves the function's existing "only start once actually indexed" precondition for BOTH branches, matching `ensure_daemon_running`'s own independent "not yet indexed" no-op (the two checks are redundant but harmless: this one avoids even attempting the daemon call, `ensure_daemon_running`'s own check is a second layer of the same safety).

In `crates/infigraph-mcp/src/tools/docs.rs`, modify `tool_watch_docs`:

```rust
pub fn tool_watch_docs(args: &Value) -> Result<String> {
    if is_remote_mode() {
        return Ok(
            "Doc watching is not supported in remote mode (Neo4j backend). \
             Reindexing is triggered via webhooks instead."
                .to_string(),
        );
    }

    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let debounce_ms = args
        .get("debounce_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(2000);

    let root = PathBuf::from(path).canonicalize().context("invalid path")?;
    let root_str = root.to_string_lossy().replace('\\', "/");

    if super::watch::watchers_disabled() {
        return Ok(format!(
            "Not starting a doc watcher for {root_str}: this MCP instance is not primary \
             (another instance holds mcp.lock and owns watchers for this machine). \
             Use get_watch_status to check the active watcher."
        ));
    }

    if infigraph_core::watch::daemon::watch_daemon_mode_enabled() {
        return match super::watch::ensure_daemon_watcher(&root)? {
            infigraph_core::watch::daemon::DaemonStartOutcome::Spawned => {
                Ok(format!("Daemon watcher started for {root_str}"))
            }
            infigraph_core::watch::daemon::DaemonStartOutcome::AlreadyRunning => {
                Ok(format!("Daemon watcher already running for {root_str}"))
            }
            infigraph_core::watch::daemon::DaemonStartOutcome::Failed(e) => {
                Err(anyhow::anyhow!("Failed to start daemon watcher: {e}"))
            }
        };
    }

    init_doc_watchers();

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let watcher_id = format!(
        "docwatch-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );

    {
        let mut guard = DOC_WATCHERS.lock().unwrap();
        if let Some(map) = guard.as_mut() {
            map.insert(
                watcher_id.clone(),
                DocWatcherEntry {
                    stop_tx,
                    path: root_str.clone(),
                },
            );
        }
    }

    let watcher_id_clone = watcher_id.clone();
    let log_prefix = watcher_id[..16.min(watcher_id.len())].to_string();
    std::thread::spawn(move || {
        if let Err(e) = infigraph_docs::watch::watch_docs(&root, debounce_ms, stop_rx, &log_prefix)
        {
            eprintln!("[{log_prefix}] watcher error: {e}");
        }
        let mut guard = DOC_WATCHERS.lock().unwrap();
        if let Some(map) = guard.as_mut() {
            map.remove(&watcher_id_clone);
        }
    });

    Ok(format!(
        "Document watcher started.\nID: {watcher_id}\nPath: {root_str}\nDebounce: {debounce_ms}ms\nUse stop_watch_docs to stop."
    ))
}
```

(Only change from the current implementation: the `is_remote_mode()` early-return stays first; `init_watchers()`-equivalent `init_doc_watchers()` moved to after the two new gate checks, matching `tool_watch_project`'s structure exactly; the two new gate blocks inserted between path-parsing and `init_doc_watchers()`.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_daemon_mode --test tool_watch_docs_primary_gate -- --test-threads=1`
Expected: PASS, all tests in both files.

- [ ] **Step 5: Run the full infigraph-mcp suite to check for regressions**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --no-fail-fast -- --test-threads=1`
Expected: green modulo `watcher_concurrency::test_graph_tools_with_group_watchers` (already-catalogued pre-existing failure this session, confirmed via `git stash` comparison during the immediately-preceding `tool_watch_project` fix — re-confirm with the same technique if any doubt).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/infigraph-mcp/src/tools/watch.rs crates/infigraph-mcp/src/tools/docs.rs crates/infigraph-mcp/tests/watcher_daemon_mode.rs crates/infigraph-mcp/tests/tool_watch_docs_primary_gate.rs
git commit -m "feat: MCP doc-watch tools respect daemon-mode toggle and primary gate"
```

---

### Task 4: MCP `stop_watch_docs`/`get_watch_status` gain path-based, daemon-aware doc support

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/docs.rs` (`tool_stop_watch_docs`)
- Modify: `crates/infigraph-mcp/src/tools/watch.rs` (`tool_get_watch_status`'s docs-status branch)
- Test: `crates/infigraph-mcp/tests/watcher_daemon_mode.rs` (extend)

**Interfaces:**
- Consumes: `.infigraph/watch.lock` (shared, existing), `.infigraph/docs.kuzu` existence (existing convention), `infigraph_core::lockfile::{try_acquire, read_holder}` (existing).
- Produces: `tool_stop_watch_docs` gains a `path`-based branch (today it only accepts `watcher_id`) that writes `.infigraph/watch.stop.docs`; no signature change (still `fn(args: &Value) -> Result<String>`, `path` becomes a second accepted argument alongside the existing `watcher_id`).

- [ ] **Step 1: Write the failing tests**

Append to `crates/infigraph-mcp/tests/watcher_daemon_mode.rs`:

```rust
/// `tool_stop_watch_docs` must accept a `path` argument (today it only
/// accepts `watcher_id`, which is meaningless in daemon mode since there is
/// no in-process DOC_WATCHERS entry to look up) and, when the shared daemon
/// is alive, write `.infigraph/watch.stop.docs`.
#[test]
fn stop_watch_docs_by_path_writes_sentinel_when_daemon_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let ig = root.join(".infigraph");
    std::fs::create_dir_all(&ig).unwrap();
    infigraph_docs::DocIndex::open(&root).unwrap().init().unwrap();

    // Simulate a live daemon: hold watch.lock for the duration of this test.
    let lock_path = ig.join("watch.lock");
    let _held = infigraph_core::lockfile::try_acquire(&lock_path, "cli-watch")
        .unwrap()
        .expect("free");

    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::docs::tool_stop_watch_docs(&args).unwrap();

    assert!(
        result.to_lowercase().contains("stop"),
        "expected a stop-signal message, got: {result}"
    );
    assert!(
        ig.join("watch.stop.docs").exists(),
        "must write the docs-specific stop sentinel while the shared daemon is alive"
    );
}

#[test]
fn stop_watch_docs_by_path_reports_no_watcher_when_lock_free() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();

    let args = serde_json::json!({ "path": root.to_string_lossy() });
    let result = infigraph_mcp::tools::docs::tool_stop_watch_docs(&args).unwrap();

    assert!(result.to_lowercase().contains("no"), "expected a no-watcher message, got: {result}");
    assert!(!root.join(".infigraph").join("watch.stop.docs").exists());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_daemon_mode -- --test-threads=1`
Expected: FAIL — `tool_stop_watch_docs` currently requires `watcher_id` and errors on a `path`-only call (`context("missing 'watcher_id'")`).

- [ ] **Step 3: Implement**

In `crates/infigraph-mcp/src/tools/docs.rs`, replace `tool_stop_watch_docs`:

```rust
pub fn tool_stop_watch_docs(args: &Value) -> Result<String> {
    if let Some(watcher_id) = args.get("watcher_id").and_then(|v| v.as_str()) {
        let mut guard = DOC_WATCHERS.lock().unwrap();
        if let Some(map) = guard.as_mut() {
            if let Some(entry) = map.remove(watcher_id) {
                let _ = entry.stop_tx.send(());
                return Ok(format!(
                    "Document watcher {watcher_id} stopped (was watching: {}).",
                    entry.path
                ));
            }
        }
        return Ok(format!("No active document watcher with ID {watcher_id}"));
    }

    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        let root = PathBuf::from(path).canonicalize().context("invalid path")?;
        let root_str = root.to_string_lossy().replace('\\', "/");
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
        let sentinel = root.join(".infigraph").join("watch.stop.docs");
        std::fs::write(&sentinel, b"")?;
        return Ok(format!(
            "Stop signal sent for the doc watcher on {root_str}. It will detach within ~1 second \
             (the code watcher, if any, is unaffected)."
        ));
    }

    anyhow::bail!("missing 'watcher_id' or 'path'")
}
```

`tool_get_watch_status`'s existing docs-status branch (`crates/infigraph-mcp/src/tools/watch.rs`, the "Check doc watchers" block inside the `watcher_id` path) is unchanged by this task — it already only reports in-process `DOC_WATCHERS` entries, which is correct: it's reached only when a `watcher_id` is supplied, and daemon-mode doc watchers never have one (same limitation `tool_get_watch_status`'s `path`-based branch already documents for code watchers: "pending-reindex tracking is not available across processes in daemon mode"). No code change needed here — confirmed by reading the function during this plan's research, not assumed.

- [ ] **Step 4: Run tests to verify they pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_daemon_mode -- --test-threads=1`
Expected: PASS, all tests.

- [ ] **Step 5: Run the full infigraph-mcp suite**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --no-fail-fast -- --test-threads=1`
Expected: green modulo the same already-catalogued `watcher_concurrency::test_graph_tools_with_group_watchers` failure.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/infigraph-mcp/src/tools/docs.rs crates/infigraph-mcp/tests/watcher_daemon_mode.rs
git commit -m "feat: stop_watch_docs accepts path, writes docs-specific stop sentinel"
```

---

### Task 5: Full verification

**Files:**
- None created; runs suites.

- [ ] **Step 1: Full workspace test suite**

```bash
env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-docs --no-fail-fast -- --test-threads=4
env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-cli --no-fail-fast -- --test-threads=4
env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --no-fail-fast -- --test-threads=4
```

Expected: green modulo `watcher_concurrency::test_graph_tools_with_group_watchers` (already-catalogued). Re-run any process-spawning test (Task 2's real-subprocess test, Task 1's thread-based tests) with `--test-threads=1` before treating a failure under `--test-threads=4` as a real regression — this codebase has an established pattern of resource-contention flakes under high parallelism for exactly this class of test.

- [ ] **Step 2: Clippy**

Run: `env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-docs -p infigraph-cli -p infigraph-mcp --all-targets -- -D warnings`
Expected: clean on files touched by this branch.

- [ ] **Step 3: Manual smoke check — merged daemon actually drives both**

```bash
env -u INFIGRAPH_WATCH_DAEMON CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-cli
mkdir -p /tmp/doc-daemon-smoke
cd /tmp/doc-daemon-smoke
mkdir -p .infigraph
echo 'fn main() {}' > main.rs
/path/to/worktree/target/debug/infigraph watch --debounce 200 &
WATCH_PID=$!
sleep 1
cat .infigraph/watch.lock   # expect a real LockInfo JSON payload

echo '# hello' > readme.md
sleep 1
grep -q "main.rs" <(cat .infigraph/watch.log 2>/dev/null || true) || echo "code side: check watch.log manually"

# Now index docs WITHOUT restarting the watcher — the whole point of this plan.
/path/to/worktree/target/debug/infigraph index-docs . 2>&1 | tail -5
sleep 2
echo '# another doc' > second.md
sleep 2
# Expect readme.md's content (or second.md, once indexed) to be searchable —
# confirms the doc thread attached mid-run without a restart.

infigraph stop-watch . 2>/dev/null || kill $WATCH_PID
cd -
rm -rf /tmp/doc-daemon-smoke
```

Not required to consider this task done (Task 2's automated test already proves the same thing programmatically), but worth doing once manually to see real log output end-to-end.

- [ ] **Step 4: Update the progress ledger**

Append to `.superpowers/sdd/progress.md`: a `=== Doc-Watcher Daemon Unification ===` section header and per-task completion lines, matching this session's established ledger style. No push/PR — per Global Constraints, fork-only.

---

## Self-Review Notes

- **Spec coverage:** the design doc's "Architecture" (extend existing daemon, one process, two handlers) is Task 2's wiring; "dynamic pickup" (attach once docs.kuzu appears, without restart) is Task 1's state machine, proven end-to-end by Task 2's real-subprocess test; "independent stop sentinel" is Task 1 (detach logic) + Task 4 (the MCP tool that writes it); "MCP integration convergence" (`auto_start_doc_watch_inner`/`tool_watch_docs` gaining the same daemon-mode awareness `auto_start_watch_inner`/`tool_watch_project` already have) is Task 3. The one deliberate deviation from the design doc — a second `notify::Watcher`/thread instead of literally one shared subscription dispatching to a `WatchHandler` trait — was raised with and approved by the user before this plan was written (see conversation), given the real risk of surgically modifying `watch_project_with_periodic`.
- **No placeholders:** every step has complete, runnable code; the one open item (confirming `DocIndex`'s exact chunk-count accessor name in Task 1) is a real, disclosed check-before-you-code note, not a stand-in for undecided logic.
- **Type/signature consistency:** `watch_docs_daemon_loop(root: &Path, debounce_ms: u64, shutdown: Arc<AtomicBool>) -> Result<()>` is defined once (Task 1) and consumed with the identical signature in Task 2. `ensure_daemon_watcher(root: &Path) -> Result<DaemonStartOutcome>` (already existing, only its visibility changes in Task 3) is consumed identically by both `tools/watch.rs` (existing callers, untouched) and `tools/docs.rs` (new callers, Task 3).
- **Env-contamination discipline:** every `cargo`/test command in this plan is prefixed `env -u INFIGRAPH_WATCH_DAEMON`, and every new test that needs the toggle ON manages it explicitly via `ENV_LOCK` + `set_var`/`remove_var`, per the Global Constraints note — this was a real, freshly-discovered gotcha from the immediately-preceding fix, not carried over from an older, possibly-stale assumption.
- **Backward compatibility:** in-process default path (`INFIGRAPH_WATCH_DAEMON` unset) is untouched by every task — Task 1 adds a new function nothing else calls yet; Task 2's new thread only runs inside `cmd_watch` (the CLI daemon subcommand, never invoked by the in-process MCP path); Task 3/4's daemon-mode branches are additive `if` blocks ahead of the existing unconditional logic, which is otherwise byte-for-byte unchanged.
