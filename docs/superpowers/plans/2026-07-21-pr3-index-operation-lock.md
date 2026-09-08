# PR 3: Index Operation Lock + Coalescing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A per-project `.infigraph/index.lock` held across each logical index operation, with coalescing (a second index run reports "already in progress — skipped" instead of interleaving) — PR 3 of `docs/superpowers/specs/2026-07-20-write-safety-locks-design.md`.

**Architecture:** New `infigraph_core::ops` module owns operation-lock semantics on top of PR 1's `lockfile`: `begin_index_op(root, wait)` returns either an RAII guard or `AlreadyRunning(holder)`. Index runs (CLI, MCP fallback, SCIP enrich, group members) use `wait=ZERO` → coalesce; watcher batch flushes use a 30s wait → serialize. The fine-grained `graph.lock` stays unchanged beneath. Bonus hardening: `cmd_index --full`'s currently-unlocked `remove_dir_all(.infigraph)` becomes guarded by acquiring the op lock *before* the wipe.

**Tech Stack:** Rust; consumes `lockfile::{try_acquire, acquire, read_holder, LockFile, LockInfo, Busy}`. Branch `feat/index-operation-lock` off `feat/write-lock-enforcement` (stacked; retarget as parents merge).

## Global Constraints

- **Fork-only PR** (pradeepmouli/infigraph). NEVER open an upstream PR — standing directive.
- Commit `--no-verify`; trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Off-limits (never touch/reformat): `crates/infigraph-core/src/scip/mod.rs`, `crates/infigraph-languages/tests/registry_integration.rs`. Never bare-rustfmt `src/lib.rs` (either crate).
- After edits: `rustfmt --check --edition 2021` every changed standalone file.
- Builds/tests: ALWAYS `CARGO_PROFILE_DEV_DEBUG=0`, tests with `-- --test-threads=4`. Never vary the debug setting (fingerprint churn spawns ~3GB duplicate lbug trees).
- Hooks: Read blocked → retry with `offset`; test-file Edit blocked → refresh `.infigraph/.test-context-called` with current epoch seconds (authorized) or bash heredoc; python3 over grep/rg.
- Zero new warnings. Line numbers below may drift — trust names.
- Coalescing message format (spec-mandated shape): `index already in progress ({role}, PID {pid}, started {secs}s ago) — skipped` — with `(unknown holder)` replacing the parenthetical when no payload is readable.

---

### Task 1: `ops` module — `begin_index_op`

**Files:**
- Create: `crates/infigraph-core/src/ops.rs`
- Modify: `crates/infigraph-core/src/lib.rs` (add `pub mod ops;` — careful, no bare rustfmt)
- Test: `crates/infigraph-core/tests/index_op.rs` (new)

**Interfaces (later tasks consume verbatim):**

```rust
pub struct IndexOpGuard { /* private: LockFile */ }

pub enum IndexOpOutcome {
    Acquired(IndexOpGuard),
    /// Lock held by a live operation; holder identity when readable.
    AlreadyRunning(Option<crate::lockfile::LockInfo>),
}

impl IndexOpOutcome {
    /// Spec-mandated coalescing note for the AlreadyRunning case.
    pub fn skip_note(&self) -> Option<String>;
}

/// Acquire the per-project index-operation lock at `<root>/.infigraph/index.lock`.
/// `wait == Duration::ZERO` → non-blocking try: a held lock yields AlreadyRunning
/// (coalescing). Nonzero → bounded wait via lockfile::acquire; Busy propagates
/// as Err (callers that wait treat Busy as a real error, not a skip).
pub fn begin_index_op(root: &Path, role: &str, wait: Duration) -> Result<IndexOpOutcome>;
```

- [ ] **Step 1 (failing tests):** create `tests/index_op.rs`:

```rust
use std::time::Duration;

use infigraph_core::ops::{begin_index_op, IndexOpOutcome};
use tempfile::TempDir;

#[test]
fn test_acquire_then_coalesce() {
    let dir = TempDir::new().unwrap();
    let g = match begin_index_op(dir.path(), "test-index", Duration::ZERO).unwrap() {
        IndexOpOutcome::Acquired(g) => g,
        IndexOpOutcome::AlreadyRunning(_) => panic!("free lock must acquire"),
    };
    // Second try coalesces and can render the skip note with holder identity.
    match begin_index_op(dir.path(), "second", Duration::ZERO).unwrap() {
        IndexOpOutcome::Acquired(_) => panic!("held lock must coalesce"),
        o @ IndexOpOutcome::AlreadyRunning(_) => {
            let note = o.skip_note().expect("skip note for AlreadyRunning");
            assert!(note.contains("index already in progress"), "{note}");
            assert!(note.contains("test-index"), "holder role in note: {note}");
            assert!(note.contains(&std::process::id().to_string()), "holder pid: {note}");
            assert!(note.ends_with("— skipped"), "{note}");
        }
    }
    drop(g);
    // Released → acquirable again.
    assert!(matches!(
        begin_index_op(dir.path(), "third", Duration::ZERO).unwrap(),
        IndexOpOutcome::Acquired(_)
    ));
}

#[test]
fn test_wait_mode_busy_is_error() {
    let dir = TempDir::new().unwrap();
    let _g = begin_index_op(dir.path(), "holder", Duration::ZERO).unwrap();
    let err = begin_index_op(dir.path(), "waiter", Duration::from_millis(200))
        .expect_err("nonzero wait on held lock must Err(Busy), not coalesce");
    assert!(err.downcast_ref::<infigraph_core::lockfile::Busy>().is_some());
}

#[test]
fn test_skip_note_unknown_holder() {
    // Bare flock (no payload) → unknown-holder note.
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join(".infigraph").join("index.lock");
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let bare = std::fs::OpenOptions::new().create(true).write(true).truncate(false)
        .open(&lock_path).unwrap();
    fs2::FileExt::lock_exclusive(&bare).unwrap();
    let o = begin_index_op(dir.path(), "x", Duration::ZERO).unwrap();
    let note = o.skip_note().expect("note");
    assert!(note.contains("unknown holder"), "{note}");
    fs2::FileExt::unlock(&bare).unwrap();
}
```

