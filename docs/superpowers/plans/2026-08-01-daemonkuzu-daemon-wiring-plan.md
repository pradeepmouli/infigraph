# DaemonKuzu Daemon Wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the `infigraph daemon` process (renamed from `infigraph watch`) to actually serve file-dropped write requests, and implement `BackendKind::DaemonKuzu` so any local-mode process can route writes through it instead of opening its own embedded Kuzu connection.

**Architecture:** The daemon's existing watcher loop (`watch_project_with_periodic`) gains a `serve_requests` mode that polls `.infigraph/requests/` on its existing cadence and serves requests against its own long-held connection. Four write paths that currently issue raw Cypher directly (bypassing any trait method) are promoted to named `GraphBackend` trait methods so a single wrapper backend can intercept every covered write uniformly; the wrapper's read path uses a real, database-enforced *read-only* connection, so any write that reaches it uncovered fails loudly instead of silently colliding or silently no-op'ing.

**Tech Stack:** Rust, `serde`/`serde_json` (existing), `arrow`/`arrow-ipc` (existing, v58.3, used only for two bulk sibling-file payloads), no new dependencies.

## Global Constraints

- Never set `CARGO_TARGET_DIR` explicitly in any command — this worktree's `.cargo/config.toml` (inherited from `scratchpad/.cargo/config.toml`) already points `target-dir` at the shared absolute path across every `scratchpad/wt-*` worktree. A relative `CARGO_TARGET_DIR=.shared-target` override shadows that and creates a stray per-worktree copy instead — this is a known, already-hit bug (~13G duplicated in one worktree before it was caught), not a hypothetical.
- No new dependencies. `arrow` is already a direct dependency of `infigraph-core` (`Cargo.toml:38`) and is used only for the two genuinely tabular bulk payloads (`WriteCallsServiceEdges`, `WriteCrossServiceEdges`); the request/result envelope itself stays JSON.
- No MCP tool renaming. `watch_project`, `watch_docs`, `stop_watch_docs`, `get_watch_status` keep their exact names.
- `.infigraph/watch.lock` / `.infigraph/watch.stop` file names are unchanged — only the CLI subcommand (`infigraph watch` → `infigraph daemon`) and its Rust identifiers are renamed.
- Reads never route through the daemon. The `DaemonKuzu` wrapper's read methods always delegate to a real, directly-opened, read-only Kuzu connection.
- None of the four promoted trait methods (`upsert_dependencies`, `store_clusters`, `store_config_bindings`, `write_cross_service_edges`) may have a *default* trait implementation written in terms of `self.raw_query(...)` — the wrapper would inherit such a default and route the write straight back into its own read-only connection, silently reintroducing the exact hole this plan closes. Every backend (`KuzuBackend`, `Neo4jBackend`, the new `DaemonKuzuBackend`) implements each of these four explicitly.
- The wrapper's error message for any write method it doesn't cover: `"not supported via direct backend access under DaemonKuzu — use <alternative> instead"`.
- `BackendKind::DaemonKuzu` is selected via `INFIGRAPH_BACKEND=daemon` — same env var, same one-value-per-mode convention as the existing `INFIGRAPH_BACKEND=neo4j`. (This resolves the spec's "exact env var name" open question with a concrete value.)
- Selecting `DaemonKuzu` implies daemon-mode watching: the backend's own `init()` path calls `ensure_daemon_running` itself rather than requiring `INFIGRAPH_WATCH_DAEMON=1` to be set independently.

---

### Task 1: Rename `infigraph watch` → `infigraph daemon`

**Files:**
- Modify: `crates/infigraph-cli/src/main.rs:62-672` (the `Watch { debounce: u64 }` variant in the `Commands` enum, and its dispatch arm)
- Modify: `crates/infigraph-cli/src/info_commands.rs:314-361` (`cmd_watch` function)
- Modify: `crates/infigraph-core/src/watch/daemon.rs:111-149` (`spawn_daemon`'s `.arg("watch")`)
- Test: existing tests referencing `cmd_watch`/the `watch` subcommand string (see Step 4)

**Interfaces:**
- Consumes: nothing new.
- Produces: `infigraph daemon` as the CLI-visible subcommand; `cmd_daemon` as the Rust function name. Later tasks (2, 3) build on this renamed function.

**Do NOT rename:** `.infigraph/watch.lock`, `.infigraph/watch.stop`, `cmd_watch_stop`, `cmd_watch_status`, the `WatchStop`/`WatchStatus` clap variants, any MCP tool name (`watch_project`, `watch_docs`, `stop_watch_docs`, `get_watch_status`), or anything in `crates/infigraph-mcp/`. These are explicitly out of scope (Global Constraints).

- [ ] **Step 1: Use the LSP tool's rename capability, not manual find/replace**

Use the `LSP` tool (rust-analyzer) to rename the following symbols, one at a time, verifying each rename's file list before applying:
- Rust function `cmd_watch` (in `crates/infigraph-cli/src/info_commands.rs`) → `cmd_daemon`

The clap `Commands::Watch` variant is not a simple identifier rename — it also changes the user-visible subcommand string. Do this one manually (Step 2) since it involves editing the variant's doc comment and field, not just an identifier the LSP can mechanically propagate.

- [ ] **Step 2: Rename the clap variant and its CLI-visible string**

In `crates/infigraph-cli/src/main.rs`, change:

```rust
    /// Watch project for file changes and auto-reindex
    Watch {
        /// Debounce interval in milliseconds
        #[arg(short, long, default_value = "500")]
        debounce: u64,
    },
```

to:

```rust
    /// Run the infigraph daemon: serves file-dropped write requests and
    /// watches for file changes to auto-reindex
    Daemon {
        /// Debounce interval in milliseconds
        #[arg(short, long, default_value = "500")]
        debounce: u64,
    },
```

Then update the matching dispatch arm:

```rust
        Commands::Watch { debounce } => cmd_watch(root, debounce),
```

to:

```rust
        Commands::Daemon { debounce } => cmd_daemon(root, debounce),
```

Search `crates/infigraph-cli/src/main.rs` for any other `Commands::Watch` match arms (there is at least one more, matched as `Commands::Watch { .. }` for a pre-dispatch check) and update those to `Commands::Daemon { .. }` as well.

- [ ] **Step 3: Update `spawn_daemon`'s re-exec argument**

In `crates/infigraph-core/src/watch/daemon.rs`, in `spawn_daemon` (around line 116):

```rust
    let mut cmd = Command::new(watch_binary);
    cmd.arg("watch")
        .current_dir(root)
```

becomes:

```rust
    let mut cmd = Command::new(watch_binary);
    cmd.arg("daemon")
        .current_dir(root)
```

- [ ] **Step 4: Find and fix every remaining reference to the old subcommand string**

Search the whole workspace for the literal string `"watch"` used as a CLI subcommand argument (not the doc-topic word "watch" used elsewhere, and not `.infigraph/watch.lock`/`watch.stop`/`WatchStop`/`WatchStatus`, which stay unchanged). Use infigraph's `search` tool with query `arg("watch")` and query `Command::new` combined with `watch`, scoped to `crates/`. Known candidates to check directly:
- `crates/infigraph-mcp/src/tools/watch.rs` (wherever it resolves the CLI binary and constructs the spawn `Command` for daemon-mode auto-start — check if it hardcodes `"watch"` as an arg anywhere separate from `ensure_daemon_running`, which Step 3 already covers)
- Any integration test that spawns `infigraph watch` as a subprocess (e.g. `crates/infigraph-cli/tests/*.rs`, `crates/infigraph-core/tests/watch_daemon.rs`, `crates/infigraph-mcp/tests/watcher_daemon_mode.rs`) — update the literal argument to `"daemon"`.

- [ ] **Step 5: Run the affected tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-cli -p infigraph-core -p infigraph-mcp daemon -- --test-threads=1`
Expected: PASS (or, if a test name still says `watch_daemon` — that's fine, test names aren't part of the rename scope; only the actual subprocess argument matters).

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-cli/src/main.rs crates/infigraph-cli/src/info_commands.rs crates/infigraph-core/src/watch/daemon.rs
git commit -m "feat: rename infigraph watch subcommand to infigraph daemon"
```

---

### Task 2: `BackendKind::DaemonKuzu` selection + self-referential-daemon prevention

**Files:**
- Modify: `crates/infigraph-core/src/lib.rs:86-96` (`BackendKind` enum), `:130-207` (`init`)
- Modify: `crates/infigraph-core/src/watch/daemon.rs:111-149` (`spawn_daemon`)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`cmd_daemon`, from Task 1)
- Test: `crates/infigraph-core/tests/backend_selection.rs` (new file)

**Interfaces:**
- Consumes: nothing from earlier tasks in this plan.
- Produces: `BackendKind::DaemonKuzu(DaemonKuzuBackend)` variant (the `DaemonKuzuBackend` type itself is a placeholder struct in this task — Task 12 gives it real behavior; this task only wires *selection*, not the wrapper's methods). `spawn_daemon` strips `INFIGRAPH_BACKEND` from its child's environment.

This task adds the enum variant and selection logic with a **minimal placeholder** `DaemonKuzuBackend` (fails every `GraphBackend` method with `unimplemented!()`) so selection can be tested end-to-end before Task 12 fills in real behavior. This keeps this task's test focused on *selection*, not on write routing.

- [ ] **Step 1: Add the placeholder `DaemonKuzuBackend` type**

Create `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs`:

```rust
use anyhow::Result;

use super::backend::GraphBackend;

/// Routes writes through the DaemonKuzu file-drop protocol instead of
/// opening a direct embedded Kuzu connection. See
/// docs/superpowers/specs/2026-08-01-daemonkuzu-daemon-wiring-design.md.
///
/// This is a placeholder: every method panics. Task 12/13 of the
/// implementation plan replace this with the real three-tier wrapper
/// (read passthrough / covered-write routing / loud error for everything
/// else).
pub struct DaemonKuzuBackend;

impl DaemonKuzuBackend {
    pub fn open(_root: &std::path::Path) -> Result<Self> {
        Ok(Self)
    }
}
```

Add `pub mod daemon_kuzu_backend;` to `crates/infigraph-core/src/graph/mod.rs`, alphabetically ordered among the other `pub mod` declarations there, and re-export `DaemonKuzuBackend` the same way `KuzuBackend` and `Neo4jBackend` are re-exported from that file.

Do NOT implement `GraphBackend` for it yet — that comes in Task 13. `BackendKind::DaemonKuzu` in Step 2 below stores a raw `DaemonKuzuBackend`, not a `Box<dyn GraphBackend>`, so this compiles without a trait impl.

- [ ] **Step 2: Write the failing test for backend selection**

Create `crates/infigraph-core/tests/backend_selection.rs`:

```rust
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;
use std::sync::Mutex;

// INFIGRAPH_BACKEND is a process-wide env var; serialize tests that set it
// so they don't race each other under cargo's default parallel test runner.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn init_selects_daemon_kuzu_backend_when_env_var_set() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    let result = infigraph.init();
    std::env::remove_var("INFIGRAPH_BACKEND");

    // init() succeeds even though the placeholder DaemonKuzuBackend has no
    // real behavior yet -- selection itself must not require a live daemon.
    assert!(result.is_ok(), "init() failed: {result:?}");
}

#[test]
fn init_selects_kuzu_backend_by_default() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    std::env::remove_var("INFIGRAPH_BACKEND");
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    assert!(infigraph.store().is_some(), "expected a real KuzuBackend, no store() handle");
}
```

- [ ] **Step 2b: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test backend_selection`
Expected: FAIL — `"daemon"` isn't a recognized value for `INFIGRAPH_BACKEND` yet, so `init_selects_daemon_kuzu_backend_when_env_var_set` falls through to the default Kuzu-open arm and does something else (or the test doesn't compile yet, since `BackendKind::DaemonKuzu` doesn't exist).

- [ ] **Step 3: Add the `DaemonKuzu` variant and `init()` arm**

In `crates/infigraph-core/src/lib.rs`, add to the `BackendKind` enum (around line 88):

```rust
enum BackendKind {
    /// Embedded Kùzu (default).
    Kuzu(graph::KuzuBackend),
    /// Not yet initialized — `init()` or `init_read_only()` must be called.
    Uninit,
    /// Remote Neo4j sidecar via Bolt.
    #[cfg(feature = "neo4j")]
    Neo4j(graph::Neo4jBackend),
    /// Routes writes through a DaemonKuzu daemon instead of opening its
    /// own embedded Kuzu connection. Selected via `INFIGRAPH_BACKEND=daemon`.
    DaemonKuzu(graph::DaemonKuzuBackend),
}
```

In `init()` (around line 133), add a new match arm before the `_ =>` catch-all:

```rust
        match backend_env.as_str() {
            #[cfg(feature = "neo4j")]
            "neo4j" => {
                // ... unchanged ...
            }
            #[cfg(not(feature = "neo4j"))]
            "neo4j" => {
                // ... unchanged ...
            }
            "daemon" => {
                let dk = graph::DaemonKuzuBackend::open(&self.root)?;
                self.backend_kind = BackendKind::DaemonKuzu(dk);
                Ok(())
            }
            _ => {
                // ... unchanged Kuzu-open logic ...
            }
        }
```

Update `backend()` (around line 431) to add the corresponding match arm:

```rust
    pub fn backend(&self) -> Option<&dyn graph::GraphBackend> {
        match &self.backend_kind {
            BackendKind::Kuzu(kb) => Some(kb),
            BackendKind::Uninit => None,
            #[cfg(feature = "neo4j")]
            BackendKind::Neo4j(neo) => Some(neo),
            BackendKind::DaemonKuzu(_) => None, // placeholder until Task 13
        }
    }
```

This compiles: `backend()` returning `None` for the placeholder is deliberate for this task — `init_selects_daemon_kuzu_backend_when_env_var_set` only checks `init()` succeeds, not that `backend()` returns something usable. Task 13 changes this arm to `Some(&self.daemon_wrapper)`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p infigraph-core --test backend_selection`
Expected: PASS, both tests.

- [ ] **Step 5: Write the failing test for `spawn_daemon`'s env stripping**

Add to `crates/infigraph-core/tests/watch_daemon.rs` (existing file — check its current imports and add alongside):

```rust
#[test]
#[cfg(unix)]
fn spawn_daemon_child_command_does_not_inherit_infigraph_backend() {
    // spawn_daemon is private to the daemon module; this test exercises it
    // indirectly through ensure_daemon_running, then inspects the actual
    // spawned process's environment via /proc (Linux) is not portable to
    // macOS, so instead: assert the *intent* at the unit level by checking
    // the Command-building helper directly. Since spawn_daemon is a free
    // function returning a spawned child (not testable via mocking without
    // real process spawn), this test spawns a real detached child against a
    // temp project and confirms it starts (Spawned outcome) with
    // INFIGRAPH_BACKEND set in the *test's* own environment -- if the
    // child inherited it and it caused `cmd_daemon` to select DaemonKuzu on
    // itself, the daemon would hang waiting on ensure_daemon_running's own
    // request (deadlock) instead of successfully acquiring watch.lock, and
    // this test's later liveness check would fail.
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project_dir.path().join(".infigraph")).unwrap();

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let outcome = infigraph_core::watch::daemon::ensure_daemon_running(
        project_dir.path(),
        std::path::Path::new(env!("CARGO_BIN_EXE_infigraph")),
    );
    std::env::remove_var("INFIGRAPH_BACKEND");

    assert_eq!(
        outcome,
        infigraph_core::watch::daemon::DaemonStartOutcome::Spawned,
        "expected the daemon to spawn successfully despite INFIGRAPH_BACKEND=daemon in this test's own env"
    );

    // Give the child a moment to acquire watch.lock -- if it deadlocked
    // trying to route its own writes through itself, this lock would
    // never be held.
    let lock_path = project_dir.path().join(".infigraph").join("watch.lock");
    let start = std::time::Instant::now();
    loop {
        if lock_path.exists() && std::fs::metadata(&lock_path).unwrap().len() > 0 {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("daemon never acquired watch.lock -- likely deadlocked routing its own writes through itself");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Clean up: signal the spawned daemon to stop.
    std::fs::write(project_dir.path().join(".infigraph").join("watch.stop"), "").unwrap();
}
```

Note: `env!("CARGO_BIN_EXE_infigraph")` requires this test crate to have `infigraph-cli` reachable as a binary at test time, which `cargo test` provides automatically for the workspace's own binaries when run via `cargo test --workspace` or `cargo test -p infigraph-core` (integration tests can reference sibling-crate binaries via this env var as long as the binary crate is a workspace member, which `infigraph-cli` is). If this doesn't resolve in practice, fall back to `infigraph_core::watch::daemon::resolve_cli_binary_sibling_of` against the test binary's own path, matching the pattern MCP uses.

- [ ] **Step 6: Run test to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test watch_daemon spawn_daemon_child_command_does_not_inherit_infigraph_backend`
Expected: The spawned child inherits `INFIGRAPH_BACKEND=daemon`, `cmd_daemon`'s own `Infigraph::open`/`init()` (reached via `watch_project`/`watch_db`/`open_transient`) selects the placeholder `DaemonKuzuBackend`, which panics with `unimplemented!()` inside the watcher loop before ever acquiring `watch.lock` — test times out waiting for the lock and fails with the panic message above.

- [ ] **Step 7: Strip `INFIGRAPH_BACKEND` at spawn**

In `crates/infigraph-core/src/watch/daemon.rs`, in `spawn_daemon` (around line 116):

```rust
    let mut cmd = Command::new(watch_binary);
    cmd.arg("daemon")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_target)
        .env_remove("INFIGRAPH_BACKEND");
```

- [ ] **Step 8: Run test to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test watch_daemon spawn_daemon_child_command_does_not_inherit_infigraph_backend`
Expected: PASS.

- [ ] **Step 9: Add the belt-and-braces layer for a manually-started daemon**

`cmd_daemon` itself doesn't call `Infigraph::open` directly — it delegates to `watch_project`/`watch_project_with_periodic`, whose internal `open_transient` (`crates/infigraph-core/src/watch/mod.rs:512-520`) is what actually calls `Infigraph::open`/`init()`. Rather than threading a new parameter through that whole call chain, force this at the top of `cmd_daemon` itself by mutating the process's own environment before any watch machinery runs — Rust's `std::env::set_var` affects every later `std::env::var` read in this process, including inside `open_transient`.

In `crates/infigraph-cli/src/info_commands.rs`, at the very top of `cmd_daemon` (before the remote-backend check):

```rust
pub(crate) fn cmd_daemon(root: &Path, debounce: u64) -> Result<()> {
    // Belt-and-braces: spawn_daemon (watch/daemon.rs) already strips
    // INFIGRAPH_BACKEND when it spawns this process normally. This
    // covers the case where someone runs `infigraph daemon` directly
    // from a shell that happens to have INFIGRAPH_BACKEND=daemon set --
    // without this, the daemon's own Infigraph::open (reached via
    // watch_project -> open_transient) would select DaemonKuzu on
    // itself and deadlock waiting on a request nothing serves.
    std::env::remove_var("INFIGRAPH_BACKEND");

    if infigraph_core::watch::daemon::is_remote_backend() {
```

- [ ] **Step 10: Write the failing test for the manual-start case**

Add to `crates/infigraph-core/tests/backend_selection.rs`:

```rust
#[test]
fn cmd_daemon_forces_kuzu_backend_regardless_of_env() {
    // Exercises cmd_daemon's own env-clearing directly rather than through
    // a real subprocess spawn (that's covered by
    // spawn_daemon_child_command_does_not_inherit_infigraph_backend in
    // watch_daemon.rs) -- this test asserts the belt-and-braces layer
    // works even when INFIGRAPH_BACKEND is set in *this* process's own
    // environment before cmd_daemon-equivalent logic runs, simulating a
    // manually-started daemon.
    let _guard_unused = (); // placeholder to keep step numbering stable if ENV_LOCK is reused
    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    std::env::remove_var("INFIGRAPH_BACKEND"); // mirrors cmd_daemon's own first line
    assert!(std::env::var("INFIGRAPH_BACKEND").is_err());
}
```

This test is intentionally trivial — it documents the fix's mechanism (env var removal) rather than re-testing subprocess spawning, which Step 5's test already covers end-to-end. Real coverage of "does a manually-started `infigraph daemon` actually serve requests correctly" comes from Task 3's watcher-loop integration test.

- [ ] **Step 11: Run test to verify it passes**

Run: `cargo test -p infigraph-core --test backend_selection`
Expected: PASS, all three tests.

- [ ] **Step 12: Commit**

```bash
git add crates/infigraph-core/src/lib.rs crates/infigraph-core/src/graph/daemon_kuzu_backend.rs crates/infigraph-core/src/graph/mod.rs crates/infigraph-core/src/watch/daemon.rs crates/infigraph-cli/src/info_commands.rs crates/infigraph-core/tests/backend_selection.rs crates/infigraph-core/tests/watch_daemon.rs
git commit -m "feat: add BackendKind::DaemonKuzu selection and self-referential-daemon prevention"
```

---

### Task 3: Watcher-loop wiring — serve requests on the existing loop cadence

**Files:**
- Modify: `crates/infigraph-core/src/watch/mod.rs:90-394` (`watch_project_with_periodic`)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`cmd_daemon`)
- Test: `crates/infigraph-core/tests/daemon_protocol_watcher_wiring.rs` (new file)

