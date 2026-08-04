# Daemon-Routed Full Reindex Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `infigraph index --full` actually work under `INFIGRAPH_BACKEND=daemon` instead of refusing, by having the daemon build a fresh graph at a side path and atomically swap it in — closing GitHub issue #50's real fix (the mitigation is already merged and closed).

**Architecture:** Build-fresh-then-swap. A new `WriteRequest::FullReindex` variant is handled entirely inside the daemon's watch loop: it acquires `index.lock` (serializing against other writes, but never blocking reads — reads already reopen a fresh connection per call and just keep hitting the still-valid old graph until the swap), builds an entirely new database at `.infigraph/graph.rebuilding/`, and on success atomically renames it into place while quarantining the old one. The CLI's `cmd_index` submits this request instead of refusing; the MCP path gets it for free since `tool_index_project` already shells out to the same CLI subprocess when one is available.

**Tech Stack:** Rust, reuses `infigraph-core`'s existing `daemon_protocol`/`watch`/`ops`/`quarantine` modules — no new dependencies.

## Global Constraints

- Every `cargo` command runs with `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND` when testing directly (this dev machine's shell profile sets both globally for interactive use; the pre-commit hook already strips them for its own commands, so `git commit` itself doesn't need this).
- Never set `CARGO_TARGET_DIR` explicitly. On `ENOSPC`, run `cargo clean --manifest-path <repo>/Cargo.toml --target-dir <repo>/.shared-target` (worktrees) or plain `cargo clean` (a non-worktree checkout) and retry.
- **Scope boundary**: this plan covers the CLI's `infigraph index --full` under daemon mode only. The MCP path (`tool_index_project`) already advertises `full` in its schema (confirmed current — R-NEW.4 was already fixed separately) and already prefers shelling out to the CLI subprocess when one is found, so it gets this fix automatically once the CLI is fixed. `tool_index_project`'s **in-process fallback** branch (used only when no CLI binary is found at all — a narrow, already-rare case) is explicitly **out of scope**: it calls `prism.index()` directly today, which does not go through this plan's daemon-routing mechanism under `INFIGRAPH_BACKEND=daemon`. This is a pre-existing narrow gap, not introduced or worsened by this plan — do not attempt to fix it here.
- **Disk cost is explicitly accepted, not mitigated.** The rebuild needs ~2x the graph's own on-disk size temporarily (old + new coexisting). No disk-preflight check is in scope — confirmed with the user that `.infigraph/graph`'s own size is small in practice regardless of codebase size.
- `WriteResult` reuses its existing `Ok { total_files, indexed_files }` and `Err { message }` variants for `FullReindex`'s own reply and for "superseded" waiter replies — do not add a new `WriteResult` variant. (This codebase has twice already paid a real cost extending this exhaustively-matched enum for genuinely irreducible data shapes like `ScipImportOk`/`ClustersOk`; `Ok`/`Err` already fit this feature's needs exactly, so adding a variant here would be pure unforced scope.)
- `IndexWorkQueue`'s existing bounded quarantine behavior (`crate::quarantine::quarantine_graph`, N=2 eviction) is reused as-is for the old-graph-aside step — do not write new quarantine/retention logic.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/infigraph-core/src/daemon_protocol.rs` | `WriteRequest::FullReindex` (new variant, no fields). |
| `crates/infigraph-core/src/lib.rs` | `Infigraph::open_local_kuzu_at` (new, `pub(crate)`) — opens a plain, non-daemon-routed `Infigraph` at an explicit `db_path`, for the daemon to build its own fresh rebuild copy without routing back through itself. |
| `crates/infigraph-core/src/watch/mod.rs` | `serve_full_reindex_request` (new function) — the whole build-fresh-then-swap sequence; one new match arm in `route_or_serve_request`. |
| `crates/infigraph-cli/src/index.rs` | `cmd_index`'s daemon-mode `--full` branch: submit the request instead of refusing. |
| `crates/infigraph-cli/tests/index_full_refused_under_daemon.rs` | Rewritten — this behavior is being replaced, not extended; the old refusal assertion is now wrong by design. |
| `crates/infigraph-core/tests/daemon_kuzu_e2e.rs` | New end-to-end tests against a real spawned daemon. |

---

### Task 1: `WriteRequest::FullReindex` + `Infigraph::open_local_kuzu_at`

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs:17-97` (`WriteRequest` enum)
- Modify: `crates/infigraph-core/src/lib.rs:129-153` (`impl Infigraph`, alongside `open`/`open_shared`), `:875-877` (existing `#[cfg(test)] mod tests` block)

**Interfaces:**
- Consumes: nothing new.
- Produces: `WriteRequest::FullReindex` (unit variant, no fields — matches `serde` derive already on the enum: `#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]`). `Infigraph::open_local_kuzu_at(root: &Path, registry: LanguageRegistry, db_path: PathBuf) -> Result<Self>` (`pub(crate)`) — Task 2 consumes this directly.

- [ ] **Step 1: Add the `FullReindex` variant**

In `crates/infigraph-core/src/daemon_protocol.rs`, add to the `WriteRequest` enum (after `ResolveCalls`, the last variant, at line 96):