- [ ] **Step 2:** run `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test index_op -- --test-threads=4` — FAIL (no `ops` module).
- [ ] **Step 3 (implement)** `src/ops.rs`:

```rust
//! Operation-scoped locks: coarser than the per-call graph write lock,
//! held across a whole logical operation (an index run, a SCIP import, a
//! watcher batch) so two operations never interleave their write batches.
//! The fine-grained `graph.lock` remains the corruption floor beneath.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::lockfile::{self, LockFile, LockInfo};

pub struct IndexOpGuard {
    _lock: LockFile,
}

pub enum IndexOpOutcome {
    Acquired(IndexOpGuard),
    /// Lock held by a live operation; holder identity when readable.
    AlreadyRunning(Option<LockInfo>),
}

impl IndexOpOutcome {
    pub fn skip_note(&self) -> Option<String> {
        match self {
            IndexOpOutcome::Acquired(_) => None,
            IndexOpOutcome::AlreadyRunning(Some(h)) => {
                let started = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs().saturating_sub(h.acquired_at))
                    .unwrap_or(0);
                Some(format!(
                    "index already in progress ({}, PID {}, started {}s ago) — skipped",
                    h.role, h.pid, started
                ))
            }
            IndexOpOutcome::AlreadyRunning(None) => {
                Some("index already in progress (unknown holder) — skipped".to_string())
            }
        }
    }
}

fn index_lock_path(root: &Path) -> std::path::PathBuf {
    root.join(".infigraph").join("index.lock")
}

pub fn begin_index_op(root: &Path, role: &str, wait: Duration) -> Result<IndexOpOutcome> {
    let path = index_lock_path(root);
    if wait.is_zero() {
        match lockfile::try_acquire(&path, role)? {
            Some(lock) => Ok(IndexOpOutcome::Acquired(IndexOpGuard { _lock: lock })),
            None => Ok(IndexOpOutcome::AlreadyRunning(lockfile::read_holder(&path))),
        }
    } else {
        let lock = lockfile::acquire(&path, role, wait)?;
        Ok(IndexOpOutcome::Acquired(IndexOpGuard { _lock: lock }))
    }
}
```

Add `pub mod ops;` to `crates/infigraph-core/src/lib.rs` alphabetically (between `multi` and `patterns` region — check actual neighbors).
- [ ] **Step 4:** tests pass (3/3). `rustfmt --check` clean on ops.rs + index_op.rs.
- [ ] **Step 5: Commit** — `feat: index operation lock with coalescing (begin_index_op)`.

---

### Task 2: CLI + MCP index entry points

**Files:**
- Modify: `crates/infigraph-cli/src/index.rs` — `cmd_index` (:9, acquisition BEFORE the `if full` wipe block at :15-42; explicit `drop(op_guard)` after index/result printing and BEFORE the SCIP-enrich child spawn later in the function), `cmd_scip_enrich` (:697, acquisition at entry)
- Modify: `crates/infigraph-mcp/src/tools/index.rs` — `tool_index_project`'s inline fallback path (the `let result = prism.index()?;` arm ~:60; the primary path shells to the CLI, which now handles it)

Wiring pattern (all three sites):

```rust
    let op = infigraph_core::ops::begin_index_op(root, "infigraph index", std::time::Duration::ZERO)?;
    let _op_guard = match op {
        infigraph_core::ops::IndexOpOutcome::Acquired(g) => g,
        o @ infigraph_core::ops::IndexOpOutcome::AlreadyRunning(_) => {
            println!("{}", o.skip_note().unwrap());
            return Ok(());
        }
    };
```