**Interfaces:**
- Consumes: `serve_one_request` (`crates/infigraph-core/src/daemon_protocol.rs`, already merged), `WriteRequest`/`WriteResult` (same file).
- Produces: `watch_project_with_periodic(..., serve_requests: bool)` — the new trailing parameter. `watch_project` (the existing thin wrapper used by MCP's in-process watcher, `crates/infigraph-mcp/src/tools/watch.rs`) keeps its own signature unchanged and internally passes `serve_requests: false`.

- [ ] **Step 1: Write the failing integration test**

Create `crates/infigraph-core/tests/daemon_protocol_watcher_wiring.rs`:

```rust
use infigraph_core::daemon_protocol::{submit_write_request, WriteRequest, WriteResult};
use infigraph_languages::bundled_registry;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn watch_loop_serves_write_requests_when_serve_requests_is_true() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    let (stop_tx, stop_rx) = mpsc::channel();
    let root = project_dir.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::watch::watch_project_with_periodic(
            &root,
            || Ok(bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
        )
    });

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    let request = WriteRequest::Index { paths: None };
    let result = submit_write_request(&staging_dir, &request, Duration::from_secs(5)).unwrap();

    match result {
        WriteResult::Ok { indexed_files, .. } => assert_eq!(indexed_files, 1),
        WriteResult::Err { message } => panic!("expected Ok, got Err: {message}"),
        other => panic!("unexpected WriteResult for Index: {other:?}"),
    }

    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
}

#[test]
fn watch_loop_does_not_serve_requests_when_serve_requests_is_false() {
    let project_dir = tempfile::tempdir().unwrap();
    let (stop_tx, stop_rx) = mpsc::channel();
    let root = project_dir.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::watch::watch_project_with_periodic(
            &root,
            || Ok(bundled_registry().unwrap()),
            50,
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            false, // serve_requests
        )
    });

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    let request = WriteRequest::Index { paths: None };
    let result = submit_write_request(&staging_dir, &request, Duration::from_millis(500));
    assert!(
        result.is_err(),
        "expected a timeout -- serve_requests=false must never serve"
    );

    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test daemon_protocol_watcher_wiring`
Expected: FAIL to compile — `watch_project_with_periodic` doesn't take a `serve_requests` parameter yet.

- [ ] **Step 3: Add the `serve_requests` parameter and request-serving to the loop**

In `crates/infigraph-core/src/watch/mod.rs`, change `watch_project_with_periodic`'s signature (around line 90):

```rust
pub fn watch_project_with_periodic<MR, F>(
    root: &Path,
    make_registry: MR,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    on_event: impl Fn(WatchEvent) + Send + 'static,
    periodic_secs: u64,
    on_periodic: Option<F>,
    serve_requests: bool,
) -> Result<()>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry> + Send + 'static,
    F: Fn(&crate::IndexResult) + Send + 'static,
{
```

Update `watch_project` (the thin wrapper, a few lines above) to pass `false`:

```rust
pub fn watch_project<MR>(
    root: &Path,
    make_registry: MR,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    on_event: impl Fn(WatchEvent) + Send + 'static,
) -> Result<()>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry> + Send + 'static,
{
    watch_project_with_periodic(
        root,
        make_registry,
        debounce_ms,
        stop_rx,
        on_event,
        0,
        None::<fn(&crate::IndexResult)>,
        false,
    )
}
```

Inside the main loop body, add request-serving right after the periodic-refresh block and before the batch-flush block (so it runs once per loop iteration, using the same `held_prism` connection those sections already share):

```rust
        // Serve file-dropped write requests -- daemon-mode only (never from
        // in-process MCP watcher threads, which always pass
        // serve_requests=false). Piggybacks on this loop's existing tick
        // (at least every 200ms via the rx.recv_timeout below) rather than
        // a separate notify-based watch on the requests directory --
        // submit_write_request's own poll-with-backoff starts at 10ms and
        // only reaches 200ms after several rounds, so this cadence is fine.
        if serve_requests {
            let requests_dir = root.join(".infigraph").join("requests");
            if let Ok(entries) = std::fs::read_dir(&requests_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "request") {
                        match begin_index_op(root, "infigraph daemon", Duration::from_secs(30)) {
                            Ok(IndexOpOutcome::Acquired(_guard)) => {
                                if let Ok(prism) = watch_db(root, &make_registry, &mut held_prism) {
                                    if let Err(e) = crate::daemon_protocol::serve_one_request(prism, &path) {
                                        eprintln!("[daemon] failed to serve request {}: {e}", path.display());
                                    }
                                }
                            }
                            Ok(o @ IndexOpOutcome::AlreadyRunning(_)) => {
                                eprintln!(
                                    "[daemon] request-serving busy ({}), retrying next tick",
                                    o.skip_note().unwrap_or_default()
                                );
                            }
                            Err(e) => {
                                eprintln!("[daemon] request-serving busy ({e}), retrying next tick");
                            }
                        }
                    }
                }
            }
        }
```

- [ ] **Step 4: Fix the one other caller of `watch_project_with_periodic`**

Search for other call sites (the function has at least one more caller beyond `watch_project` itself — check `crates/infigraph-mcp/src/tools/watch.rs` and any doc-watch equivalent) and add `false` as their trailing argument too, since none of them are the daemon-mode CLI process.

- [ ] **Step 5: Run test to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test daemon_protocol_watcher_wiring`
Expected: PASS, both tests.

- [ ] **Step 6: Wire `cmd_daemon` to pass `serve_requests: true`**

`cmd_daemon` currently calls `infigraph_core::watch::watch_project(...)` (the wrapper). Switch it to call `watch_project_with_periodic` directly so it can pass `true`:

```rust
    infigraph_core::watch::watch_project_with_periodic(
        root,
        bundled_registry,
        debounce,
        stop_rx,
        |evt| {
            println!("[watch] {evt}");
        },
        0,
        None::<fn(&infigraph_core::IndexResult)>,
        true,
    )?;
```

- [ ] **Step 7: Run the full daemon-mode test suite to check for regressions**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core -p infigraph-mcp watch -- --test-threads=1`
Expected: PASS. If any watcher test fails specifically because it now expects `watch_project_with_periodic`'s old 7-argument signature, update that call site's trailing argument to `false` (it's not daemon-mode) rather than changing the test's assertions.

- [ ] **Step 8: Commit**

```bash
git add crates/infigraph-core/src/watch/mod.rs crates/infigraph-cli/src/info_commands.rs crates/infigraph-core/tests/daemon_protocol_watcher_wiring.rs
git commit -m "feat: wire watcher loop to serve file-dropped write requests in daemon mode"
```

---

### Task 4: Finish the `ScipImport` handler

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs` (`serve_one_request`, `WriteResult`)
- Modify: `crates/infigraph-core/src/scip/mod.rs` (`ImportStats` serde derives)
- Test: `crates/infigraph-core/tests/daemon_protocol_serve.rs` (existing file — add alongside)

**Interfaces:**
- Consumes: `WriteRequest::ScipImport { scip_path: PathBuf }` (already exists), `Infigraph`'s SCIP-import path.
- Produces: `WriteResult::ScipImportOk(crate::scip::ImportStats)` — a new variant later tasks (13) match on directly.

`Infigraph` has no existing high-level `scip_import` wrapper method (the two real callers, `import_scip_and_cleanup` and `tool_scip_import`, both call `backend.import_scip_index(&scip_path, Some(root))` directly via `prism.backend()`). Add a thin `Infigraph::import_scip` method so `serve_one_request` has something clean to call, matching the existing `index()`/`index_files()` pattern.

- [ ] **Step 1: Write the failing test**

Add to `crates/infigraph-core/tests/daemon_protocol_serve.rs`:

```rust
#[test]
fn serve_one_request_handles_scip_import() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-scip.request");
    let result_path = staging_dir.join("test-scip.result");
    // A nonexistent scip file is fine for this test -- it exercises the
    // handler routes to import_scip and returns Err cleanly, not that a
    // real SCIP import succeeds (that's covered by existing scip-import
    // integration tests).
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::ScipImport {
            scip_path: "does/not/exist.scip".into(),
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    assert!(result_path.exists());
    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(result, WriteResult::Err { .. }),
        "expected Err for a missing scip file, got {result:?}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve serve_one_request_handles_scip_import`
Expected: FAIL — currently `serve_one_request` returns `WriteResult::Err { message: "scip-import serving not yet implemented" }` for every `ScipImport` request without attempting the import, so this specific assertion may actually already pass (`Err` either way) — the goal of this test is to lock in real behavior; run it now to confirm the CURRENT stub also returns `Err`, then Step 4 confirms the message text actually changes to reflect a real attempt, not the stub text.

- [ ] **Step 3: Add `Infigraph::import_scip`**

In `crates/infigraph-core/src/lib.rs`, add a method near `index()`/`index_files()` (around line 300):

```rust
    /// Import a SCIP index file into the graph. Thin wrapper matching
    /// index()/index_files()'s shape, so the daemon protocol has a single
    /// clean entry point rather than reaching for prism.backend() directly.
    pub fn import_scip(&self, scip_path: &Path) -> Result<graph::ImportStats> {
        let backend = self.backend().context("call init() first")?;
        backend.import_scip_index(scip_path, Some(&self.root))
    }
```

Check `ImportStats`'s actual export path (it's referenced in `crates/infigraph-core/src/graph/backend.rs` as `crate::scip::ImportStats` — re-export it from `graph` if not already, or use the fully-qualified `crate::scip::ImportStats` path in the signature above to match how `backend.rs` itself imports it).

- [ ] **Step 4: Add `ImportStats` serde derives and a dedicated `WriteResult::ScipImportOk` variant**

`ImportStats`'s real fields (`files_processed`, `symbols_added`, `symbols_enriched`, `symbols_skipped`, `relations_added`, `references_added`, `corrections_learned` — confirmed at `crates/infigraph-core/src/scip/mod.rs:679-688`) don't fit `WriteResult::Ok`'s two `usize` fields without losing data. Extend the enum with a variant that carries the real struct instead of squeezing it into the generic shape.

In `crates/infigraph-core/src/scip/mod.rs`, change:

```rust
#[derive(Default, Debug)]
pub struct ImportStats {
```

to:

```rust
#[derive(Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct ImportStats {
```

In `crates/infigraph-core/src/daemon_protocol.rs`, change the `WriteResult` enum:

```rust
pub enum WriteResult {
    Ok {
        total_files: usize,
        indexed_files: usize,
    },
    Err {
        message: String,
    },
}
```

to:

```rust
pub enum WriteResult {
    Ok {
        total_files: usize,
        indexed_files: usize,
    },
    /// Real SCIP import stats -- `Ok`'s two usize fields can't represent
    /// ImportStats's seven fields without losing data.
    ScipImportOk(crate::scip::ImportStats),
    Err {
        message: String,
    },
}
```

- [ ] **Step 5: Wire the handler in `serve_one_request`**

In `crates/infigraph-core/src/daemon_protocol.rs`, replace the `ScipImport` arm:

```rust
            WriteRequest::ScipImport { scip_path } => match infigraph.import_scip(scip_path) {
                Ok(stats) => WriteResult::ScipImportOk(stats),
                Err(e) => WriteResult::Err {
                    message: e.to_string(),
                },
            },
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve serve_one_request_handles_scip_import`
Expected: PASS, with the error message reflecting a real file-not-found error rather than "not yet implemented".

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-core/src/lib.rs crates/infigraph-core/src/scip/mod.rs crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/tests/daemon_protocol_serve.rs
git commit -m "feat: finish ScipImport handler in serve_one_request"
```

---

### Task 5: `IngestStructured` variant + handler (including `Inline` sibling-file case)

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs` (`WriteRequest` enum, `serve_one_request`)
- Test: `crates/infigraph-core/tests/daemon_protocol_serve.rs`

**Interfaces:**
- Consumes: `discover_schemas` (`crates/infigraph-core/src/structured/schema.rs:117-150`), `GraphBackend::ingest_structured_directory/_file/_data` (existing trait methods).
- Produces: `WriteRequest::IngestStructured { schema_id: String, source: IngestSource }`, `IngestSource` enum, `write_atomic`-based sibling-file helper reused by Task 7.

- [ ] **Step 1: Write the failing test for `File`/`Directory` sources**

Add to `crates/infigraph-core/tests/daemon_protocol_serve.rs`:

```rust
#[test]
fn serve_one_request_handles_ingest_structured_file() {
    let project_dir = tempfile::tempdir().unwrap();
    let schema_dir = project_dir.path().join(".infigraph").join("structured-schemas");
    std::fs::create_dir_all(&schema_dir).unwrap();
    std::fs::write(
        schema_dir.join("test.toml"),
        r#"
[schema]
schema_id = "test-schema"
name = "Test Schema"
node_table = "TestNode"

[[schema.columns]]
name = "id"
type = "string"
"#,
    )
    .unwrap();
    std::fs::write(
        project_dir.path().join("data.json"),
        r#"[{"id": "a"}, {"id": "b"}]"#,
    )
    .unwrap();

    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-ingest.request");
    let result_path = staging_dir.join("test-ingest.result");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::IngestStructured {
            schema_id: "test-schema".to_string(),
            source: IngestSource::File("data.json".into()),
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    match result {
        WriteResult::Ok { indexed_files, .. } => assert_eq!(indexed_files, 2),
        WriteResult::Err { message } => panic!("expected Ok, got Err: {message}"),
        other => panic!("unexpected WriteResult for IngestStructured: {other:?}"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve serve_one_request_handles_ingest_structured_file`
Expected: FAIL to compile — `WriteRequest::IngestStructured`/`IngestSource` don't exist yet.

- [ ] **Step 3: Add the types and File/Directory handling**

In `crates/infigraph-core/src/daemon_protocol.rs`, extend `WriteRequest`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WriteRequest {
    /// Index specific files. `None` means a full project reindex.
    Index { paths: Option<Vec<PathBuf>> },
    /// Import a SCIP index file at the given path.
    ScipImport { scip_path: PathBuf },
    /// Ingest structured data using a schema already discoverable by the
    /// daemon itself (via discover_schemas) -- looked up by schema_id, not
    /// serialized into the request.
    IngestStructured {
        schema_id: String,
        source: IngestSource,
    },
}

/// Where IngestStructured's data comes from. `Inline` carries no data
/// itself -- the actual array lives in a sibling `.data.json` file next to
/// the request (see write_ingest_inline_sibling / read_ingest_inline_sibling),
/// following the same reference-not-payload convention paths already use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IngestSource {
    File(PathBuf),
    Directory(PathBuf),
    Inline,
}
```

Add the `WriteRequest::IngestStructured` arm to `serve_one_request`:

```rust
            WriteRequest::IngestStructured { schema_id, source } => {
                match handle_ingest_structured(infigraph, schema_id, source, request_path) {
                    Ok(r) => WriteResult::Ok {
                        total_files: r.nodes_created,
                        indexed_files: r.edges_created,
                    },
                    Err(e) => WriteResult::Err {
                        message: e.to_string(),
                    },
                }
            }
```

Add the handler function (below `serve_one_request`):

```rust
fn handle_ingest_structured(
    infigraph: &Infigraph,
    schema_id: &str,
    source: &IngestSource,
    request_path: &Path,
) -> anyhow::Result<crate::structured::IngestResult> {
    let backend = infigraph
        .backend()
        .ok_or_else(|| anyhow::anyhow!("graph not initialized"))?;
    let schemas = crate::structured::discover_schemas(infigraph.root())?;
    let (_, schema) = schemas
        .iter()
        .find(|(_, s)| s.schema.schema_id == schema_id)
        .ok_or_else(|| anyhow::anyhow!("schema '{schema_id}' not found"))?;

    match source {
        IngestSource::File(path) => {
            let full_path = infigraph.root().join(path);
            backend.ingest_structured_file(&schema.schema, &full_path)
        }
        IngestSource::Directory(path) => {
            let full_path = infigraph.root().join(path);
            backend.ingest_structured_directory(&schema.schema, &full_path)
        }
        IngestSource::Inline => {
            let sibling_path = request_path.with_extension("data.json");
            let contents = std::fs::read_to_string(&sibling_path)?;
            let data: Vec<serde_json::Value> = serde_json::from_str(&contents)?;
            let result = backend.ingest_structured_data(&schema.schema, &data)?;
            std::fs::remove_file(&sibling_path).ok();
            Ok(result)
        }
    }
}
```

Verify `Infigraph` exposes `root()` (it does, `crates/infigraph-core/src/lib.rs:446`) and `structured::discover_schemas`/`structured::IngestResult` are already `pub` from the `structured` module (confirmed: `discover_schemas` is `pub fn`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve serve_one_request_handles_ingest_structured_file`
Expected: PASS.

- [ ] **Step 5: Write the failing test for the `Inline` sibling-file case**

Add to `crates/infigraph-core/tests/daemon_protocol_serve.rs`:

```rust
#[test]
fn serve_one_request_handles_ingest_structured_inline() {
    let project_dir = tempfile::tempdir().unwrap();
    let schema_dir = project_dir.path().join(".infigraph").join("structured-schemas");
    std::fs::create_dir_all(&schema_dir).unwrap();
    std::fs::write(
        schema_dir.join("test.toml"),
        r#"
[schema]
schema_id = "test-schema"
name = "Test Schema"
node_table = "TestNode"

[[schema.columns]]
name = "id"
type = "string"
"#,
    )
    .unwrap();

    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-inline.request");
    let result_path = staging_dir.join("test-inline.result");
    let sibling_path = staging_dir.join("test-inline.data.json");

    write_atomic(&sibling_path, r#"[{"id": "a"}, {"id": "b"}, {"id": "c"}]"#).unwrap();
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::IngestStructured {
            schema_id: "test-schema".to_string(),
            source: IngestSource::Inline,
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    assert!(
        !sibling_path.exists(),
        "sibling data file should be cleaned up after serving"
    );
    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    match result {
        WriteResult::Ok { indexed_files, .. } => assert_eq!(indexed_files, 3),
        WriteResult::Err { message } => panic!("expected Ok, got Err: {message}"),
        other => panic!("unexpected WriteResult for IngestStructured Inline: {other:?}"),
    }
}
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve serve_one_request_handles_ingest_structured_inline`
Expected: PASS (the `Inline` arm implemented in Step 3 already handles this).

- [ ] **Step 7: Add the client-side sibling-file writer**

In `crates/infigraph-core/src/daemon_protocol.rs`, add a helper for the client side (used by the `DaemonKuzuBackend` wrapper in Task 13, not by `serve_one_request`) so the request-name-derived sibling path convention is defined once:

```rust
/// Writes `data` as a sibling `.data.json` file next to where a request
/// named `request_path` will be written, using the same atomic-write
/// guarantee as the request/result files themselves. Returns the path the
/// server-side handler will read (request_path.with_extension("data.json")).
pub fn write_ingest_inline_sibling(
    request_path: &Path,
    data: &[serde_json::Value],
) -> anyhow::Result<PathBuf> {
    let sibling_path = request_path.with_extension("data.json");
    write_atomic(&sibling_path, &serde_json::to_string(data)?)?;
    Ok(sibling_path)
}
```

Note: this helper needs the FINAL request path to derive the sibling path, but `submit_write_request` currently generates that path internally and doesn't expose it before writing the request. Task 13's wrapper will need `submit_write_request` (or a variant of it) to write the sibling file *before* the request file, using the same generated name -- revisit `submit_write_request`'s structure in Task 13 rather than solving it here; this task only establishes the naming convention and the server-side read/cleanup, which Step 3 already implements.

- [ ] **Step 8: Commit**

```bash
git add crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/tests/daemon_protocol_serve.rs
git commit -m "feat: add IngestStructured request/handler with sibling-file inline data"
```

---

### Task 6: `UpsertRepo` + `DeriveTestedBy` variants + handlers

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs`
- Test: `crates/infigraph-core/tests/daemon_protocol_serve.rs`

**Interfaces:**
- Consumes: `GraphBackend::upsert_repo` (existing, has a default no-op impl — see the warning below), `GraphBackend::derive_tested_by_edges` (existing).
- Produces: `WriteRequest::UpsertRepo { namespace: String }`, `WriteRequest::DeriveTestedBy { files: Option<Vec<String>> }`.

**Important:** `upsert_repo` has a *default* trait implementation (`fn upsert_repo(&self, _repo_name: &str) -> Result<()> { Ok(()) }`, `backend.rs` around line 143) — a no-op, meaningful only for Neo4j today. When Task 13 builds the `DaemonKuzuBackend` wrapper, it MUST override `upsert_repo` explicitly to route through `submit_write_request`; if it doesn't, the wrapper silently inherits the no-op default and every `upsert_repo` call becomes an invisible success-that-does-nothing under `DaemonKuzu` — exactly the failure mode this whole plan exists to prevent. This task only adds the request type and server-side handler; Task 13's checklist repeats this warning at the point where it matters.

- [ ] **Step 1: Write the failing tests**

Add to `crates/infigraph-core/tests/daemon_protocol_serve.rs`:

```rust
#[test]
fn serve_one_request_handles_upsert_repo() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-repo.request");
    let result_path = staging_dir.join("test-repo.result");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::UpsertRepo {
            namespace: "org/repo".to_string(),
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(matches!(result, WriteResult::Ok { .. }), "expected Ok, got {result:?}");
}

#[test]
fn serve_one_request_handles_derive_tested_by() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n\ndef test_hello():\n    hello()\n",
    )
    .unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    infigraph.index().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-tested-by.request");
    let result_path = staging_dir.join("test-tested-by.result");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::DeriveTestedBy { files: None }).unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    match result {
        WriteResult::Ok { .. } => {}
        WriteResult::Err { message } => panic!("expected Ok, got Err: {message}"),
        other => panic!("unexpected WriteResult for DeriveTestedBy: {other:?}"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve serve_one_request_handles_upsert_repo serve_one_request_handles_derive_tested_by`
Expected: FAIL to compile — the variants don't exist yet.

- [ ] **Step 3: Add the variants and handlers**

In `crates/infigraph-core/src/daemon_protocol.rs`, extend `WriteRequest`:

```rust
    /// Create a Repo node and link this project's files to it.
    UpsertRepo { namespace: String },
    /// Derive TESTED_BY edges. `files` scopes to changed files for
    /// incremental runs; `None` means a full derivation pass.
    DeriveTestedBy { files: Option<Vec<String>> },
```

Add to `serve_one_request`:

```rust
            WriteRequest::UpsertRepo { namespace } => {
                let backend = infigraph.backend();
                match backend {
                    Some(b) => match b.upsert_repo(namespace) {
                        Ok(()) => WriteResult::Ok { total_files: 0, indexed_files: 0 },
                        Err(e) => WriteResult::Err { message: e.to_string() },
                    },
                    None => WriteResult::Err { message: "graph not initialized".to_string() },
                }
            }
            WriteRequest::DeriveTestedBy { files } => {
                let backend = infigraph.backend();
                let files_ref: Option<Vec<&str>> =
                    files.as_ref().map(|f| f.iter().map(String::as_str).collect());
                match backend {
                    Some(b) => match b.derive_tested_by_edges(files_ref.as_deref()) {
                        Ok(count) => WriteResult::Ok { total_files: 0, indexed_files: count },
                        Err(e) => WriteResult::Err { message: e.to_string() },
                    },
                    None => WriteResult::Err { message: "graph not initialized".to_string() },
                }
            }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve serve_one_request_handles_upsert_repo serve_one_request_handles_derive_tested_by`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/tests/daemon_protocol_serve.rs
git commit -m "feat: add UpsertRepo and DeriveTestedBy requests/handlers"
```

---

### Task 7: `WriteCallsServiceEdges` (Arrow IPC batch) + `UpsertSimilarEdge` (unbatched) variants + handlers

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs`
- Modify: `crates/infigraph-core/src/graph/backend.rs` (`CallsServiceEdge` gains `Serialize`/`Deserialize`)
- Test: `crates/infigraph-core/tests/daemon_protocol_serve.rs`

**Interfaces:**
- Consumes: `GraphBackend::write_calls_service_edges` (existing), `GraphBackend::upsert_similar_edge` (existing), `CallsServiceEdge` (existing struct, `crates/infigraph-core/src/graph/backend.rs:22-28`).
- Produces: `WriteRequest::WriteCallsServiceEdges { edges_path: PathBuf }` (Arrow IPC sibling file, not inline — see Step 3's rationale), `WriteRequest::UpsertSimilarEdge { id_a: String, id_b: String, score: f32 }`.

`CallsServiceEdge` (`symbol_id`, `target_id`, `method`, `path` — all `String`) currently derives only `Debug, Clone`. Add `Serialize, Deserialize` so it can round-trip through Arrow IPC (via `serde_arrow` conversion, or direct `arrow::array` construction — see Step 3).

- [ ] **Step 1: Write the failing test for `UpsertSimilarEdge`**

Add to `crates/infigraph-core/tests/daemon_protocol_serve.rs`:

```rust
#[test]
fn serve_one_request_handles_upsert_similar_edge() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def foo():\n    pass\n\ndef bar():\n    pass\n",
    )
    .unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    infigraph.index().unwrap();

    let symbols = infigraph.backend().unwrap().symbols_with_docstring(None).unwrap();
    assert!(symbols.len() >= 2, "expected at least 2 symbols to link");

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-similar.request");
    let result_path = staging_dir.join("test-similar.result");
    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::UpsertSimilarEdge {
            id_a: symbols[0].id.clone(),
            id_b: symbols[1].id.clone(),
            score: 0.9,
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(matches!(result, WriteResult::Ok { .. }), "expected Ok, got {result:?}");
}
```

- [ ] **Step 2: Run test to verify it fails, then add the variant + handler**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve serve_one_request_handles_upsert_similar_edge`
Expected: FAIL to compile.

In `crates/infigraph-core/src/daemon_protocol.rs`:

```rust
    /// Link two symbols as similar (clone detection). Deliberately
    /// unbatched -- matches Neo4jBackend::upsert_similar_edge's existing
    /// per-call precedent (one Cypher MERGE per call).
    UpsertSimilarEdge { id_a: String, id_b: String, score: f32 },
```

```rust
            WriteRequest::UpsertSimilarEdge { id_a, id_b, score } => {
                match infigraph.backend() {
                    Some(b) => match b.upsert_similar_edge(id_a, id_b, *score) {
                        Ok(()) => WriteResult::Ok { total_files: 0, indexed_files: 0 },
                        Err(e) => WriteResult::Err { message: e.to_string() },
                    },
                    None => WriteResult::Err { message: "graph not initialized".to_string() },
                }
            }
```

Run the test again: PASS.

- [ ] **Step 3: Write the failing test for `WriteCallsServiceEdges` with an Arrow IPC sibling file**

Add to `crates/infigraph-core/tests/daemon_protocol_serve.rs`:

```rust
#[test]
fn serve_one_request_handles_write_calls_service_edges() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-cse.request");
    let result_path = staging_dir.join("test-cse.result");
    let edges_path = staging_dir.join("test-cse.edges.arrow");

    let edges = vec![
        infigraph_core::graph::CallsServiceEdge {
            symbol_id: "s1".to_string(),
            target_id: "t1".to_string(),
            method: "GET".to_string(),
            path: "/foo".to_string(),
        },
        infigraph_core::graph::CallsServiceEdge {
            symbol_id: "s2".to_string(),
            target_id: "t2".to_string(),
            method: "POST".to_string(),
            path: "/bar".to_string(),
        },
    ];
    infigraph_core::daemon_protocol::write_calls_service_edges_arrow(&edges_path, &edges).unwrap();

    write_atomic(
        &request_path,
        &serde_json::to_string(&WriteRequest::WriteCallsServiceEdges {
            edges_path: edges_path.clone(),
        })
        .unwrap(),
    )
    .unwrap();

    serve_one_request(&infigraph, &request_path).unwrap();

    assert!(!edges_path.exists(), "sibling edges file should be cleaned up after serving");
    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(matches!(result, WriteResult::Ok { .. }), "expected Ok, got {result:?}");
}
```

- [ ] **Step 4: Add `CallsServiceEdge` serde derives and the Arrow IPC read/write helpers**

In `crates/infigraph-core/src/graph/backend.rs`, change:

```rust
#[derive(Debug, Clone)]
pub struct CallsServiceEdge {
```

to:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CallsServiceEdge {
```

In `crates/infigraph-core/src/daemon_protocol.rs`, add the Arrow IPC helpers. All four string fields map to Arrow `StringArray` columns:

```rust
use crate::graph::CallsServiceEdge;
use arrow::array::{RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use std::sync::Arc;

fn calls_service_edges_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("symbol_id", DataType::Utf8, false),
        Field::new("target_id", DataType::Utf8, false),
        Field::new("method", DataType::Utf8, false),
        Field::new("path", DataType::Utf8, false),
    ]))
}