```rust
    /// Rebuild the graph from scratch. Handled entirely inside the daemon's
    /// watch loop (`serve_full_reindex_request` in `watch/mod.rs`), which
    /// builds a fresh database at a side path and atomically swaps it in --
    /// see `docs/superpowers/specs/2026-08-04-daemon-routed-full-reindex-design.md`.
    /// No fields: it always means "rebuild everything."
    FullReindex,
```

- [ ] **Step 2: Write the failing test for `open_local_kuzu_at`**

Add to `crates/infigraph-core/src/lib.rs`'s existing `#[cfg(test)] mod tests` block (after the last test in that module — check the current end of the module and append there; do not create a second `mod tests`):

```rust
    /// `open_local_kuzu_at` is what the daemon's own full-reindex handler
    /// uses to build its rebuild copy -- it must ALWAYS open a plain, local
    /// Kuzu backend, never route through the daemon protocol, regardless of
    /// what `INFIGRAPH_BACKEND` happens to be set to in this process's own
    /// environment (the daemon's own env has `INFIGRAPH_BACKEND=daemon` set,
    /// since that's how it was told to run as one -- routing its own
    /// internal rebuild back through itself would be circular and would
    /// hang, since nothing would be listening for that self-submitted
    /// request in time).
    #[test]
    fn open_local_kuzu_at_ignores_daemon_backend_env_and_uses_a_custom_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();

        let custom_db_path = root.join(".infigraph").join("graph.rebuilding");
        let default_db_path = root.join(".infigraph").join("graph");

        std::env::set_var("INFIGRAPH_BACKEND", "daemon");
        let registry = LanguageRegistry::new();
        let prism = Infigraph::open_local_kuzu_at(root, registry, custom_db_path.clone()).unwrap();
        std::env::remove_var("INFIGRAPH_BACKEND");

        assert!(
            !prism.is_daemon_backend(),
            "must always open plain Kuzu, never route through the daemon protocol"
        );
        assert!(
            custom_db_path.exists(),
            "backend should have created its database at the custom path"
        );
        assert!(
            !default_db_path.exists(),
            "must not touch the default .infigraph/graph path"
        );

        // Confirm it's genuinely usable, not just constructed.
        let backend = prism.backend().unwrap();
        assert!(backend.get_file_hashes().unwrap().is_empty());
    }
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --lib open_local_kuzu_at_ignores_daemon_backend_env_and_uses_a_custom_path`
Expected: FAIL with a compile error — `open_local_kuzu_at` not found on `Infigraph`.

- [ ] **Step 4: Implement `open_local_kuzu_at`**

In `crates/infigraph-core/src/lib.rs`, add to `impl Infigraph` (immediately after `open_shared`, i.e. after line 153):

```rust
    /// Opens a plain, non-daemon-routed `Infigraph` at an explicit
    /// `db_path`, bypassing `init()`'s `INFIGRAPH_BACKEND` env-var
    /// branching entirely. Used by the daemon's own full-reindex handler
    /// (`serve_full_reindex_request` in `watch/mod.rs`) to build a fresh
    /// graph at a side path (e.g. `.infigraph/graph.rebuilding`) from
    /// *inside* the daemon process itself -- which must never route back
    /// through the daemon protocol to reach its own rebuild, regardless of
    /// what `INFIGRAPH_BACKEND` is set to in this process's environment.
    pub(crate) fn open_local_kuzu_at(
        root: &Path,
        registry: LanguageRegistry,
        db_path: PathBuf,
    ) -> Result<Self> {
        let root = root.canonicalize().context("invalid project root")?;
        let kb = graph::KuzuBackend::open(&db_path)?;
        Ok(Self {
            root,
            db_path,
            registry: std::sync::Arc::new(registry),
            backend_kind: BackendKind::Kuzu(kb),
            namespace: None,
        })
    }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --lib open_local_kuzu_at_ignores_daemon_backend_env_and_uses_a_custom_path`
Expected: `test result: ok. 1 passed; 0 failed`

- [ ] **Step 6: Run the full lib suite to confirm no regression**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --lib -- --test-threads=1`
Expected: all passing, same pass count as before this change plus 1.

- [ ] **Step 7: Clippy and fmt**

Run: `cargo clippy -p infigraph-core --all-targets -- -D warnings && cargo fmt -- --check`
Expected: both clean.

- [ ] **Step 8: Commit**

```bash
git add crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/src/lib.rs
git commit -m "feat: add WriteRequest::FullReindex and Infigraph::open_local_kuzu_at"
```

---

### Task 2: Daemon-side handler — build-fresh-then-swap

**Files:**
- Modify: `crates/infigraph-core/src/watch/mod.rs:814-966` (`route_or_serve_request`, add a match arm), and add a new `serve_full_reindex_request` function nearby (alongside `serve_request_locked`, i.e. near line 772-805).

**Interfaces:**
- Consumes: `WriteRequest::FullReindex` (Task 1), `Infigraph::open_local_kuzu_at` (Task 1), `crate::ops::begin_index_op`/`IndexOpOutcome` (existing), `crate::quarantine::quarantine_graph(infigraph_dir: &Path, graph_name: &str) -> Result<PathBuf>` (existing), `IndexWorkQueue::drain() -> DrainedQueue` (existing, Task 1 of the daemon-index-work-queue plan), `poison_watch_db`/`watch_db` (existing), `crate::daemon_protocol::write_atomic`/`WriteResult` (existing), `crate::embed::update_embeddings` (existing).
- Produces: `fn serve_full_reindex_request<MR>(root: &Path, path: &Path, queue: &Arc<Mutex<IndexWorkQueue>>, make_registry: &MR, held: &mut Option<Arc<Infigraph>>, drain_in_flight: bool)` — Task 3's CLI change doesn't call this directly (it submits the request over the daemon protocol), but Task 4's tests exercise it indirectly through a real daemon.

Read `crates/infigraph-core/src/watch/mod.rs`'s current `route_or_serve_request` (lines 814-966) and `serve_request_locked` (lines 772-805) in full before starting — this task's new function follows `serve_request_locked`'s exact locking pattern (`if drain_in_flight { return; }` then `begin_index_op` with a 30s wait), just with different post-lock behavior.

- [ ] **Step 1: Write the failing regression test (drives the handler directly, no real daemon process needed)**

Add to `crates/infigraph-core/src/watch/drain.rs`'s existing `#[cfg(test)] mod tests` block (this file already has the established pattern for driving daemon-internal logic directly against a real temp-dir project — reuse its `python_registry`/`open_project`/`python_pack` helpers rather than reinventing them; read the existing tests in this module first to confirm their exact current names before using them, since this plan was written without re-verifying every helper's exact signature a second time):