Roles: `"infigraph index"` (cmd_index), `"scip-enrich"` (cmd_scip_enrich — its skip goes to eprintln/log, and it returns without error since enrichment re-runs on the next index), `"index_project (mcp)"` (MCP fallback — the skip note becomes the tool's Ok(String) return).

- [ ] **Step 1:** Wire `cmd_index`: acquire before the full-wipe block (this also closes the unlocked `remove_dir_all(.infigraph)` gap — note it in the commit message); print skip note + `return Ok(())` on AlreadyRunning; **explicitly `drop(_op_guard);` immediately after the index-result printing and before any SCIP-enrich spawn** — otherwise the detached child coalesces against its own parent and enrichment silently never runs. Locate the spawn (search `scip_enrich_args` / the "SCIP enrichment starting in background" print) and place the drop above it with a comment stating exactly that hazard.
- [ ] **Step 2:** Wire `cmd_scip_enrich` (entry acquisition, role "scip-enrich", skip → log line + clean return) and the MCP fallback path (skip note returned as the tool output).
- [ ] **Step 3:** Verify: `CARGO_PROFILE_DEV_DEBUG=0 cargo check -p infigraph-cli -p infigraph-mcp` clean; run `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-cli -- --test-threads=4` (cli_parity + index tests) and the mcp index-adjacent suites per the compiler's guidance — green.
- [ ] **Step 4:** Manual smoke (cheap, run in a temp copy of `tests/fixtures/python-simple`): run `target/debug/infigraph index` twice concurrently (`&` + immediate second run) — second prints the skip note. Include the observed output in the report.
- [ ] **Step 5: Commit** — `feat: index runs take the operation lock and coalesce; --full wipe now guarded`.

---

### Task 3: Watcher batch + periodic flushes

**Files:**
- Modify: `crates/infigraph-core/src/watch/mod.rs` — batch flush (~:174-180, `prism.index_files(&paths)`) and the periodic-reindex branch (~:153-171, `prism.index()`)
- Test: `crates/infigraph-mcp/tests/watcher_reindex.rs` (existing suites must stay green) + one new core test in `tests/index_op.rs`

Behavior: watcher writes SERIALIZE (they must not be lost): `begin_index_op(root, "infigraph watch", Duration::from_secs(30))`. On `Err` (Busy — an index run exceeding 30s): do NOT drop the drained paths — re-add them to the batch (inspect `ChangeBatch`'s API in `watch/batch.rs`; if no re-add method exists, add a `pub fn readd(&mut self, paths: Vec<PathBuf>)` that merges them back and resets the window) and `continue` the loop (retry next window). Log one `[watch] index operation busy (…holder…), retrying batch next window` line.

- [ ] **Step 1:** New core test: hold the op lock, call the watch-side helper... the flush logic is inline in the loop, so extract the acquisition+flush decision into a small testable helper if trivial — otherwise test via `begin_index_op` semantics only (already covered in Task 1) and rely on the existing watcher integration suites for the loop behavior; state in the report which route was taken and why.
- [ ] **Step 2:** Wire both branches (batch flush + periodic) with the wait-mode acquisition; implement readd-on-Busy.
- [ ] **Step 3:** Verify: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_reindex --test watcher_concurrency -- --test-threads=4` green; core index_op suite green.
- [ ] **Step 4: Commit** — `feat: watcher batches serialize on the index operation lock, requeueing on contention`.

---

### Task 4: Group operations

**Files:**
- Modify: `crates/infigraph-core/src/multi/mod.rs` — `index_group` (per-member loop: `begin_index_op(member_root, "group index", Duration::ZERO)`; on AlreadyRunning, record the skip note in that member's result entry and continue with other members — the results tuple/struct may need a message slot; extend minimally)
- Modify: `crates/infigraph-core/src/multi/combined.rs` — combined-graph build entry (~:42 already write-locks the combined store): additionally take the GROUP root's op lock (`begin_index_op(group_root, "group build", Duration::ZERO)`, coalesce with note) — the group dir is project-shaped so the same `.infigraph/index.lock` convention applies
- Callers surfacing the notes: `crates/infigraph-mcp/src/tools/groups.rs` `tool_group_index` (:214) result formatting, CLI group command equivalents (locate via compiler)

- [ ] **Step 1:** Wire per-member coalescing in `index_group`; skipped members appear in output as `repo: skipped — index already in progress (…)`.
- [ ] **Step 2:** Wire the combined-build op lock (group root), coalescing with note through both MCP and CLI surfaces.
- [ ] **Step 3:** Verify: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test combined_graph -- --test-threads=4` + `cargo test -p infigraph-mcp --test groups_watch_perf -- --test-threads=4` (and whatever group suites the compiler/test list names) — green.
- [ ] **Step 4: Commit** — `feat: group index/build take operation locks per member and per group`.

---

### Task 5: Full verification + stacked fork PR

- [ ] **Step 1:** `set -o pipefail; CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core -- --test-threads=4` and same for `-p infigraph-mcp` — core all green; mcp green except the KNOWN pre-existing `advertised_tools_match_mcp_tool_names` failure (proven pre-existing on base; do not fix here).
- [ ] **Step 2:** `cargo clippy -p infigraph-core -p infigraph-cli -p infigraph-mcp --tests` — no new warnings (pre-existing vuln/mod.rs one is out of scope).
- [ ] **Step 3:** `git push -u origin feat/index-operation-lock`.
- [ ] **Step 4:** Fork PR ONLY: `gh pr create --repo pradeepmouli/infigraph --base feat/write-lock-enforcement --head feat/index-operation-lock` (stacked). **No upstream PR.**