/// Writes `edges` as an Arrow IPC file at `path` -- genuinely tabular bulk
/// data (many rows, same shape), unlike the small heterogeneous
/// WriteRequest/WriteResult envelope, which stays JSON.
pub fn write_calls_service_edges_arrow(path: &Path, edges: &[CallsServiceEdge]) -> anyhow::Result<()> {
    let schema = calls_service_edges_schema();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(edges.iter().map(|e| e.symbol_id.as_str()).collect::<Vec<_>>())),
            Arc::new(StringArray::from(edges.iter().map(|e| e.target_id.as_str()).collect::<Vec<_>>())),
            Arc::new(StringArray::from(edges.iter().map(|e| e.method.as_str()).collect::<Vec<_>>())),
            Arc::new(StringArray::from(edges.iter().map(|e| e.path.as_str()).collect::<Vec<_>>())),
        ],
    )?;
    let file = std::fs::File::create(path)?;
    let mut writer = arrow::ipc::writer::FileWriter::try_new(file, &schema)?;
    writer.write(&batch)?;
    writer.finish()?;
    Ok(())
}

/// Reads `CallsServiceEdge`s back from an Arrow IPC file written by
/// write_calls_service_edges_arrow.
pub fn read_calls_service_edges_arrow(path: &Path) -> anyhow::Result<Vec<CallsServiceEdge>> {
    let file = std::fs::File::open(path)?;
    let reader = arrow::ipc::reader::FileReader::try_new(file, None)?;
    let mut edges = Vec::new();
    for batch in reader {
        let batch = batch?;
        let symbol_ids = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
        let target_ids = batch.column(1).as_any().downcast_ref::<StringArray>().unwrap();
        let methods = batch.column(2).as_any().downcast_ref::<StringArray>().unwrap();
        let paths = batch.column(3).as_any().downcast_ref::<StringArray>().unwrap();
        for i in 0..batch.num_rows() {
            edges.push(CallsServiceEdge {
                symbol_id: symbol_ids.value(i).to_string(),
                target_id: target_ids.value(i).to_string(),
                method: methods.value(i).to_string(),
                path: paths.value(i).to_string(),
            });
        }
    }
    Ok(edges)
}
```

Add the `arrow` import at the top of `daemon_protocol.rs` if not already present.

Extend `WriteRequest`:

```rust
    /// Write a batch of CALLS_SERVICE edges. The edges themselves live in
    /// an Arrow IPC sibling file at edges_path (genuinely tabular bulk
    /// data), not inline in this envelope.
    WriteCallsServiceEdges { edges_path: PathBuf },