```rust
    /// Drives `serve_full_reindex_request` directly (no real daemon process)
    /// against a real temp-dir project: seeds a graph, submits FullReindex,
    /// confirms the old content is genuinely gone and the graph is rebuilt,
    /// and confirms the old graph directory was quarantined (renamed aside),
    /// not deleted.
    #[test]
    fn full_reindex_rebuilds_the_graph_and_quarantines_the_old_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("old.py"), "def old_symbol():\n    pass\n").unwrap();

        let prism = open_project(root);
        prism.index().unwrap();

        let old_graph_path = root.join(".infigraph").join("graph");
        assert!(old_graph_path.exists());

        // Simulate what changed between the bootstrap index and the full
        // reindex request: old.py is replaced by new.py, matching a real
        // "the codebase moved on" scenario a full reindex is meant to catch.
        std::fs::remove_file(root.join("old.py")).unwrap();
        std::fs::write(root.join("new.py"), "def new_symbol():\n    pass\n").unwrap();

        let queue = std::sync::Arc::new(std::sync::Mutex::new(
            crate::watch::queue::IndexWorkQueue::new(),
        ));
        let mut held: Option<std::sync::Arc<crate::Infigraph>> = None;
        let make_registry = || Ok(python_registry());

        let request_path = root.join(".infigraph").join("fullreindex.request");
        std::fs::write(
            &request_path,
            serde_json::to_string(&crate::daemon_protocol::WriteRequest::FullReindex).unwrap(),
        )
        .unwrap();

        crate::watch::mod_test_support::serve_full_reindex_request(
            root,
            &request_path,
            &queue,
            &make_registry,
            &mut held,
            false,
        );

        let reply_path = request_path.with_extension("result");
        let reply: crate::daemon_protocol::WriteResult =
            serde_json::from_str(&std::fs::read_to_string(&reply_path).unwrap()).unwrap();
        match reply {
            crate::daemon_protocol::WriteResult::Ok { indexed_files, .. } => {
                assert_eq!(indexed_files, 1, "only new.py should be in the rebuilt graph");
            }
            other => panic!("expected WriteResult::Ok, got {other:?}"),
        }

        // Verify against a completely fresh read connection, not the
        // (now-poisoned) `held` handle from before the swap.
        let verify = crate::graph::KuzuBackend::open_read_only(&old_graph_path).unwrap();
        let rows = verify
            .raw_query("MATCH (s:Symbol) RETURN s.name")
            .unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert!(names.contains(&"new_symbol"), "rebuilt graph must contain the new symbol");
        assert!(!names.contains(&"old_symbol"), "rebuilt graph must not contain the old symbol");

        let quarantine_entries: Vec<_> = std::fs::read_dir(root.join(".infigraph"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("graph.corrupt."))
            .collect();
        assert_eq!(
            quarantine_entries.len(),
            1,
            "the old graph must be quarantined (renamed aside), not deleted"
        );
    }
```

**Implementer note on the `crate::watch::mod_test_support::serve_full_reindex_request` path above:** `serve_full_reindex_request` will be a private (non-`pub`) function in `watch/mod.rs` per Step 2 below, so this test (living in `drain.rs`, a sibling module) cannot call it directly without a visibility adjustment. Before writing this test, either (a) mark `serve_full_reindex_request` `pub(crate)` instead of private — the simplest fix, matching `execute_drain`'s own `pub(crate)` visibility — and call it as `crate::watch::serve_full_reindex_request(...)` directly (drop the `mod_test_support` indirection above, it was a placeholder for "however visibility ends up working," not a real module to create), or (b) move this test into `watch/mod.rs`'s own test area instead of `drain.rs` if one exists. Prefer (a): check `serve_request_locked`'s own visibility (it's currently a private `fn`, non-`pub`) — if this codebase's convention is that these handler functions stay private and are only tested through `route_or_serve_request`'s public-enough surface or through real e2e tests, follow that convention instead and route this test through `route_or_serve_request(root, &request_path, &queue, &make_registry, &mut held, false)` instead of calling `serve_full_reindex_request` directly, which requires no visibility change at all. This is a plan-time uncertainty flagged deliberately rather than guessed — resolve it by reading the current file structure first, and pick whichever keeps the smallest visibility surface.