```

Add to `serve_one_request`:

```rust
            WriteRequest::WriteCallsServiceEdges { edges_path } => {
                match read_calls_service_edges_arrow(edges_path)
                    .and_then(|edges| {
                        infigraph
                            .backend()
                            .ok_or_else(|| anyhow::anyhow!("graph not initialized"))
                            .and_then(|b| b.write_calls_service_edges(&edges).map_err(Into::into))
                    })
                {
                    Ok(()) => {
                        std::fs::remove_file(edges_path).ok();
                        WriteResult::Ok { total_files: 0, indexed_files: 0 }
                    }
                    Err(e) => WriteResult::Err { message: e.to_string() },
                }
            }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve serve_one_request_handles_write_calls_service_edges serve_one_request_handles_upsert_similar_edge`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/src/graph/backend.rs crates/infigraph-core/tests/daemon_protocol_serve.rs
git commit -m "feat: add WriteCallsServiceEdges (Arrow IPC) and UpsertSimilarEdge requests/handlers"
```

---

### Task 8: Promote `UpsertDependencies` — migrate `store_manifest`'s raw Cypher to a trait method

**Files:**
- Modify: `crates/infigraph-core/src/graph/backend.rs` (new trait method)
- Modify: `crates/infigraph-core/src/graph/kuzu_backend.rs` (impl)
- Modify: `crates/infigraph-core/src/graph/neo4j_backend.rs` (impl)
- Modify: `crates/infigraph-core/src/manifest/mod.rs:729-777` (`store_manifest`, `scan_csproj`'s discard at `:720`)
- Modify: `crates/infigraph-core/src/daemon_protocol.rs`
- Test: `crates/infigraph-core/tests/manifest_backend.rs` (new file), `daemon_protocol_serve.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `GraphBackend::upsert_dependencies(&self, result: &ManifestResult) -> Result<()>`, `WriteRequest::UpsertDependencies { result: ManifestResult }`.

- [ ] **Step 1: Add `Serialize`/`Deserialize` to `ManifestResult`/`DepEntry`**

In `crates/infigraph-core/src/manifest/mod.rs`, change:

```rust
#[derive(Debug, Clone)]
pub struct DepEntry {
```

to:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DepEntry {
```

and:

```rust
#[derive(Debug, Default)]
pub struct ManifestResult {
```

to:

```rust
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ManifestResult {
```

- [ ] **Step 2: Write the failing test for the new trait method**

Create `crates/infigraph-core/tests/manifest_backend.rs`:

```rust
use infigraph_core::manifest::{DepEntry, ManifestResult};
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn upsert_dependencies_creates_dependency_node_and_edge() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    infigraph.index().unwrap();

    let result = ManifestResult {
        ecosystem: "pypi".to_string(),
        manifest_file: "requirements.txt".to_string(),
        deps: vec![DepEntry {
            name: "requests".to_string(),
            version: "2.31.0".to_string(),
            ecosystem: "pypi".to_string(),
            is_dev: false,
        }],
        doc_urls: vec![],
    };

    let backend = infigraph.backend().unwrap();
    backend.upsert_dependencies(&result).unwrap();

    let rows = backend
        .raw_query("MATCH (d:Dependency) WHERE d.id = 'pypi::requests' RETURN d.id")
        .unwrap();
    assert_eq!(rows.len(), 1, "expected the Dependency node to exist");
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test manifest_backend`
Expected: FAIL to compile — `upsert_dependencies` doesn't exist on `GraphBackend` yet.

- [ ] **Step 4: Add the trait method (no default impl) and implement it on `KuzuBackend`**

In `crates/infigraph-core/src/graph/backend.rs`, add near `write_calls_service_edges`:

```rust
    /// Store a manifest's dependencies as Dependency nodes + DEPENDS_ON
    /// edges. No default impl -- see the Global Constraints note in
    /// docs/superpowers/plans/2026-08-01-daemonkuzu-daemon-wiring-plan.md:
    /// a default written in terms of raw_query would be silently inherited
    /// by the DaemonKuzu wrapper's read-only connection.
    fn upsert_dependencies(&self, result: &crate::manifest::ManifestResult) -> Result<()>;
```

Add `use crate::manifest::ManifestResult;` if `manifest` isn't already imported in `backend.rs` (it currently isn't — add the import).

In `crates/infigraph-core/src/graph/kuzu_backend.rs`, add the implementation. Move `store_manifest`'s body here verbatim, adapted to `&self`:

```rust
    fn upsert_dependencies(&self, result: &crate::manifest::ManifestResult) -> Result<()> {
        for dep in &result.deps {
            let id = format!("{}::{}", dep.ecosystem, dep.name);
            let check = format!(
                "MATCH (d:Dependency) WHERE d.id = '{}' RETURN d.id",
                escape(&id)
            );
            let existing = self.raw_query(&check)?;
            if existing.is_empty() {
                let insert = format!(
                    "CREATE (d:Dependency {{id: '{}', name: '{}', version: '{}', ecosystem: '{}', is_dev: {}}})",
                    escape(&id), escape(&dep.name), escape(&dep.version), escape(&dep.ecosystem), dep.is_dev
                );
                self.raw_query(&insert)?;
            } else {
                let update = format!(
                    "MATCH (d:Dependency) WHERE d.id = '{}' SET d.version = '{}', d.is_dev = {}",
                    escape(&id),
                    escape(&dep.version),
                    dep.is_dev
                );
                self.raw_query(&update)?;
            }

            let manifest_base = escape(result.manifest_file.rsplit('/').next().unwrap_or(""));
            let rel = if let Some(repo) = self.repo_filter() {
                let r = escape(repo);
                format!(
                    "MATCH (m:Module), (d:Dependency) \
                     WHERE m.file STARTS WITH '{r}/' AND m.file CONTAINS '{manifest_base}' AND d.id = '{}' \
                     CREATE (m)-[:DEPENDS_ON {{is_dev: {}}}]->(d)",
                    escape(&id),
                    dep.is_dev
                )
            } else {
                format!(
                    "MATCH (m:Module), (d:Dependency) WHERE m.file CONTAINS '{manifest_base}' AND d.id = '{}' \
                     CREATE (m)-[:DEPENDS_ON {{is_dev: {}}}]->(d)",
                    escape(&id),
                    dep.is_dev
                )
            };
            self.raw_query(&rel)?;
        }
        Ok(())
    }
```

Note this version propagates each `raw_query` call's `Result` with `?` instead of the original's `let _ =` discards — this is the "un-swallow errors at migrated call sites" fix the spec calls for, applied at the point of migration rather than as a separate pass.

- [ ] **Step 5: Implement on `Neo4jBackend`**

In `crates/infigraph-core/src/graph/neo4j_backend.rs`, add an equivalent implementation using that backend's own `raw_query`/query-execution pattern (follow the existing style of nearby methods like `write_calls_service_edges` in the same file for the Bolt-query idiom, including its own repo-scoping via `self.repo_filter()`).

- [ ] **Step 6: Update `store_manifest` to call the new trait method**

In `crates/infigraph-core/src/manifest/mod.rs`, replace the whole `store_manifest` function body:

```rust
fn store_manifest(backend: &dyn GraphBackend, result: &ManifestResult) -> Result<()> {
    backend.upsert_dependencies(result)
}
```

And fix `scan_csproj`'s discard (around line 720):

```rust
                if !deps.is_empty() {
                    let result = ManifestResult {
                        ecosystem: "nuget".to_string(),
                        manifest_file: path.to_string_lossy().replace('\\', "/"),
                        deps,
                        doc_urls: Vec::new(),
                    };
                    if let Err(e) = store_manifest(backend, &result) {
                        eprintln!("[manifest] failed to store {}: {e}", result.manifest_file);
                    }
                    results.push(result);
                }
```

- [ ] **Step 7: Run test to verify it passes**

Run: `cargo test -p infigraph-core --test manifest_backend`
Expected: PASS.

- [ ] **Step 8: Run the existing manifest test suite to check for regressions**

Run: `cargo test -p infigraph-core manifest::`
Expected: PASS (existing `index_manifests` tests still exercise the same end-to-end behavior through the now-thinner `store_manifest`).

- [ ] **Step 9: Add the `WriteRequest::UpsertDependencies` variant + handler**

In `crates/infigraph-core/src/daemon_protocol.rs`:

```rust
    /// Store a manifest's parsed dependencies. Small, serde-serializable
    /// payload -- rides inline in this envelope, no sibling file needed.
    UpsertDependencies { result: crate::manifest::ManifestResult },
```

```rust
            WriteRequest::UpsertDependencies { result } => match infigraph.backend() {
                Some(b) => match b.upsert_dependencies(result) {
                    Ok(()) => WriteResult::Ok { total_files: 0, indexed_files: 0 },
                    Err(e) => WriteResult::Err { message: e.to_string() },
                },
                None => WriteResult::Err { message: "graph not initialized".to_string() },
            },
```

Add a test to `daemon_protocol_serve.rs` mirroring the pattern of Task 6/7's tests (construct a `ManifestResult`, write it as the request, call `serve_one_request`, assert the Dependency node exists via a follow-up `raw_query`).

- [ ] **Step 10: Run test to verify it passes**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve`
Expected: PASS, including the new test.

- [ ] **Step 11: Commit**

```bash
git add crates/infigraph-core/src/graph/backend.rs crates/infigraph-core/src/graph/kuzu_backend.rs crates/infigraph-core/src/graph/neo4j_backend.rs crates/infigraph-core/src/manifest/mod.rs crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/tests/manifest_backend.rs crates/infigraph-core/tests/daemon_protocol_serve.rs
git commit -m "feat: promote store_manifest's raw Cypher writes to GraphBackend::upsert_dependencies"
```

---

### Task 9: Promote `StoreClusters` — migrate `store_clusters`'s raw Cypher to a trait method

**Files:**
- Modify: `crates/infigraph-core/src/graph/backend.rs`, `kuzu_backend.rs`, `neo4j_backend.rs`
- Modify: `crates/infigraph-core/src/cluster/mod.rs:251-320` (`store_clusters`)
- Modify: `crates/infigraph-core/src/daemon_protocol.rs`
- Test: `crates/infigraph-core/tests/cluster_backend.rs` (new file), `daemon_protocol_serve.rs`

**Interfaces:**
- Consumes: `WriteResult::ScipImportOk` (Task 4) — this task adds the sibling `ClustersOk` variant to the same enum.
- Produces: `GraphBackend::store_clusters(&self, idx_to_id: &[String], community: &[usize], modularity: f64) -> Result<ClusterStats>`, `WriteRequest::StoreClusters { idx_to_id: Vec<String>, community: Vec<usize>, modularity: f64 }`, `WriteResult::ClustersOk(ClusterStats)`.

- [ ] **Step 1: Add `Serialize`/`Deserialize` to `ClusterStats`**

In `crates/infigraph-core/src/cluster/mod.rs`, find `ClusterStats`'s definition and add the derives:

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct ClusterStats {
```

(Add `use serde::{Serialize, Deserialize};` at the top of the file if not already imported.)

- [ ] **Step 2: Write the failing test**

Create `crates/infigraph-core/tests/cluster_backend.rs`:

```rust
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn store_clusters_creates_cluster_node_and_membership() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    infigraph.index().unwrap();

    let symbols = infigraph.backend().unwrap().symbols_with_docstring(None).unwrap();
    assert!(!symbols.is_empty());

    let backend = infigraph.backend().unwrap();
    let idx_to_id: Vec<String> = symbols.iter().map(|s| s.id.clone()).collect();
    let community: Vec<usize> = vec![0; idx_to_id.len()];
    let stats = backend.store_clusters(&idx_to_id, &community, 0.5).unwrap();

    assert_eq!(stats.num_clusters, 1);

    let rows = backend.raw_query("MATCH (c:Cluster) RETURN c.id").unwrap();
    assert_eq!(rows.len(), 1);
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test cluster_backend`
Expected: FAIL to compile.

- [ ] **Step 4: Add the trait method and implement on `KuzuBackend`**

In `crates/infigraph-core/src/graph/backend.rs`:

```rust
    /// Store cluster-detection results as Cluster nodes + MEMBER_OF edges.
    /// Clears any existing Cluster/MEMBER_OF data first. No default impl
    /// -- see the Global Constraints note in the implementation plan.
    fn store_clusters(
        &self,
        idx_to_id: &[String],
        community: &[usize],
        modularity: f64,
    ) -> Result<crate::cluster::ClusterStats>;
```

In `crates/infigraph-core/src/graph/kuzu_backend.rs`, move `store_clusters`'s body (from `cluster/mod.rs`) here, adapted to `&self` and propagating errors with `?` instead of discarding:

```rust
    fn store_clusters(
        &self,
        idx_to_id: &[String],
        community: &[usize],
        modularity: f64,
    ) -> Result<crate::cluster::ClusterStats> {
        self.raw_query("MATCH (s:Symbol)-[r:MEMBER_OF]->(c:Cluster) DELETE r")?;
        self.raw_query("MATCH (c:Cluster) DELETE c")?;

        let mut comm_members: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
        for (node, &comm) in community.iter().enumerate() {
            comm_members.entry(comm).or_default().push(node);
        }

        let mut cluster_sizes = Vec::new();

        for (cluster_idx, members) in comm_members.values().enumerate() {
            let cluster_id = format!("cluster_{}", cluster_idx);
            let cluster_name = format!("Cluster {}", cluster_idx);

            let mut files: Vec<&str> = Vec::new();
            for &node in members {
                let sym_id = &idx_to_id[node];
                if let Some((file, _)) = sym_id.rsplit_once("::") {
                    if !files.contains(&file) {
                        files.push(file);
                    }
                }
            }
            files.truncate(5);
            let description = format!(
                "{} symbols across files: {}",
                members.len(),
                files.join(", ")
            );

            let create_cluster = format!(
                "CREATE (c:Cluster {{id: '{}', name: '{}', description: '{}'}})",
                escape(&cluster_id),
                escape(&cluster_name),
                escape(&description),
            );
            self.raw_query(&create_cluster)?;

            for &node in members {
                let sym_id = &idx_to_id[node];
                let create_edge = format!(
                    "MATCH (s:Symbol), (c:Cluster) WHERE s.id = '{}' AND c.id = '{}' CREATE (s)-[:MEMBER_OF]->(c)",
                    escape(sym_id),
                    escape(&cluster_id),
                );
                self.raw_query(&create_edge)?;
            }

            cluster_sizes.push(members.len());
        }

        Ok(crate::cluster::ClusterStats {
            num_clusters: cluster_sizes.len(),
            cluster_sizes,
            modularity,
        })
    }
```

Check `kuzu_backend.rs`'s existing private `escape` helper (`crates/infigraph-core/src/graph/kuzu_backend.rs:48-50`) matches the one used above — reuse it rather than duplicating.

- [ ] **Step 5: Implement on `Neo4jBackend`**

Follow the same pattern as Task 8's Step 5, adapted for this method's Cypher.

- [ ] **Step 6: Update `store_clusters`'s free-function caller in `cluster/mod.rs`**

Replace the free function `store_clusters` with a call to the new trait method at its one call site (find where it's invoked — likely `detect_clusters`):

```rust
    backend.store_clusters(idx_to_id, community, modularity)
```

Delete the old free `fn store_clusters(...)` from `cluster/mod.rs` entirely (its body has moved to `kuzu_backend.rs`/`neo4j_backend.rs`).

- [ ] **Step 7: Run test to verify it passes, then run the existing cluster test suite**

Run: `cargo test -p infigraph-core --test cluster_backend`
Expected: PASS.

Run: `cargo test -p infigraph-core cluster::`
Expected: PASS (no regressions in existing `detect_clusters` tests).

- [ ] **Step 8: Add `WriteRequest::StoreClusters` variant, a `WriteResult::ClustersOk` variant, and the handler + test**

`WriteResult::Ok`'s two `usize` fields can't represent `ClusterStats { num_clusters, cluster_sizes: Vec<usize>, modularity: f64 }` without losing data — extend the enum with a dedicated variant, the same approach Task 4 used for `ScipImportOk`.

In `crates/infigraph-core/src/daemon_protocol.rs`, add the request variant:

```rust
    /// Store cluster-detection results. idx_to_id/community are already in
    /// memory on the caller's side by the time this is called -- small
    /// enough to ride inline, no sibling file needed.
    StoreClusters {
        idx_to_id: Vec<String>,
        community: Vec<usize>,
        modularity: f64,
    },
```

Extend `WriteResult` with the `ClustersOk` variant, alongside `ScipImportOk` from Task 4:

```rust
pub enum WriteResult {
    Ok {
        total_files: usize,
        indexed_files: usize,
    },
    ScipImportOk(crate::scip::ImportStats),
    ClustersOk(crate::cluster::ClusterStats),
    Err {
        message: String,
    },
}
```

Wire the handler in `serve_one_request`:

```rust
            WriteRequest::StoreClusters { idx_to_id, community, modularity } => match infigraph.backend() {
                Some(b) => match b.store_clusters(idx_to_id, community, *modularity) {
                    Ok(stats) => WriteResult::ClustersOk(stats),
                    Err(e) => WriteResult::Err { message: e.to_string() },
                },
                None => WriteResult::Err { message: "graph not initialized".to_string() },
            },
```

Add a test to `daemon_protocol_serve.rs` mirroring `store_clusters_creates_cluster_node_and_membership` (Step 2 above): index a small project, submit a `StoreClusters` request with a single-community `community` vec covering every indexed symbol, call `serve_one_request`, assert `matches!(result, WriteResult::ClustersOk(ref stats) if stats.num_clusters == 1)`, then follow up with `raw_query("MATCH (c:Cluster) RETURN c.id")` to confirm the node exists.

- [ ] **Step 9: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add crates/infigraph-core/src/graph/backend.rs crates/infigraph-core/src/graph/kuzu_backend.rs crates/infigraph-core/src/graph/neo4j_backend.rs crates/infigraph-core/src/cluster/mod.rs crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/tests/cluster_backend.rs crates/infigraph-core/tests/daemon_protocol_serve.rs
git commit -m "feat: promote store_clusters's raw Cypher writes to GraphBackend::store_clusters"
```

---

### Task 10: Promote `StoreConfigBindings` — migrate `write_config_bindings`'s raw Cypher to a trait method

**Files:**
- Modify: `crates/infigraph-core/src/graph/backend.rs`, `kuzu_backend.rs`, `neo4j_backend.rs`
- Modify: `crates/infigraph-core/src/config/mod.rs` (`write_config_bindings`, roughly lines 217-236)
- Modify: `crates/infigraph-core/src/daemon_protocol.rs`
- Test: `crates/infigraph-core/tests/config_backend.rs` (new file), `daemon_protocol_serve.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `GraphBackend::store_config_bindings(&self, bindings: &[ConfigBindingWire]) -> Result<()>`, `WriteRequest::StoreConfigBindings { bindings: Vec<ConfigBindingWire> }`.

`ConfigBinding` (`crates/infigraph-core/src/config/mod.rs:8-15`) has a `kind: &'static str` field, which cannot round-trip through `serde_json` back into an owned value without unsafe leaking. Define a small owned "wire" struct instead of trying to make `ConfigBinding` itself serializable.

- [ ] **Step 1: Define `ConfigBindingWire`**

In `crates/infigraph-core/src/config/mod.rs`, add near `ConfigBinding`:

```rust
/// Owned, serializable counterpart to `ConfigBinding` (whose `kind` field
/// is `&'static str` and can't round-trip through serde_json into an
/// owned value). Used only for the DaemonKuzu wire protocol.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConfigBindingWire {
    pub symbol_id: String,
    pub kind: String,
    pub key: String,
    pub value: String,
    pub profile: String,
    pub source_file: String,
}

impl From<&ConfigBinding> for ConfigBindingWire {
    fn from(b: &ConfigBinding) -> Self {
        Self {
            symbol_id: b.symbol_id.clone(),
            kind: b.kind.to_string(),
            key: b.key.clone(),
            value: b.value.clone(),
            profile: b.profile.clone(),
            source_file: b.source_file.clone(),
        }
    }
}
```

- [ ] **Step 2: Write the failing test**

Create `crates/infigraph-core/tests/config_backend.rs`:

```rust
use infigraph_core::config::ConfigBindingWire;
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn store_config_bindings_creates_node_and_edge() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    infigraph.index().unwrap();

    let symbols = infigraph.backend().unwrap().symbols_with_docstring(None).unwrap();
    let symbol_id = symbols.first().map(|s| s.id.clone()).unwrap_or_else(|| "nonexistent".to_string());

    let backend = infigraph.backend().unwrap();
    let bindings = vec![ConfigBindingWire {
        symbol_id: symbol_id.clone(),
        kind: "EnvVar".to_string(),
        key: "DATABASE_URL".to_string(),
        value: "postgres://...".to_string(),
        profile: "default".to_string(),
        source_file: "main.py".to_string(),
    }];
    backend.store_config_bindings(&bindings).unwrap();

    let rows = backend.raw_query("MATCH (c:ConfigBinding) RETURN c.id").unwrap();
    assert_eq!(rows.len(), 1);
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test config_backend`
Expected: FAIL to compile.

- [ ] **Step 4: Add the trait method and implement on `KuzuBackend`**

In `crates/infigraph-core/src/graph/backend.rs`:

```rust
    /// Store detected config bindings as ConfigBinding nodes + HAS_CONFIG
    /// edges. Clears existing ConfigBinding data first. No default impl --
    /// see the Global Constraints note in the implementation plan.
    fn store_config_bindings(&self, bindings: &[crate::config::ConfigBindingWire]) -> Result<()>;
```

In `crates/infigraph-core/src/graph/kuzu_backend.rs`, move `write_config_bindings`'s body here (adapted to the wire type's owned `String` fields, so no `crate::escape_str` signature changes needed — it already takes `&str`, which `String` derefs to):

```rust
    fn store_config_bindings(&self, bindings: &[crate::config::ConfigBindingWire]) -> Result<()> {
        self.raw_query("MATCH (c:ConfigBinding) DETACH DELETE c")?;

        for b in bindings {
            let id = format!("{}::{}::{}", b.symbol_id, b.kind, b.key);
            let id_esc = crate::escape_str(&id);
            let kind_esc = crate::escape_str(&b.kind);
            let key_esc = crate::escape_str(&b.key);
            let val_esc = crate::escape_str(&b.value);
            let profile_esc = crate::escape_str(&b.profile);
            let src_esc = crate::escape_str(&b.source_file);
            let sym_esc = crate::escape_str(&b.symbol_id);

            self.raw_query(&format!(
                "CREATE (c:ConfigBinding {{id: '{id_esc}', kind: '{kind_esc}', key: '{key_esc}', value: '{val_esc}', `profile`: '{profile_esc}', source_file: '{src_esc}'}})"
            ))?;
            self.raw_query(&format!(
                "MATCH (s:Symbol), (c:ConfigBinding) WHERE s.id = '{sym_esc}' AND c.id = '{id_esc}' CREATE (s)-[:HAS_CONFIG]->(c)"
            ))?;
        }

        Ok(())
    }
```

Check `crate::escape_str`'s visibility from `kuzu_backend.rs` — it's used the same way in `config/mod.rs` today (`pub(crate) fn escape_str` in `lib.rs:53`, already crate-visible), so this compiles without new exports.

- [ ] **Step 5: Implement on `Neo4jBackend`**

Same pattern as prior promotion tasks.

- [ ] **Step 6: Update `detect_config_bindings` to call the new trait method**

In `crates/infigraph-core/src/config/mod.rs`, replace the call to the old `write_config_bindings` free function with a conversion + trait-method call at its call site inside `detect_config_bindings`:

```rust
    let wire_bindings: Vec<ConfigBindingWire> = bindings.iter().map(ConfigBindingWire::from).collect();
    backend.store_config_bindings(&wire_bindings)?;
```

Delete the old free `fn write_config_bindings(...)` entirely.

- [ ] **Step 7: Run test to verify it passes, then the existing config test suite**

Run: `cargo test -p infigraph-core --test config_backend`
Expected: PASS.

Run: `cargo test -p infigraph-core config::`
Expected: PASS (no regressions in `detect_config_bindings` tests).

- [ ] **Step 8: Add `WriteRequest::StoreConfigBindings` variant + handler + test**

Mirror Task 8's Step 9-10 pattern, with `WriteRequest::StoreConfigBindings { bindings: Vec<crate::config::ConfigBindingWire> }`.

- [ ] **Step 9: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add crates/infigraph-core/src/graph/backend.rs crates/infigraph-core/src/graph/kuzu_backend.rs crates/infigraph-core/src/graph/neo4j_backend.rs crates/infigraph-core/src/config/mod.rs crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/tests/config_backend.rs crates/infigraph-core/tests/daemon_protocol_serve.rs
git commit -m "feat: promote write_config_bindings's raw Cypher writes to GraphBackend::store_config_bindings"
```

---

### Task 11: Promote `WriteCrossServiceEdges` — migrate `link_cross_service_calls`'s raw Cypher writes to a trait method

**Files:**
- Modify: `crates/infigraph-core/src/graph/backend.rs`, `kuzu_backend.rs`, `neo4j_backend.rs`
- Modify: `crates/infigraph-core/src/multi/cross_service.rs:593-618`, `:693-711`
- Modify: `crates/infigraph-core/src/daemon_protocol.rs`
- Test: `crates/infigraph-core/tests/cross_service_backend.rs` (new file), `daemon_protocol_serve.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks (reuses the Arrow IPC helper pattern from Task 7).
- Produces: `GraphBackend::write_cross_service_edges(&self, candidates: &[CrossServiceEdgeCandidate]) -> Result<usize>`, `WriteRequest::WriteCrossServiceEdges { edges_path: PathBuf }`.

`link_cross_service_calls` is more complex than the other three promotion targets: it opens a *separate* `Infigraph`/backend **per repo** in the group (each repo has its own local graph) and, for each repo, interleaves an existence check (a read) with the two writes (`MERGE` the target node, `CREATE` the edge) inside a loop over that repo's candidate service-call matches. This task promotes the per-repo write step (existence-check + merge + create, for one repo's batch of candidates) to a trait method — the caller (`link_cross_service_calls`) keeps its existing per-repo backend-opening structure unchanged, since that's a genuinely separate concern from this plan's write-routing goal. **Multi-repo daemon fan-out under `DaemonKuzu`** (each of those per-repo backends independently selecting `DaemonKuzu` and needing its own live daemon) is out of scope for this task and remains the spec's documented open question — this task's test exercises a single repo's write path only.

- [ ] **Step 1: Define `CrossServiceEdgeCandidate`**

In `crates/infigraph-core/src/graph/backend.rs`, add near `CallsServiceEdge`:

```rust
/// One candidate cross-service edge: an ExternalService target node to
/// MERGE (idempotent — safe to run group_link repeatedly) plus the
/// CALLS_SERVICE edge to CREATE if it doesn't already exist. The
/// existence check and the two writes all happen inside the backend
/// implementation, not the caller — this is why the read (the existence
/// check) is safe to route through the same daemon call as the writes:
/// server-side, it runs against the real connection, not the wrapper's
/// read-only one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CrossServiceEdgeCandidate {
    pub target_id: String,
    pub target_name: String,
    pub docstring: String,
    pub caller_symbol_id: String,
    pub method: String,
    pub path: String,
    pub target_service: String,
}
```

- [ ] **Step 2: Write the failing test**

Create `crates/infigraph-core/tests/cross_service_backend.rs`:

```rust
use infigraph_core::graph::CrossServiceEdgeCandidate;
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn write_cross_service_edges_creates_target_node_and_edge_once() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def caller():\n    pass\n",
    )
    .unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    infigraph.index().unwrap();

    let symbols = infigraph.backend().unwrap().symbols_with_docstring(None).unwrap();
    let caller_id = symbols.first().map(|s| s.id.clone()).unwrap();

    let backend = infigraph.backend().unwrap();
    let candidate = CrossServiceEdgeCandidate {
        target_id: "xsvc::payments::POST::/charge".to_string(),
        target_name: "payments POST /charge".to_string(),
        docstring: "External service: payments POST /charge".to_string(),
        caller_symbol_id: caller_id.clone(),
        method: "POST".to_string(),
        path: "/charge".to_string(),
        target_service: "payments".to_string(),
    };

    let created_first = backend.write_cross_service_edges(&[candidate.clone()]).unwrap();
    assert_eq!(created_first, 1);

    // Idempotent: running the same candidate again creates no new edge.
    let created_second = backend.write_cross_service_edges(&[candidate]).unwrap();
    assert_eq!(created_second, 0);

    let rows = backend
        .raw_query("MATCH (:Symbol)-[e:CALLS_SERVICE]->(:Symbol) RETURN e.method")
        .unwrap();
    assert_eq!(rows.len(), 1, "expected exactly one edge, not a duplicate");
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test cross_service_backend`
Expected: FAIL to compile.

- [ ] **Step 4: Add the trait method and implement on `KuzuBackend`**

In `crates/infigraph-core/src/graph/backend.rs`:

```rust
    /// Write a batch of cross-service call edges for one repo's graph.
    /// Idempotent per candidate (MERGE the target, skip the edge CREATE
    /// if it already exists). Returns the number of edges actually
    /// created (not the number of candidates). No default impl -- see the
    /// Global Constraints note in the implementation plan.
    fn write_cross_service_edges(&self, candidates: &[CrossServiceEdgeCandidate]) -> Result<usize>;