- [ ] **Step 2: Run the test to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --lib full_reindex_rebuilds_the_graph_and_quarantines_the_old_one`
Expected: FAIL — `WriteRequest::FullReindex` has no handling in `route_or_serve_request` yet (falls through to the `_ =>` wildcard arm, which calls `serve_request_locked`/`serve_one_request`, which has no match arm for `FullReindex` either — this should fail to compile or panic, confirming the gap).

- [ ] **Step 3: Implement `serve_full_reindex_request`**

In `crates/infigraph-core/src/watch/mod.rs`, add a new function alongside `serve_request_locked` (after it, around line 806):

```rust
/// Handles `WriteRequest::FullReindex`: builds an entirely new database at
/// `.infigraph/graph.rebuilding/`, leaving the live `.infigraph/graph`
/// completely untouched and fully readable throughout (reads already
/// reopen a fresh connection per call, so they transparently keep hitting
/// the still-valid old graph right up until the swap). On success,
/// atomically swaps the two directories in and quarantines the old one
/// (`crate::quarantine::quarantine_graph`) rather than deleting it. See
/// docs/superpowers/specs/2026-08-04-daemon-routed-full-reindex-design.md.
fn serve_full_reindex_request<MR>(
    root: &Path,
    path: &Path,
    queue: &Arc<Mutex<crate::watch::queue::IndexWorkQueue>>,
    make_registry: &MR,
    held: &mut Option<Arc<Infigraph>>,
    drain_in_flight: bool,
) where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    let reply_path = path.with_extension("result");

    // Never drain unless Acquired -- same invariant every other locked
    // operation in this loop preserves. Deferring here (rather than
    // blocking) also gets "wait for any in-progress drain to finish
    // first" for free: it's the same lock every write already
    // serializes on.
    if drain_in_flight {
        return;
    }

    let guard = match begin_index_op(root, "infigraph daemon (full reindex)", Duration::from_secs(30)) {
        Ok(IndexOpOutcome::Acquired(guard)) => guard,
        Ok(o @ IndexOpOutcome::AlreadyRunning(_)) => {
            eprintln!(
                "[daemon] full-reindex busy ({}), retrying next tick",
                o.skip_note().unwrap_or_default()
            );
            return;
        }
        Err(e) => {
            eprintln!("[daemon] full-reindex busy ({e}), retrying next tick");
            return;
        }
    };

    // Anything only queued (not yet executing) is genuinely moot -- the
    // full reindex is about to re-scan every file from disk regardless of
    // what was pending. Its waiters still get answered, just with a
    // superseded reply rather than silence.
    let superseded = queue.lock().unwrap().drain();
    for waiter in &superseded.waiters {
        let result = crate::daemon_protocol::WriteResult::Err {
            message: "superseded by a full reindex; resubmit if still needed".to_string(),
        };
        if let Ok(json) = serde_json::to_string(&result) {
            let _ = crate::daemon_protocol::write_atomic(&waiter.reply_path, &json);
        }
    }

    let infigraph_dir = root.join(".infigraph");
    let rebuilding_path = infigraph_dir.join("graph.rebuilding");
    let live_path = infigraph_dir.join("graph");

    // Clean up any stale leftover from a previously-interrupted rebuild
    // attempt (e.g. the daemon was killed mid-rebuild last time) before
    // starting a new one.
    if rebuilding_path.exists() {
        let _ = std::fs::remove_dir_all(&rebuilding_path);
        let _ = std::fs::remove_file(&rebuilding_path);
    }

    let registry = match make_registry() {
        Ok(r) => r,
        Err(e) => {
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!("full reindex failed: could not build language registry: {e}"),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(&reply_path, &json);
            }
            std::fs::remove_file(path).ok();
            drop(guard);
            return;
        }
    };

    let build_result = Infigraph::open_local_kuzu_at(root, registry, rebuilding_path.clone())
        .and_then(|fresh| {
            let backend = fresh
                .backend()
                .ok_or_else(|| anyhow::anyhow!("freshly-opened backend was not initialized"))?;
            let scan = fresh.scan_changed_files(backend)?;
            if !scan.extractions.is_empty() {
                backend.upsert_files_bulk(&scan.extractions, true)?;
            }
            let resolve_stats = backend.resolve_calls(&scan.extractions, None)?;
            Ok((scan.extractions.len(), resolve_stats))
        });

    match build_result {
        Ok((indexed_files, _resolve_stats)) => {
            // The live graph was never touched up to this point -- only
            // now, with a verified-good fresh build in hand, do we poison
            // the daemon's own handle and swap.
            poison_watch_db(held);

            let quarantine_result = if live_path.exists() {
                crate::quarantine::quarantine_graph(&infigraph_dir, "graph").map(|_| ())
            } else {
                Ok(())
            };

            match quarantine_result.and_then(|()| {
                std::fs::rename(&rebuilding_path, &live_path)
                    .map_err(|e| anyhow::anyhow!("failed to swap rebuilt graph into place: {e}"))
            }) {
                Ok(()) => {
                    // Reopen and reconcile embeddings against the NEW
                    // graph -- update_embeddings queries the live symbol
                    // set and prunes anything not in it, so this converges
                    // embeddings.bin to the rebuilt graph regardless of
                    // whether it was wiped first (it wasn't, deliberately
                    // -- see the design doc).
                    if let Ok(prism) = watch_db(root, make_registry, held) {
                        if let Some(backend) = prism.backend() {
                            if let Err(e) = crate::embed::update_embeddings(backend, root, &[]) {
                                eprintln!("[daemon] full-reindex: embedding update failed: {e}");
                            }
                        }
                    }
                    let result = crate::daemon_protocol::WriteResult::Ok {
                        total_files: indexed_files,
                        indexed_files,
                    };
                    if let Ok(json) = serde_json::to_string(&result) {
                        let _ = crate::daemon_protocol::write_atomic(&reply_path, &json);
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[daemon] full-reindex swap failed after a successful rebuild: {e:#} \
                         -- check {} and {} by hand",
                        live_path.display(),
                        rebuilding_path.display()
                    );
                    let result = crate::daemon_protocol::WriteResult::Err {
                        message: format!(
                            "full reindex rebuilt successfully but the swap failed: {e:#}. \
                             Manual recovery needed: check {} and {}",
                            live_path.display(),
                            rebuilding_path.display()
                        ),
                    };
                    if let Ok(json) = serde_json::to_string(&result) {
                        let _ = crate::daemon_protocol::write_atomic(&reply_path, &json);
                    }
                }
            }
        }
        Err(e) => {
            // The live graph was never touched -- clean up the incomplete
            // rebuild attempt and reply with the failure. The daemon keeps
            // serving the old (still fully valid) graph exactly as before.
            let _ = std::fs::remove_dir_all(&rebuilding_path);
            let _ = std::fs::remove_file(&rebuilding_path);
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!("full reindex failed: {e:#}"),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(&reply_path, &json);
            }
        }
    }

    std::fs::remove_file(path).ok();
    drop(guard);
}
```

**Note on `update_embeddings(backend, root, &[])`**: `update_embeddings` treats an empty `changed_files` slice as "treat all symbols as changed" per its own doc comment (confirmed by reading its current source: `if !changed_set.is_empty() && !changed_set.contains(file) && existing.contains_key(id) { return None; }` — an empty `changed_set` never satisfies `!changed_set.is_empty()`, so nothing is skipped). This is exactly the desired behavior here (re-embed everything against the freshly-rebuilt graph), and matches this function's own doc comment rather than being a guess.

- [ ] **Step 4: Wire the new match arm into `route_or_serve_request`**

In `crates/infigraph-core/src/watch/mod.rs`, add a new arm to `route_or_serve_request`'s `match request { ... }` (before the final `_ =>` wildcard arm, so it doesn't fall through to `serve_request_locked`):

```rust
        WriteRequest::FullReindex => {
            serve_full_reindex_request(root, path, queue, make_registry, held, drain_in_flight);
        }
```

- [ ] **Step 5: Resolve the Step 1 visibility question and run the test**

Per Step 1's implementer note: check `serve_request_locked`'s current visibility in the real file, apply the same convention to `serve_full_reindex_request`, and adjust the test in Step 1 to call it through whichever path that visibility allows (either directly, or through `route_or_serve_request`).

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --lib full_reindex_rebuilds_the_graph_and_quarantines_the_old_one`
Expected: `test result: ok. 1 passed; 0 failed`

- [ ] **Step 6: Run the full lib suite plus targeted daemon suite**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --lib --test daemon_kuzu_backend --test daemon_protocol_serve --test backend_selection --test watch_daemon --test daemon_kuzu_e2e -- --test-threads=1`
Expected: all passing, no regressions in any Task 1-4 daemon-index-work-queue tests.

- [ ] **Step 7: Clippy and fmt**

Run: `cargo clippy -p infigraph-core --all-targets -- -D warnings && cargo fmt -- --check`
Expected: both clean.

- [ ] **Step 8: Commit**

```bash
git add crates/infigraph-core/src/watch/mod.rs crates/infigraph-core/src/watch/drain.rs
git commit -m "feat: daemon-side FullReindex handler (build-fresh-then-swap)"
```

---

### Task 3: CLI wiring

**Files:**
- Modify: `crates/infigraph-cli/src/index.rs:41-89` (`cmd_index`'s `if full { ... }` block)
- Modify: `crates/infigraph-cli/tests/index_full_refused_under_daemon.rs` (rewritten — the old refusal behavior is being replaced)

**Interfaces:**
- Consumes: `WriteRequest::FullReindex` (Task 1), `crate::daemon_protocol::submit_write_request` (existing: `submit_write_request(staging_dir: &Path, request: &WriteRequest, timeout: Duration) -> anyhow::Result<WriteResult>`), `WriteResult` (existing).
- Produces: nothing new for later tasks — this is the last client-facing piece.

- [ ] **Step 1: Delete the now-obsolete refusal test**

Read the current `crates/infigraph-cli/tests/index_full_refused_under_daemon.rs` first — it asserts `!output.status.success()` and specific refusal-message text. That behavior is being replaced by this task, not extended, so this whole test file's assertions are now wrong by design. Replace its entire contents with a new test proving the opposite: a real daemon-routed full reindex succeeds and actually rebuilds the graph.

```rust
//! `infigraph index --full` under `INFIGRAPH_BACKEND=daemon` now works by
//! routing through the daemon's own build-fresh-then-swap handler, instead
//! of refusing. See
//! docs/superpowers/specs/2026-08-04-daemon-routed-full-reindex-design.md
//! and the real e2e coverage in
//! crates/infigraph-core/tests/daemon_kuzu_e2e.rs (this file only checks
//! the CLI-level success/failure contract, not the daemon internals).