```

In `crates/infigraph-core/src/graph/kuzu_backend.rs`:

```rust
    fn write_cross_service_edges(&self, candidates: &[CrossServiceEdgeCandidate]) -> Result<usize> {
        let mut created = 0;
        for c in candidates {
            let target_id = escape(&c.target_id);
            let target_name = escape(&c.target_name);
            let docstring = escape(&c.docstring);
            let caller_sym = escape(&c.caller_symbol_id);
            let method = escape(&c.method);
            let path = escape(&c.path);
            let target_svc = escape(&c.target_service);

            let create_target = format!(
                "MERGE (t:Symbol {{id: '{target_id}'}}) \
                 ON CREATE SET t.name = '{target_name}', t.kind = 'ExternalService', \
                 t.file = '(external)', t.start_line = 0, t.end_line = 0, \
                 t.signature_hash = '', t.language = 'external', t.visibility = 'public', \
                 t.parent = '', t.docstring = '{docstring}', t.complexity = 0"
            );
            self.raw_query(&create_target)?;

            let check_edge = format!(
                "MATCH (caller:Symbol {{id: '{caller_sym}'}})-[:CALLS_SERVICE]->(target:Symbol {{id: '{target_id}'}}) RETURN caller.id"
            );
            let existing = self.raw_query(&check_edge)?;
            if !existing.is_empty() {
                continue;
            }

            let create_edge = format!(
                "MATCH (caller:Symbol {{id: '{caller_sym}'}}), (target:Symbol {{id: '{target_id}'}}) \
                 CREATE (caller)-[:CALLS_SERVICE {{method: '{method}', path: '{path}', target_service: '{target_svc}'}}]->(target)"
            );
            self.raw_query(&create_edge)?;
            created += 1;
        }
        Ok(created)
    }
```

- [ ] **Step 5: Implement on `Neo4jBackend`**

Same pattern as prior promotion tasks.

- [ ] **Step 6: Update `link_cross_service_calls` to call the new trait method**

In `crates/infigraph-core/src/multi/cross_service.rs`, replace the per-candidate loop's write logic (both the main service-call section around line 593 and the SharedPackage-linking section around line 693) to build a `Vec<CrossServiceEdgeCandidate>` per repo and call `backend.write_cross_service_edges(&candidates)` once per repo, instead of issuing `raw_query` calls per candidate inline. The existence-check read (`check_edge`) that used to happen in the caller now happens inside the trait method (Step 4) — remove the caller's own `check_edge`/`existing` logic and just collect every candidate; the trait method's idempotent MERGE-then-conditional-CREATE handles duplicates.

Keep the `total` counter (used for the function's return value / log message) working by summing what `write_cross_service_edges` returns per repo instead of incrementing per successful `raw_query`.

- [ ] **Step 7: Run test to verify it passes, then the existing cross-service test suite**

Run: `cargo test -p infigraph-core --test cross_service_backend`
Expected: PASS.

Run: `cargo test -p infigraph-core cross_service`
Expected: PASS (no regressions in `link_cross_service_calls`/`group link` tests).

- [ ] **Step 8: Add `WriteRequest::WriteCrossServiceEdges` variant + Arrow IPC handler + test**

Mirror Task 7's `WriteCallsServiceEdges` pattern exactly (Arrow IPC sibling file, same schema-building approach but with `CrossServiceEdgeCandidate`'s 7 string fields instead of 4), producing `WriteRequest::WriteCrossServiceEdges { edges_path: PathBuf }` and `write_cross_service_edges_arrow`/`read_cross_service_edges_arrow` helpers alongside `write_calls_service_edges_arrow`/`read_calls_service_edges_arrow` in `daemon_protocol.rs`.

- [ ] **Step 9: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test daemon_protocol_serve`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add crates/infigraph-core/src/graph/backend.rs crates/infigraph-core/src/graph/kuzu_backend.rs crates/infigraph-core/src/graph/neo4j_backend.rs crates/infigraph-core/src/multi/cross_service.rs crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/tests/cross_service_backend.rs crates/infigraph-core/tests/daemon_protocol_serve.rs
git commit -m "feat: promote link_cross_service_calls's raw Cypher writes to GraphBackend::write_cross_service_edges"
```

---

### Task 12: `DaemonKuzuBackend` wrapper — read-only passthrough + the DB-enforced write safety net

**Files:**
- Modify: `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs` (replace the Task 2 placeholder)
- Test: `crates/infigraph-core/tests/daemon_kuzu_backend.rs` (new file)

**Interfaces:**
- Consumes: `KuzuBackend::open_read_only` (existing, `crates/infigraph-core/src/graph/kuzu_backend.rs:34-36`), all `GraphBackend` methods it must implement.
- Produces: `DaemonKuzuBackend` fully implementing `GraphBackend` for the *read* tier and the *uncovered-write-errors* tier. Task 13 wires the *covered-write* tier (routing through `submit_write_request`).

This task's regression test is the spec's flagged open item: proving a write statement through the wrapper's read-only `raw_query` actually returns `Err` at the database level, not just at the Rust type level.

- [ ] **Step 1: Write the failing test proving read-only write-rejection**

Create `crates/infigraph-core/tests/daemon_kuzu_backend.rs`:

```rust
use infigraph_core::graph::{DaemonKuzuBackend, GraphBackend};
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn read_only_connection_rejects_write_statements() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap(); // opens direct Kuzu, creates the graph on disk
    drop(infigraph); // release the write connection so the read-only open below can succeed

    let dk = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    let result = dk.raw_query("CREATE (n:Symbol {id: 'should-not-be-written'})");

    assert!(
        result.is_err(),
        "a CREATE through the read-only connection must fail at the DB level"
    );

    // Confirm nothing was actually written -- reopen a fresh read-only
    // connection (not reusing dk, to rule out any client-side caching)
    // and check the node genuinely doesn't exist.
    let verify = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    let rows = verify
        .raw_query("MATCH (n:Symbol {id: 'should-not-be-written'}) RETURN n.id")
        .unwrap();
    assert!(rows.is_empty(), "the rejected CREATE must not have partially applied");
}

#[test]
fn read_methods_pass_through_to_a_real_connection() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    infigraph.index().unwrap();
    drop(infigraph);

    let dk = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    let stats = dk.stats().unwrap();
    assert!(stats.symbol_count > 0, "expected real read access to the already-indexed graph");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test daemon_kuzu_backend`
Expected: FAIL to compile — `DaemonKuzuBackend` doesn't implement `GraphBackend` yet (Task 2's placeholder deliberately doesn't), and `open` doesn't open a real read-only connection.

- [ ] **Step 3: Implement `DaemonKuzuBackend`'s read tier and open a real read-only connection**

Replace `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs`'s contents entirely:

```rust
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use crate::learned::LearnedStore;
use crate::model::FileExtraction;
use crate::resolve::ResolveStats;

use super::backend::{CallsServiceEdge, CrossServiceEdgeCandidate, GraphBackend};
use super::kuzu_backend::KuzuBackend;
use super::{
    ApiSymbol, ArchitectureStats, BranchInfo, ComplexityRow, DeadCodeRow, FileDeps, GraphStats,
    ImpactRow, ReferenceRow, SymbolDetail, SymbolMeta, SymbolRow, SymbolWithDocstring, TestContext,
    TestCoverage, TypeHierarchy,
};

/// Routes writes through the DaemonKuzu file-drop protocol instead of
/// opening a direct embedded Kuzu connection. See
/// docs/superpowers/specs/2026-08-01-daemonkuzu-daemon-wiring-design.md.
///
/// Three-tier contract:
/// 1. Reads delegate to `read_conn`, a real directly-opened read-only
///    Kuzu connection -- reads never route through the daemon.
/// 2. The write methods covered by WriteRequest (see daemon_protocol.rs)
///    route through submit_write_request (Task 13).
/// 3. Any other write method returns a clear error rather than silently
///    writing through read_conn (which would fail at the DB level, per
///    read_only_connection_rejects_write_statements) or reintroducing a
///    real collision some other way.
pub struct DaemonKuzuBackend {
    read_conn: KuzuBackend,
    root: std::path::PathBuf,
}

impl DaemonKuzuBackend {
    pub fn open(root: &Path) -> Result<Self> {
        let db_path = root.join(".infigraph").join("graph");
        let read_conn = KuzuBackend::open_read_only(&db_path)?;
        Ok(Self {
            read_conn,
            root: root.to_path_buf(),
        })
    }

    fn not_supported(method: &str, alternative: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "not supported via direct backend access under DaemonKuzu -- use {alternative} instead ({method})"
        )
    }
}

impl GraphBackend for DaemonKuzuBackend {
    // ── Tier 1: reads pass through to the real read-only connection ──

    fn stats(&self) -> Result<GraphStats> { self.read_conn.stats() }
    fn get_file_hashes(&self) -> Result<HashMap<String, String>> { self.read_conn.get_file_hashes() }
    fn get_all_symbols(&self) -> Result<Vec<(String, String, String, String)>> { self.read_conn.get_all_symbols() }
    fn symbols_in_file(&self, file: &str) -> Result<Vec<SymbolRow>> { self.read_conn.symbols_in_file(file) }
    fn find_symbol_by_id(&self, id: &str) -> Result<Option<SymbolDetail>> { self.read_conn.find_symbol_by_id(id) }
    fn symbols_in_range(&self, file: &str, start: u32, end: u32) -> Result<Vec<SymbolDetail>> { self.read_conn.symbols_in_range(file, start, end) }
    fn skeleton(&self, file: &str) -> Result<String> { self.read_conn.skeleton(file) }
    fn callers_of(&self, symbol_id: &str) -> Result<Vec<String>> { self.read_conn.callers_of(symbol_id) }
    fn callees_of(&self, symbol_id: &str) -> Result<Vec<String>> { self.read_conn.callees_of(symbol_id) }
    fn branches_of(&self, symbol_id: &str) -> Result<Vec<BranchInfo>> { self.read_conn.branches_of(symbol_id) }
    fn transitive_impact(&self, id: &str, max_depth: u32) -> Result<Vec<ImpactRow>> { self.read_conn.transitive_impact(id, max_depth) }
    fn find_all_references(&self, id: &str) -> Result<Vec<ReferenceRow>> { self.read_conn.find_all_references(id) }
    fn cross_cutting_for(&self, id: &str) -> Result<Vec<(String, String)>> { self.read_conn.cross_cutting_for(id) }
    fn get_api_surface(&self) -> Result<Vec<ApiSymbol>> { self.read_conn.get_api_surface() }
    fn get_file_deps(&self, file: &str) -> Result<FileDeps> { self.read_conn.get_file_deps(file) }
    fn get_type_hierarchy(&self, id: &str, max_depth: u32) -> Result<TypeHierarchy> { self.read_conn.get_type_hierarchy(id, max_depth) }
    fn get_test_coverage(&self) -> Result<TestCoverage> { self.read_conn.get_test_coverage() }
    fn generate_test_context(&self, file_filter: Option<&str>, limit: usize, test_type: Option<&str>) -> Result<TestContext> {
        self.read_conn.generate_test_context(file_filter, limit, test_type)
    }
    fn raw_query(&self, query: &str) -> Result<Vec<Vec<String>>> { self.read_conn.raw_query(query) }
    fn get_symbols_for_search(&self) -> Result<Vec<Vec<String>>> { self.read_conn.get_symbols_for_search() }
    fn symbol_metadata(&self, id: &str) -> Result<Option<SymbolMeta>> { self.read_conn.symbol_metadata(id) }
    fn get_complexity_ranking(&self, file_filter: Option<&str>) -> Result<Vec<ComplexityRow>> { self.read_conn.get_complexity_ranking(file_filter) }
    fn list_indexed_files(&self) -> Result<Vec<String>> { self.read_conn.list_indexed_files() }
    fn find_uncalled_symbols(&self) -> Result<Vec<DeadCodeRow>> { self.read_conn.find_uncalled_symbols() }
    fn get_architecture_stats(&self) -> Result<ArchitectureStats> { self.read_conn.get_architecture_stats() }
    fn symbols_with_docstring(&self, kind_filter: Option<&[&str]>) -> Result<Vec<SymbolWithDocstring>> {
        self.read_conn.symbols_with_docstring(kind_filter)
    }
    fn repo_filter(&self) -> Option<&str> { self.read_conn.repo_filter() }