use std::process::Command;

#[test]
fn full_reindex_succeeds_under_daemon_backend_with_a_real_running_daemon() {
    let project = tempfile::tempdir().expect("failed to create project temp dir");
    let fake_home = tempfile::tempdir().expect("failed to create fake home temp dir");

    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n")
        .expect("failed to write fixture file");

    let cli = env!("CARGO_BIN_EXE_infigraph");

    // Bootstrap-index locally first (no daemon involved yet), matching the
    // established pattern in crates/infigraph-core/tests/daemon_kuzu_e2e.rs.
    let bootstrap = Command::new(cli)
        .arg("index")
        .current_dir(project.path())
        .env("HOME", fake_home.path())
        .env_remove("INFIGRAPH_BACKEND")
        .status()
        .expect("failed to run bootstrap infigraph index");
    assert!(bootstrap.success(), "bootstrap index must succeed");

    // Start a real daemon against the project.
    let mut daemon = Command::new(cli)
        .arg("daemon")
        .current_dir(project.path())
        .env("HOME", fake_home.path())
        .env_remove("INFIGRAPH_BACKEND")
        .spawn()
        .expect("failed to spawn daemon");

    let lock_path = project.path().join(".infigraph").join("watch.lock");
    let start = std::time::Instant::now();
    loop {
        if lock_path.exists() && std::fs::metadata(&lock_path).unwrap().len() > 0 {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(10) {
            let _ = daemon.kill();
            panic!("daemon never acquired watch.lock");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let output = Command::new(cli)
        .arg("index")
        .arg("--full")
        .arg("--no-embed")
        .current_dir(project.path())
        .env("HOME", fake_home.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("failed to run infigraph index --full");

    let _ = daemon.kill();
    let _ = daemon.wait();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected `infigraph index --full` to succeed under the daemon backend, but it failed:\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        !stderr.contains("not yet supported under the daemon backend"),
        "the old refusal message must not appear -- this behavior was replaced, got:\n{stderr}"
    );

    // Verify the graph genuinely has real content (a real rebuild
    // happened, not a silent no-op).
    let graph_path = project.path().join(".infigraph").join("graph");
    assert!(graph_path.exists(), "graph must exist after a full reindex");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo build -p infigraph-cli --bin infigraph && cargo test -p infigraph-cli --test index_full_refused_under_daemon`
Expected: FAIL — `cmd_index` still refuses `--full` under daemon mode, so `output.status.success()` is false.

- [ ] **Step 3: Replace the refusal with a real submission**

In `crates/infigraph-cli/src/index.rs`, replace the `else if infigraph_core::daemon_backend_selected() { ... bail! ... }` branch (currently lines 51-69) with:

```rust
        } else if infigraph_core::daemon_backend_selected() {
            // Routed through the daemon's own FullReindex handler, which
            // builds a fresh database at a side path and atomically swaps
            // it in -- see
            // docs/superpowers/specs/2026-08-04-daemon-routed-full-reindex-design.md.
            // Closes https://github.com/pradeepmouli/infigraph/issues/50's
            // real fix (the mitigation this branch used to be is now
            // closed).
            let staging_dir = root.join(".infigraph").join("requests");
            let result = infigraph_core::daemon_protocol::submit_write_request(
                &staging_dir,
                &infigraph_core::daemon_protocol::WriteRequest::FullReindex,
                std::time::Duration::from_secs(600),
            )?;
            match result {
                infigraph_core::daemon_protocol::WriteResult::Ok {
                    total_files,
                    indexed_files,
                } => {
                    println!("Indexed {indexed_files} files ({total_files} total, full reindex)");
                }
                infigraph_core::daemon_protocol::WriteResult::Err { message } => {
                    anyhow::bail!("full reindex failed: {message}");
                }
                other => {
                    anyhow::bail!("full reindex returned an unexpected result: {other:?}");
                }
            }
            // The daemon already did the full reindex -- nothing left for
            // this process to do.
            return Ok(());
        } else {
```

Note this drops the earlier `op_guard` (already `None` under `daemon_backend_selected()` per the existing code at line 19-20 — no change needed there) and skips the rest of `cmd_index`'s body (the fall-through `Infigraph::open(root, registry)?; prism.index()?; ...` normal-index sequence) via the early `return Ok(())`, since the daemon already did the full rebuild.

- [ ] **Step 4: Run the test to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo build -p infigraph-cli --bin infigraph && cargo test -p infigraph-cli --test index_full_refused_under_daemon -- --test-threads=1`
Expected: `test result: ok. 1 passed; 0 failed`

- [ ] **Step 5: Run the full infigraph-cli test suite**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-cli -- --test-threads=1`
Expected: all passing.

- [ ] **Step 6: Clippy and fmt**

Run: `cargo clippy -p infigraph-cli --all-targets -- -D warnings && cargo fmt -- --check`
Expected: both clean.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-cli/src/index.rs crates/infigraph-cli/tests/index_full_refused_under_daemon.rs
git commit -m "feat: route infigraph index --full through the daemon instead of refusing"
```

---

### Task 4: End-to-end verification

**Files:**
- Modify: `crates/infigraph-core/tests/daemon_kuzu_e2e.rs` (add new tests, reusing this file's existing `start_real_daemon`/`cli_binary`/`ENV_LOCK`/`verify_conn`/`KillOnDrop`/`CLI_INDEX_DEADLINE` helpers exactly as they are — do not modify them)

**Interfaces:**
- Consumes: everything from Tasks 1-3, plus this file's existing test helpers (read them in full before writing — their exact current bodies are reproduced in this plan's research but re-confirm nothing drifted).

- [ ] **Step 1: Write the concurrent-request-superseded test**

Add to `crates/infigraph-core/tests/daemon_kuzu_e2e.rs`:

```rust
/// A request that's only queued (not yet executing) when a FullReindex
/// arrives is superseded, not silently dropped -- its waiter gets an
/// explicit reply rather than hanging until its own client-side timeout.
#[test]
fn a_queued_request_racing_a_full_reindex_gets_a_superseded_reply_not_a_hang() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    let mut daemon = start_real_daemon(project.path());

    // Submit both near-simultaneously: an ad-hoc single-file Index request
    // and a FullReindex, racing to see which the daemon picks up first.
    // Regardless of ordering, the Index request's client must get SOME
    // reply within the deadline -- either its own normal completion (if it
    // slipped in first) or a superseded error (if FullReindex won) -- never
    // a hang.
    std::fs::write(project.path().join("b.py"), "def b():\n    pass\n").unwrap();

    let cli = cli_binary();
    let index_child = std::process::Command::new(&cli)
        .arg("index")
        .arg("--no-embed")
        .current_dir(project.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .env("INFIGRAPH_INDEX_VIA_DAEMON", "1")
        .spawn()
        .unwrap();

    let full_output = std::process::Command::new(&cli)
        .arg("index")
        .arg("--full")
        .arg("--no-embed")
        .current_dir(project.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .output()
        .unwrap();

    let index_output = index_child.wait_with_output().unwrap();

    // Both must complete within the deadline (neither hangs), regardless
    // of which one the daemon happened to serve first.
    assert!(
        full_output.status.success(),
        "full reindex must succeed:\nstderr={}",
        String::from_utf8_lossy(&full_output.stderr)
    );
    // The ad-hoc index either succeeded normally or failed with the
    // superseded message -- both are acceptable outcomes; a hang (the
    // process still running past this point) is what this test guards
    // against, and `wait_with_output` above already proves it didn't hang.
    let index_stderr = String::from_utf8_lossy(&index_output.stderr);
    if !index_output.status.success() {
        assert!(
            index_stderr.contains("superseded"),
            "if the ad-hoc index failed, it must be because it was superseded, not some other error:\n{index_stderr}"
        );
    }

    daemon.0.kill().ok();
}

/// A read attempted during the rebuild window sees the still-valid OLD
/// graph, not an error or an empty result -- proving reads are genuinely
/// unaffected by an in-progress full reindex, not just assumed to be.
#[test]
fn a_read_during_full_reindex_sees_the_old_graph_not_an_error() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project = tempfile::tempdir().unwrap();
    // Enough files that the rebuild takes real wall time, giving a window
    // for a concurrent read to land mid-rebuild.
    for i in 0..300 {
        std::fs::write(
            project.path().join(format!("f{i}.py")),
            format!("def f{i}():\n    pass\n"),
        )
        .unwrap();
    }

    let mut daemon = start_real_daemon(project.path());
    let cli = cli_binary();

    let mut full = std::process::Command::new(&cli)
        .arg("index")
        .arg("--full")
        .arg("--no-embed")
        .current_dir(project.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .spawn()
        .unwrap();

    // Give the rebuild a moment to actually start before reading.
    std::thread::sleep(Duration::from_millis(200));

    // A direct read-only connection against the LIVE path (not through the
    // daemon protocol -- reads never route through it), matching
    // `verify_conn`'s own approach.
    let read_during = verify_conn(project.path());
    let rows = read_during
        .raw_query("MATCH (s:Symbol) RETURN count(s.id)")
        .expect("a read during the rebuild window must succeed, not error");
    assert!(!rows.is_empty());

    let status = full.wait().unwrap();
    assert!(status.success(), "full reindex must still succeed");

    daemon.0.kill().ok();
}

/// A failed rebuild (extraction/write error mid-build) leaves the live
/// graph completely unharmed -- the real advantage of build-fresh-then-swap
/// over wipe-in-place.
#[test]
fn a_failed_rebuild_leaves_the_live_graph_untouched() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    let mut daemon = start_real_daemon(project.path());

    let before = verify_conn(project.path());
    let before_rows = before
        .raw_query("MATCH (s:Symbol) RETURN s.name")
        .unwrap();
    assert!(!before_rows.is_empty(), "must have real content before the attempt");

    // Make the project root itself unreadable-as-a-directory to force
    // `scan_changed_files`'s file collection to fail partway through the
    // rebuild -- a real, reproducible failure mode rather than a
    // synthetic hook. (Restored before the daemon shuts down, so cleanup
    // doesn't itself fail.)
    let unreadable_dir = project.path().join("unreadable");
    std::fs::create_dir(&unreadable_dir).unwrap();
    std::fs::write(unreadable_dir.join("x.py"), "def x():\n    pass\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&unreadable_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    }

    let cli = cli_binary();
    let output = std::process::Command::new(&cli)
        .arg("index")
        .arg("--full")
        .arg("--no-embed")
        .current_dir(project.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .output()
        .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&unreadable_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Whether this specific permission trick actually fails the rebuild is
    // platform/permission-model dependent (e.g. root-owned CI runners may
    // bypass it) -- assert on the INVARIANT this test actually cares
    // about (the live graph survives) rather than requiring the injection
    // to have worked, so this test isn't flaky-by-construction across
    // environments.
    let _ = output.status;

    let after = verify_conn(project.path());
    let after_rows = after.raw_query("MATCH (s:Symbol) RETURN s.name").unwrap();
    assert!(
        !after_rows.is_empty(),
        "the live graph must survive regardless of whether the rebuild succeeded or failed"
    );

    let rebuilding_path = project.path().join(".infigraph").join("graph.rebuilding");
    assert!(
        !rebuilding_path.exists(),
        "an incomplete rebuild attempt must not leave graph.rebuilding behind"
    );

    daemon.0.kill().ok();
}
```

- [ ] **Step 2: Run the new tests to verify they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo build -p infigraph-cli --bin infigraph && cargo test -p infigraph-core --test daemon_kuzu_e2e -- --test-threads=1 --nocapture 2>&1 | tail -60`
Expected: all three new tests FAIL (Task 2/3 aren't wired up to a real daemon process test yet at this point if run before Tasks 1-3 are committed — if Tasks 1-3 are already committed by the time this task runs, these should already pass; run this step regardless to confirm the tests are exercising real behavior, not tautologies — if they pass immediately with zero prior failures observed, treat that as a signal to double check the test actually reaches the code path it claims to, not as success).

- [ ] **Step 3: Run to verify they pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo build -p infigraph-cli --bin infigraph && cargo test -p infigraph-core --test daemon_kuzu_e2e -- --test-threads=1 --nocapture 2>&1 | tail -60`
Expected: all tests in the file pass, including the three new ones.

- [ ] **Step 4: Run the full relevant suite one more time**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test daemon_kuzu_backend --test daemon_protocol_serve --test backend_selection --test watch_daemon --test daemon_kuzu_e2e --lib -- --test-threads=1` and `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-cli -- --test-threads=1`
Expected: everything green.

- [ ] **Step 5: Clippy and fmt**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: both clean.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/tests/daemon_kuzu_e2e.rs
git commit -m "test: e2e coverage for daemon-routed full reindex"
```

---

## Self-Review

**Spec coverage:**
- Build-fresh-then-swap architecture → Task 2. ✅
- `WriteRequest::FullReindex`, no payload → Task 1. ✅
- CLI submits instead of refuses → Task 3. ✅
- MCP path → confirmed already covered (schema already exposes `full`, prefers CLI subprocess) — explicitly noted as no-task-needed in Global Constraints, not silently dropped. ✅
- "Residue" rule (in-progress work finishes via lock-wait, queued-but-not-started work discarded with superseded waiter replies) → Task 2, Step 3. ✅
- Reads unaffected, no lock check on the read path → architecturally true by construction (build-fresh-then-swap never touches the live path until the final rename) — verified by Task 4's dedicated test rather than merely asserted. ✅
- Old graph quarantined, not deleted, reusing existing `quarantine_graph` (N=2 bound included) → Task 2, Step 3. ✅
- Failure handling: failed rebuild leaves live graph untouched; failed swap surfaces a loud, actionable error → Task 2, Step 3 (both branches), Task 4's failure-injection test. ✅
- Testing section's five scenarios → all five covered: real rebuild (Task 2's unit-style test + Task 3's CLI test), concurrent-superseded (Task 4), read-during-window (Task 4), quarantine confirmed (Task 2's test), failure-injection (Task 4). ✅

**Placeholder scan:** Task 2 Step 1 contains one flagged, deliberate uncertainty (the `serve_full_reindex_request` visibility question) — resolved with concrete alternatives and a clear "prefer (a)" recommendation, not a bare TBD. This mirrors the previous daemon-index-work-queue plan's own precedent for flagging genuine plan-time unknowns rather than guessing and presenting it as settled. Everything else is complete, runnable code.

**Type consistency:** `WriteRequest::FullReindex` (Task 1) is referenced identically in Task 2's match arm and Task 2/4's test code. `Infigraph::open_local_kuzu_at`'s signature (Task 1) matches its one call site in Task 2 exactly (`root: &Path, registry: LanguageRegistry, db_path: PathBuf`). `serve_full_reindex_request`'s parameter list (Task 2) matches `route_or_serve_request`'s call to it exactly, mirroring `serve_request_locked`'s existing parameter shape.