    // ── Tier 3 placeholders: Task 13 replaces each of these with a Tier 2
    //    submit_write_request call. Left as loud errors here so this task
    //    compiles as a complete GraphBackend impl on its own. ──

    fn upsert_similar_edge(&self, _id_a: &str, _id_b: &str, _score: f32) -> Result<()> {
        Err(Self::not_supported("upsert_similar_edge", "Infigraph's daemon protocol (wired in Task 13)"))
    }
    fn upsert_file(&self, _extraction: &FileExtraction) -> Result<()> {
        Err(Self::not_supported("upsert_file", "Infigraph::index()/index_files()"))
    }
    fn upsert_files_bulk(&self, _extractions: &[FileExtraction], _existing_hashes_empty: bool) -> Result<()> {
        Err(Self::not_supported("upsert_files_bulk", "Infigraph::index()/index_files()"))
    }
    fn remove_file(&self, _file: &str) -> Result<()> {
        Err(Self::not_supported("remove_file", "Infigraph::index()/index_files() (internal only)"))
    }
    fn derive_tested_by_edges(&self, _changed_files: Option<&[&str]>) -> Result<usize> {
        Err(Self::not_supported("derive_tested_by_edges", "Infigraph's daemon protocol (wired in Task 13)"))
    }
    fn upsert_repo(&self, _repo_name: &str) -> Result<()> {
        // Deliberately overridden (not left as the trait's no-op default)
        // -- see Task 6's warning about the inherited-default trap.
        Err(Self::not_supported("upsert_repo", "Infigraph's daemon protocol (wired in Task 13)"))
    }
    fn write_calls_service_edges(&self, _edges: &[CallsServiceEdge]) -> Result<()> {
        Err(Self::not_supported("write_calls_service_edges", "Infigraph's daemon protocol (wired in Task 13)"))
    }
    fn resolve_calls(&self, _extractions: &[FileExtraction], _learned: Option<&LearnedStore>) -> Result<ResolveStats> {
        Err(Self::not_supported("resolve_calls", "Infigraph::index()/index_files() (internal only)"))
    }
    fn re_resolve_for_files(&self, _files: &[String], _extractions: &[FileExtraction], _learned: Option<&LearnedStore>) -> Result<ResolveStats> {
        Err(Self::not_supported("re_resolve_for_files", "Infigraph::index()/index_files() (internal only)"))
    }
    fn import_scip_index(&self, _index_path: &Path, _project_root: Option<&Path>) -> Result<crate::scip::ImportStats> {
        Err(Self::not_supported("import_scip_index", "Infigraph's daemon protocol (wired in Task 13)"))
    }
    fn ingest_structured_data(&self, _schema: &crate::structured::SchemaMeta, _data: &[serde_json::Value]) -> Result<crate::structured::IngestResult> {
        Err(Self::not_supported("ingest_structured_data", "Infigraph's daemon protocol (wired in Task 13)"))
    }
    fn ingest_structured_file(&self, _schema: &crate::structured::SchemaMeta, _path: &Path) -> Result<crate::structured::IngestResult> {
        Err(Self::not_supported("ingest_structured_file", "Infigraph's daemon protocol (wired in Task 13)"))
    }
    fn ingest_structured_directory(&self, _schema: &crate::structured::SchemaMeta, _dir: &Path) -> Result<crate::structured::IngestResult> {
        Err(Self::not_supported("ingest_structured_directory", "Infigraph's daemon protocol (wired in Task 13)"))
    }
    fn upsert_dependencies(&self, _result: &crate::manifest::ManifestResult) -> Result<()> {
        Err(Self::not_supported("upsert_dependencies", "Infigraph's daemon protocol (wired in Task 13)"))
    }
    fn store_clusters(&self, _idx_to_id: &[String], _community: &[usize], _modularity: f64) -> Result<crate::cluster::ClusterStats> {
        Err(Self::not_supported("store_clusters", "Infigraph's daemon protocol (wired in Task 13)"))
    }
    fn store_config_bindings(&self, _bindings: &[crate::config::ConfigBindingWire]) -> Result<()> {
        Err(Self::not_supported("store_config_bindings", "Infigraph's daemon protocol (wired in Task 13)"))
    }
    fn write_cross_service_edges(&self, _candidates: &[CrossServiceEdgeCandidate]) -> Result<usize> {
        Err(Self::not_supported("write_cross_service_edges", "Infigraph's daemon protocol (wired in Task 13)"))
    }
}
```

Note: this lists every `GraphBackend` trait method explicitly (no reliance on defaults for `clear_all_data`/`upsert_repo`/`repo_filter`, even where the trait has a default) so the wrapper's behavior for each method is a deliberate choice, not an accident of trait default inheritance — `repo_filter` legitimately delegates to `read_conn` (it's a read), `upsert_repo` deliberately does NOT use the trait's no-op default (per Task 6's warning), and `clear_all_data` is omitted here because the trait's own default (a no-op, `backend.rs:124-126`) is the correct behavior for `DaemonKuzu` too (matches `KuzuBackend`'s own reliance on the same default) — leaving it un-overridden is a deliberate choice, not an oversight; add a one-line comment above the `impl GraphBackend for DaemonKuzuBackend` block noting this explicitly so a future reader doesn't mistake the omission for a gap.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p infigraph-core --test daemon_kuzu_backend`
Expected: PASS, both tests. If `read_only_connection_rejects_write_statements` still passes (i.e. the `CREATE` doesn't error), this is a load-bearing finding for the whole design — stop and re-read the spec's Open Questions section before proceeding to Task 13, since the entire safety-net argument depends on this test's assertion being true. Do not weaken the assertion to make it pass; if Kuzu's read-only mode doesn't reject writes at the DB level, that's a real problem requiring a design conversation, not a test to relax.

- [ ] **Step 5: Update `BackendKind::DaemonKuzu`'s construction in `lib.rs` (from Task 2) to use the real type**

Task 2's `graph::DaemonKuzuBackend::open(&self.root)` call already matches this task's real `open` signature — no change needed there. Update `backend()`'s match arm (Task 2, Step 3) from returning `None` to returning `Some`:

```rust
            BackendKind::DaemonKuzu(dk) => Some(dk),
```

- [ ] **Step 6: Run the full backend_selection test suite to confirm no regression**

Run: `cargo test -p infigraph-core --test backend_selection`
Expected: PASS, all three tests (from Task 2) still pass with the real wrapper now wired in.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-core/src/graph/daemon_kuzu_backend.rs crates/infigraph-core/src/lib.rs crates/infigraph-core/tests/daemon_kuzu_backend.rs
git commit -m "feat: implement DaemonKuzuBackend's read-only passthrough and DB-enforced write safety net"
```

---

### Task 13: Wire the 10 covered write methods into the wrapper

**Files:**
- Modify: `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs`
- Modify: `crates/infigraph-core/src/daemon_protocol.rs` (`submit_write_request` needs to expose its generated request path for the `IngestStructured::Inline`/Arrow-sibling-file cases)
- Test: `crates/infigraph-core/tests/daemon_kuzu_backend.rs`

**Interfaces:**
- Consumes: every `WriteRequest` variant from Tasks 4-11, `submit_write_request`.
- Produces: the fully-wired `DaemonKuzuBackend` — this is the task where the whole plan's pieces actually connect end-to-end.

Task 5's Step 7 flagged that `submit_write_request` generates its request path internally, which doesn't work for the two variants needing a sibling file written *before* the request. Resolve this by adding a small variant of `submit_write_request` that takes a pre-built request path.

- [ ] **Step 1: Add `submit_write_request_at` for the sibling-file cases**

In `crates/infigraph-core/src/daemon_protocol.rs`, refactor `submit_write_request` to share its polling logic with a new function that accepts an already-decided name:

```rust
/// Generates a unique request name the same way submit_write_request does
/// internally, without writing anything -- used by callers that need to
/// write a sibling file (Arrow IPC edges, inline ingest data) before the
/// request file itself, using the same generated name.
pub fn generate_request_name() -> String {
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        counter
    )
}

/// Same as submit_write_request, but the request/result file names are
/// pre-determined (via generate_request_name) rather than generated
/// internally -- lets a caller write a sibling file first, using the same
/// name, before the request file that references it exists.
pub fn submit_write_request_named(
    staging_dir: &Path,
    name: &str,
    request: &WriteRequest,
    timeout: Duration,
) -> anyhow::Result<WriteResult> {
    std::fs::create_dir_all(staging_dir)?;
    let request_path = staging_dir.join(format!("{name}.request"));
    let result_path = staging_dir.join(format!("{name}.result"));

    write_atomic(&request_path, &serde_json::to_string(request)?)?;

    let start = Instant::now();
    let mut delay = Duration::from_millis(10);
    loop {
        if result_path.exists() {
            let contents = std::fs::read_to_string(&result_path)?;
            std::fs::remove_file(&result_path).ok();
            return Ok(serde_json::from_str(&contents)?);
        }
        if start.elapsed() >= timeout {
            std::fs::remove_file(&request_path).ok();
            anyhow::bail!(
                "no daemon responded to write request within {:?} ({})",
                timeout,
                request_path.display()
            );
        }
        std::thread::sleep(delay.min(timeout.saturating_sub(start.elapsed())));
        delay = (delay * 2).min(Duration::from_millis(200));
    }
}
```

Refactor `submit_write_request` itself to call `submit_write_request_named` with a freshly-generated name, so the polling logic lives in exactly one place:

```rust
pub fn submit_write_request(
    staging_dir: &Path,
    request: &WriteRequest,
    timeout: Duration,
) -> anyhow::Result<WriteResult> {
    let name = generate_request_name();
    submit_write_request_named(staging_dir, &name, request, timeout)
}
```

- [ ] **Step 2: Run the existing daemon_protocol unit tests to confirm the refactor didn't break anything**

Run: `cargo test -p infigraph-core daemon_protocol::`
Expected: PASS, unchanged (this refactor is behavior-preserving for `submit_write_request`'s existing callers).

- [ ] **Step 3: Write the failing tests for the wrapper's covered writes**

Add to `crates/infigraph-core/tests/daemon_kuzu_backend.rs` — one representative test per write tier is enough here (the individual `WriteRequest` handlers already have their own tests from Tasks 4-11; this task's tests confirm the *wrapper* correctly builds and submits the request):

```rust
#[test]
fn wrapper_upsert_repo_routes_through_daemon_protocol() {
    let project_dir = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();
    drop(infigraph);

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();

    // Simulate the daemon: a background thread serves the one request this
    // test's upsert_repo call will submit.
    let served_root = project_dir.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        let registry = bundled_registry().unwrap();
        let mut server_infigraph = Infigraph::open(&served_root, registry).unwrap();
        server_infigraph.init().unwrap();
        let staging_dir = served_root.join(".infigraph").join("requests");
        let start = std::time::Instant::now();
        loop {
            if let Ok(entries) = std::fs::read_dir(&staging_dir) {
                for entry in entries.flatten() {
                    if entry.path().extension().is_some_and(|e| e == "request") {
                        infigraph_core::daemon_protocol::serve_one_request(&server_infigraph, &entry.path()).unwrap();
                        return;
                    }
                }
            }
            if start.elapsed() > std::time::Duration::from_secs(5) {
                panic!("test daemon never saw a request");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });

    let dk = DaemonKuzuBackend::open(project_dir.path()).unwrap();
    dk.upsert_repo("org/repo").unwrap();

    handle.join().unwrap();
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test daemon_kuzu_backend wrapper_upsert_repo_routes_through_daemon_protocol`
Expected: FAIL — `dk.upsert_repo` currently returns the Task 12 placeholder's `not_supported` error, not routing through `submit_write_request`.

- [ ] **Step 5: Wire the simple (non-Arrow) covered writes**

In `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs`, replace the placeholder implementations for `upsert_repo`, `derive_tested_by_edges`, `upsert_similar_edge`, `upsert_dependencies`, `store_clusters`, `store_config_bindings`, and `import_scip_index` with real routing. Each follows the same shape — build the matching `WriteRequest`, submit it via `crate::daemon_protocol::submit_write_request`, translate `WriteResult` back:

```rust
    fn upsert_repo(&self, repo_name: &str) -> Result<()> {
        let staging_dir = self.root.join(".infigraph").join("requests");
        let request = crate::daemon_protocol::WriteRequest::UpsertRepo {
            namespace: repo_name.to_string(),
        };
        match crate::daemon_protocol::submit_write_request(&staging_dir, &request, std::time::Duration::from_secs(30))? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!("unexpected WriteResult for UpsertRepo: {other:?}")),
        }
    }

    fn derive_tested_by_edges(&self, changed_files: Option<&[&str]>) -> Result<usize> {
        let staging_dir = self.root.join(".infigraph").join("requests");
        let request = crate::daemon_protocol::WriteRequest::DeriveTestedBy {
            files: changed_files.map(|files| files.iter().map(|s| s.to_string()).collect()),
        };
        match crate::daemon_protocol::submit_write_request(&staging_dir, &request, std::time::Duration::from_secs(60))? {
            crate::daemon_protocol::WriteResult::Ok { indexed_files, .. } => Ok(indexed_files),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!("unexpected WriteResult for DeriveTestedBy: {other:?}")),
        }
    }

    fn upsert_similar_edge(&self, id_a: &str, id_b: &str, score: f32) -> Result<()> {
        let staging_dir = self.root.join(".infigraph").join("requests");
        let request = crate::daemon_protocol::WriteRequest::UpsertSimilarEdge {
            id_a: id_a.to_string(),
            id_b: id_b.to_string(),
            score,
        };
        match crate::daemon_protocol::submit_write_request(&staging_dir, &request, std::time::Duration::from_secs(30))? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!("unexpected WriteResult for UpsertSimilarEdge: {other:?}")),
        }
    }

    fn upsert_dependencies(&self, result: &crate::manifest::ManifestResult) -> Result<()> {
        let staging_dir = self.root.join(".infigraph").join("requests");
        let request = crate::daemon_protocol::WriteRequest::UpsertDependencies { result: result.clone() };
        match crate::daemon_protocol::submit_write_request(&staging_dir, &request, std::time::Duration::from_secs(30))? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!("unexpected WriteResult for UpsertDependencies: {other:?}")),
        }
    }

    fn store_clusters(&self, idx_to_id: &[String], community: &[usize], modularity: f64) -> Result<crate::cluster::ClusterStats> {
        let staging_dir = self.root.join(".infigraph").join("requests");
        let request = crate::daemon_protocol::WriteRequest::StoreClusters {
            idx_to_id: idx_to_id.to_vec(),
            community: community.to_vec(),
            modularity,
        };
        match crate::daemon_protocol::submit_write_request(&staging_dir, &request, std::time::Duration::from_secs(30))? {
            crate::daemon_protocol::WriteResult::ClustersOk(stats) => Ok(stats),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!("unexpected WriteResult for StoreClusters: {other:?}")),
        }
    }

    fn store_config_bindings(&self, bindings: &[crate::config::ConfigBindingWire]) -> Result<()> {
        let staging_dir = self.root.join(".infigraph").join("requests");
        let request = crate::daemon_protocol::WriteRequest::StoreConfigBindings { bindings: bindings.to_vec() };
        match crate::daemon_protocol::submit_write_request(&staging_dir, &request, std::time::Duration::from_secs(30))? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!("unexpected WriteResult for StoreConfigBindings: {other:?}")),
        }
    }

    fn import_scip_index(&self, index_path: &Path, _project_root: Option<&Path>) -> Result<crate::scip::ImportStats> {
        let staging_dir = self.root.join(".infigraph").join("requests");
        let request = crate::daemon_protocol::WriteRequest::ScipImport { scip_path: index_path.to_path_buf() };
        match crate::daemon_protocol::submit_write_request(&staging_dir, &request, std::time::Duration::from_secs(120))? {
            crate::daemon_protocol::WriteResult::ScipImportOk(stats) => Ok(stats),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!("unexpected WriteResult for ScipImport: {other:?}")),
        }
    }
```

`store_clusters` and `import_scip_index` return their real structs directly via the `ClustersOk`/`ScipImportOk` variants added in Tasks 9 and 4 — no lossy remapping onto `total_files`/`indexed_files`. Every match above adds a trailing `other => Err(...)` arm because `WriteResult` now has four variants (`Ok`, `ScipImportOk`, `ClustersOk`, `Err`); a given request kind only ever produces one specific non-`Err` variant in practice, but the compiler still requires each `match` to be exhaustive over the whole enum.

- [ ] **Step 6: Wire the Arrow-sibling-file covered writes (`write_calls_service_edges`, `write_cross_service_edges`, `ingest_structured_*`)**

These need `submit_write_request_named` from Step 1 so the sibling file's name matches the request's generated name:

```rust
    fn write_calls_service_edges(&self, edges: &[CallsServiceEdge]) -> Result<()> {
        if edges.is_empty() {
            return Ok(());
        }
        let staging_dir = self.root.join(".infigraph").join("requests");
        std::fs::create_dir_all(&staging_dir)?;
        let name = crate::daemon_protocol::generate_request_name();
        let edges_path = staging_dir.join(format!("{name}.edges.arrow"));
        crate::daemon_protocol::write_calls_service_edges_arrow(&edges_path, edges)?;

        let request = crate::daemon_protocol::WriteRequest::WriteCallsServiceEdges { edges_path: edges_path.clone() };
        match crate::daemon_protocol::submit_write_request_named(&staging_dir, &name, &request, std::time::Duration::from_secs(60)) {
            Ok(crate::daemon_protocol::WriteResult::Ok { .. }) => Ok(()),
            Ok(crate::daemon_protocol::WriteResult::Err { message }) => Err(anyhow::anyhow!(message)),
            Ok(other) => Err(anyhow::anyhow!("unexpected WriteResult for WriteCallsServiceEdges: {other:?}")),
            Err(e) => {
                std::fs::remove_file(&edges_path).ok(); // clean up on timeout -- the daemon never consumed it
                Err(e)
            }
        }
    }
```

Apply the same pattern to `write_cross_service_edges` (using `write_cross_service_edges_arrow` from Task 11) and to `ingest_structured_data`/`_file`/`_directory` (using `write_ingest_inline_sibling` from Task 5 for the `_data` case only; `_file`/`_directory` need no sibling file, just `WriteRequest::IngestStructured { source: IngestSource::File(...)/Directory(...) }` via plain `submit_write_request`). For `ingest_structured_data(&self, schema: &SchemaMeta, data: &[serde_json::Value])`, use `schema.schema_id` to build the request's `schema_id` field — the wrapper never serializes the full `SchemaMeta` itself, per the spec's design.

- [ ] **Step 7: Run test to verify it passes**

Run: `cargo test -p infigraph-core --test daemon_kuzu_backend wrapper_upsert_repo_routes_through_daemon_protocol`
Expected: PASS.

- [ ] **Step 8: Add one wrapper-level test per remaining covered write, following the same background-thread-serves-one-request pattern as Step 3**

At minimum, add tests for: `derive_tested_by_edges`, `upsert_similar_edge`, `write_calls_service_edges` (confirm the Arrow sibling file is cleaned up after serving, same assertion style as the direct handler test in Task 7), and `ingest_structured_data` with `Inline` data (confirm its sibling file is cleaned up too). Reuse the background-serving-thread helper structure from Step 3 rather than duplicating it per test — factor it into a private test-only helper function in `daemon_kuzu_backend.rs`'s test file if it's copy-pasted more than twice.

- [ ] **Step 9: Run the full wrapper test suite**

Run: `cargo test -p infigraph-core --test daemon_kuzu_backend`
Expected: PASS, all tests.

- [ ] **Step 10: Commit**

```bash
git add crates/infigraph-core/src/graph/daemon_kuzu_backend.rs crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/tests/daemon_kuzu_backend.rs
git commit -m "feat: wire all covered write methods into the DaemonKuzu wrapper"
```

---

### Task 14: End-to-end verification — `infigraph daemon` + `DaemonKuzu` client, real process

**Files:**
- Test: `crates/infigraph-core/tests/daemon_kuzu_e2e.rs` (new file)

**Interfaces:**
- Consumes: everything from Tasks 1-13.
- Produces: nothing for later tasks — this is the plan's final proof that a real spawned `infigraph daemon` process and a real `DaemonKuzu`-backed client `Infigraph` genuinely interoperate, not just the in-process simulated-server tests Tasks 4-13 used.

- [ ] **Step 1: Write the end-to-end test**

Create `crates/infigraph-core/tests/daemon_kuzu_e2e.rs`:

```rust
use std::process::Command;
use std::time::Duration;

#[test]
fn real_daemon_process_serves_a_daemon_kuzu_client() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    // Bootstrap: index once directly (BackendKind::Kuzu, no daemon
    // involved) so .infigraph/ exists before starting the daemon.
    let status = Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .arg("index")
        .current_dir(project_dir.path())
        .env_remove("INFIGRAPH_BACKEND")
        .status()
        .unwrap();
    assert!(status.success(), "bootstrap index failed");

    // Start the real daemon as a detached child.
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .arg("daemon")
        .current_dir(project_dir.path())
        .env_remove("INFIGRAPH_BACKEND")
        .spawn()
        .unwrap();

    // Wait for it to acquire watch.lock (proves it started successfully).
    let lock_path = project_dir.path().join(".infigraph").join("watch.lock");
    let start = std::time::Instant::now();
    loop {
        if lock_path.exists() && std::fs::metadata(&lock_path).unwrap().len() > 0 {
            break;
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = daemon.kill();
            panic!("daemon never acquired watch.lock");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // A DaemonKuzu-backed client submits a write request against the same
    // project the real daemon process is watching.
    let registry = infigraph_languages::bundled_registry().unwrap();
    let mut client = infigraph_core::Infigraph::open(project_dir.path(), registry).unwrap();
    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let init_result = client.init();
    std::env::remove_var("INFIGRAPH_BACKEND");
    init_result.unwrap();

    std::fs::write(
        project_dir.path().join("second.py"),
        "def another():\n    pass\n",
    )
    .unwrap();

    let backend = client.backend().unwrap();
    // upsert_repo is small and side-effect-observable via raw_query on a
    // *separate* direct read-only connection below, without needing to
    // wait for the file watcher's own debounce/reindex cycle.
    backend.upsert_repo("e2e-test/project").unwrap();

    // Verify via a fresh, independent read-only connection (not the
    // client's own wrapper) that the real daemon process actually
    // performed the write.
    let verify_registry = infigraph_languages::bundled_registry().unwrap();
    let mut verify = infigraph_core::Infigraph::open(project_dir.path(), verify_registry).unwrap();
    verify.init().unwrap(); // BackendKind::Kuzu -- but the daemon holds the write connection...
```

This last step needs care: opening a second *direct* `Kuzu` connection while the daemon holds its own would collide (that's the exact problem this whole design exists to prevent) — use the wrapper's own read path instead, which is what a real `DaemonKuzu`-backed reader would do, or use `KuzuBackend::open_read_only` directly (the same primitive `DaemonKuzuBackend` uses internally):

```rust
    let verify = infigraph_core::graph::KuzuBackend::open_read_only(
        &project_dir.path().join(".infigraph").join("graph"),
    ).unwrap();
    let rows = verify
        .raw_query("MATCH (r:Repo {name: 'e2e-test/project'}) RETURN r.name")
        .unwrap();
    assert_eq!(rows.len(), 1, "expected the daemon to have created the Repo node");

    // Clean up: stop the daemon via its sentinel file.
    std::fs::write(project_dir.path().join(".infigraph").join("watch.stop"), "").unwrap();
    let _ = daemon.wait_timeout_or_kill(Duration::from_secs(5));
}
```

`wait_timeout_or_kill` isn't a real `std::process::Child` method — replace the last two lines with a simple bounded wait-then-kill loop:

```rust
    std::fs::write(project_dir.path().join(".infigraph").join("watch.stop"), "").unwrap();
    let stop_start = std::time::Instant::now();
    loop {
        if let Ok(Some(_)) = daemon.try_wait() {
            break;
        }
        if stop_start.elapsed() > Duration::from_secs(5) {
            let _ = daemon.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
```

Note `upsert_repo` was chosen for this end-to-end test specifically because `Repo`/`upsert_repo` has no other write path that could create it as a side effect (unlike `Index`, whose end-to-end round trip is already covered by Task 3's test) — this makes the assertion unambiguous proof that the specific write went through the real daemon.

- [ ] **Step 2: Run test to verify it fails naturally, then passes once everything from Tasks 1-13 is in place**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test daemon_kuzu_e2e -- --test-threads=1`
Expected: This test should already PASS at this point in the plan, since it exercises only functionality Tasks 1-13 already built and verified individually — this task's job is proving the *composition* works with a real spawned process, not introducing new behavior. If it fails, the failure points at an integration gap between two tasks' pieces that their individual tests didn't catch (e.g. a path-resolution mismatch between the CLI's `current_dir` and `Infigraph::open`'s root canonicalization) — debug and fix the root cause rather than adjusting the test's assertions to match broken behavior.

- [ ] **Step 3: Commit**

```bash
git add crates/infigraph-core/tests/daemon_kuzu_e2e.rs
git commit -m "test: end-to-end verification of a real daemon process serving a DaemonKuzu client"
```

---

### Task 15: Fix remaining error-swallowing (`upsert_similar_edge` discards in `cmd_clones`/`tool_detect_clones`)

**Files:**
- Modify: `crates/infigraph-cli/src/analysis_commands.rs:503` (`cmd_clones`)
- Modify: `crates/infigraph-mcp/src/tools/analysis/clones.rs` (`tool_detect_clones`'s `store_edges` block)

**Interfaces:**
- Consumes: nothing new.
- Produces: nothing for later tasks — this is the plan's last remaining item from the spec's "Call-site changes required" section (the `manifest`/`cross_service` discards were already fixed as part of Tasks 8 and 11's migrations).

- [ ] **Step 1: Fix `cmd_clones`'s discard**

In `crates/infigraph-cli/src/analysis_commands.rs`, around line 503:

```rust
    for (score, i, j) in &pairs {
        let _ = backend.upsert_similar_edge(&symbol_vecs[*i].0, &symbol_vecs[*j].0, *score);
    }
```

becomes:

```rust
    for (score, i, j) in &pairs {
        if let Err(e) = backend.upsert_similar_edge(&symbol_vecs[*i].0, &symbol_vecs[*j].0, *score) {
            eprintln!(
                "warning: failed to store similarity edge {} <-> {}: {e}",
                symbol_vecs[*i].0, symbol_vecs[*j].0
            );
        }
    }
```

- [ ] **Step 2: Fix `tool_detect_clones`'s discard**

Read `crates/infigraph-mcp/src/tools/analysis/clones.rs` to find its `store_edges` block's exact current discard pattern (it mirrors `cmd_clones`'s CLI logic), and apply the same fix — replace `let _ = backend.upsert_similar_edge(...)` with an `if let Err(e) = ...` that at minimum logs (via whatever this file's existing warning-logging convention is — check nearby code in the same file for the established pattern, e.g. `eprintln!` vs a structured log call, and match it).

- [ ] **Step 3: Run the existing clone-detection test suite to confirm no regressions**

Run: `cargo test -p infigraph-cli -p infigraph-mcp clones`
Expected: PASS (this is a pure error-surfacing change — no behavior change on the success path).

- [ ] **Step 4: Commit**

```bash
git add crates/infigraph-cli/src/analysis_commands.rs crates/infigraph-mcp/src/tools/analysis/clones.rs
git commit -m "fix: surface upsert_similar_edge failures as warnings instead of discarding them"
```

---

### Task 16: Full workspace verification

**Files:** none (verification task)

- [ ] **Step 1: Run the full workspace test suite**

This machine has two known environmental gotchas (from the prior merged daemonkuzu-file-drop-protocol plan's Task 6 — see its ledger at `.superpowers/sdd/2026-07-31-daemonkuzu-file-drop-protocol/progress.md` for the full history):

(a) Never set `CARGO_TARGET_DIR` explicitly. `scratchpad/.cargo/config.toml` already points `target-dir` at the repo's shared absolute path for every `scratchpad/wt-*` worktree — a relative `CARGO_TARGET_DIR=.shared-target` override (this plan originally used that convention throughout; it has since been corrected) resolves against whatever directory the command runs from, so under a worktree it silently creates a second, duplicate target dir instead of preventing one. Just run plain `cargo`/`cargo test` and let the ambient config resolve it.

(b) This machine's `~/.zshrc` exports `INFIGRAPH_WATCH_DAEMON=1` globally, which breaks `watcher_concurrency.rs`/`watcher_reindex.rs` tests unless the run is prefixed with `env -u INFIGRAPH_WATCH_DAEMON`.

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test --workspace --no-fail-fast`
Expected: PASS. If a failure looks unrelated to this plan's changes (e.g. the same Kuzu mmap-exhaustion-under-full-parallelism or cross-test-pollution patterns documented in the prior plan's Task 6 ledger), rerun that specific test/file in isolation (`cargo test -p <crate> --test <file> -- --test-threads=1` or similar) before treating it as a real regression — if it passes in isolation, it's environmental, not a defect in this plan's code.

- [ ] **Step 2: Run `cargo fmt` and `cargo clippy`**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo fmt -- --check && env -u INFIGRAPH_WATCH_DAEMON cargo clippy --all-targets -- -D warnings`
Expected: clean, no warnings.

- [ ] **Step 3: Verify the Write Coverage Audit's promises actually hold**

Run a final manual check (not a new automated test — this is a documentation-accuracy check): confirm every row of the spec's Table A and Table B (`docs/superpowers/specs/2026-08-01-daemonkuzu-daemon-wiring-design.md`) has a corresponding `WriteRequest` variant that this plan implemented, and that Task 12's `DaemonKuzuBackend` impl has no `unimplemented!()`/placeholder-`not_supported` left for any of the 10 covered methods (only genuinely uncovered methods like `upsert_file`/`upsert_files_bulk`/`resolve_calls`/`re_resolve_for_files`/`remove_file` should still return the loud error, matching the spec's "ruled out, internal-only" list).

- [ ] **Step 4: Commit any final fixes, then stop — this plan's scope ends here**

If Steps 1-3 are clean, there's nothing to commit. This plan deliberately does not migrate any call site beyond what's enumerated in the Write Coverage Audit, does not address the orphaned-result-file cleanup gap (spec's Open Questions), and does not validate multi-repo daemon fan-out for `group link`/`group build` under `DaemonKuzu` (Task 11's explicit scope boundary) — all three remain follow-ups for whoever picks up production daemon operation next.
