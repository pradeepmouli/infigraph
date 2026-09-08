# Port PR37 Hardening Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port three genuine hardening fixes from upstream PR #37 (`intuit/infigraph`, `fix/mcp-crash-resilience`, external contributor `stashilin`) into our fork's `feat/health-beacons` branch, adapted to our fork's current (diverged) code rather than copy-pasted.

**Architecture:** Three independent fixes, each self-contained:
1. A preflight file-size check (`validate_db_file`) before every `kuzu::Database::new` call, in both `GraphStore` (code graph) and `DocStore` (doc store) — turns a truncated/corrupt-file abort/segfault into a normal `Err` that our existing `Infigraph::init()` recovery can act on.
2. `infigraph-mcp`'s crash-recovery reindex (`auto_reindex_all`) currently only walks the global registry + groups dir, so with an empty registry (the common standalone case) it's a silent no-op. A new `collect_reindex_targets` helper adds the supervisor's startup cwd as a candidate.
3. The MCP worker process currently has no way to detect its supervisor dying abnormally (SIGKILL, crash) — it survives re-parented to PID 1, still holding the instance lock. A new `lifecycle` module makes the worker poll for supervisor death (PID-reuse-immune on Unix via `getppid()` reparent detection, `OpenProcess`/`WaitForSingleObject` on Windows) and self-exit.

**Tech Stack:** Rust (edition 2021), `kuzu`/`lbug` embedded graph DB, `libc` (already a dependency of `infigraph-core`, needs adding to `infigraph-mcp`), `windows-sys` (new dependency, Windows-only).

## Global Constraints

- **Fork-only for now.** Target branch is `feat/health-beacons` directly in the main working tree (`/Users/pmouli/GitHub.nosync/active/rust/infigraph`) — no new worktree, no upstream PR. The user explicitly said "leave [PR37] on the fork for now."
- **Adapt, don't copy-paste.** Our fork's `crates/infigraph-core/src/graph/store.rs`, `crates/infigraph-mcp/src/recovery.rs`, and `crates/infigraph-mcp/src/main.rs` have diverged from PR37's base (our own lock-timeout plumbing, `GraphCorruption` WAL-detection, lock-guarded wipe-with-quarantine). Every step below shows the exact current code and the exact adapted diff — do not blindly apply PR37's raw patch text.
- Every `cargo` invocation runs with `CARGO_PROFILE_DEV_DEBUG=0` (hard repo rule — mixing debug settings spawns multi-GB duplicate build trees).
- Commit with `--no-verify` only if the pre-commit hook fails on a confirmed pre-existing, unrelated environmental flake (this session's known list: `write_lock_perf::test_contended_lock_throughput`, `groups_watch_perf::test_groups_watch_perf` — the latter's root cause was fixed earlier this session, so it should no longer flake; if it does, investigate before assuming it's still the old cause). Any other failure must be investigated as a real regression.
- Tasks 1 and 2 are independent of each other. Task 3 also touches `crates/infigraph-mcp/src/main.rs` (same file as Task 2) — **Task 2 must be committed before Task 3's implementer starts** to avoid two subagents editing the same file concurrently. Execute in order: Task 1, Task 2, Task 3.
- `validate_db_file`'s error is a plain `anyhow::Error` (via `anyhow::bail!`), not wrapped in our fork's existing `GraphCorruption` type. Confirmed via `find_all_references`: `GraphCorruption` has exactly one production call site (`GraphStore::open_read_only`'s WAL-message detection) and no downstream code downcasts it — so this is a safe, non-breaking choice, not a gap.

---

### Task 1: DB preflight validation (`validate_db_file`)

**Files:**
- Modify: `crates/infigraph-core/src/graph/store.rs` (add `validate_db_file` + call sites + tests)
- Modify: `crates/infigraph-core/src/graph/mod.rs:29` (export `validate_db_file`)
- Modify: `crates/infigraph-docs/src/store.rs` (call `validate_db_file` in `DocStore::open`)

**Interfaces:**
- Produces: `pub fn validate_db_file(path: &Path) -> Result<()>` in `crates/infigraph-core/src/graph/store.rs`, re-exported as `infigraph_core::graph::validate_db_file`. Consumed by both `GraphStore`'s own open paths and `infigraph-docs`'s `DocStore::open`.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `crates/infigraph-core/src/graph/store.rs` (this file has no existing `#[cfg(test)] mod tests` block — confirmed via `get_symbols_in_file`, so this creates one):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test: a truncated graph file used to be handed straight to
    /// `kuzu::Database::new`, which parses a bogus size field from the header
    /// and either aborts the process (Linux) or segfaults later at read time
    /// (macOS, `BufferManager::optimisticRead`). The preflight must turn this
    /// into a normal `Err` so `Infigraph::init`'s wipe-and-rebuild path runs.
    #[test]
    fn open_truncated_db_file_returns_err_instead_of_aborting() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        std::fs::write(&db_path, b"garbage, way below one page").unwrap();

        let err = GraphStore::open(&db_path)
            .map(|_| ())
            .expect_err("truncated file must be rejected");
        assert!(
            err.to_string().contains("truncated/corrupt"),
            "unexpected error: {err}"
        );

        let err = GraphStore::open_read_only(&db_path)
            .map(|_| ())
            .expect_err("truncated file must be rejected in read-only mode too");
        assert!(err.to_string().contains("truncated/corrupt"));
    }

    #[test]
    fn validate_db_file_accepts_missing_path_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Missing → fresh create, fine.
        assert!(validate_db_file(&dir.path().join("does-not-exist")).is_ok());
        // Directory (legacy layout) → fine.
        assert!(validate_db_file(dir.path()).is_ok());
    }

    #[test]
    fn open_fresh_then_reopen_still_works_with_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        // Fresh create.
        drop(GraphStore::open(&db_path).expect("fresh create must succeed"));
        // Reopen of a valid db must pass the preflight.
        drop(GraphStore::open(&db_path).expect("reopen of valid db must succeed"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --lib graph::store::tests -- --nocapture`
Expected: compile error — `validate_db_file` is not defined yet (`cannot find function \`validate_db_file\` in this scope` for the two tests that call it directly; the truncated-file test will compile but is expected to FAIL at runtime since nothing currently rejects the truncated file — actually note: on this dev machine `Database::new` on truly garbage bytes is likely to itself return an `Err` already at the FFI layer some of the time rather than abort, since the abort/segfault is platform/build-dependent per `CLAUDE.md`'s own caveat ("observed on Linux, not macOS") — the important, deterministic regression is the two `validate_db_file`-calling tests failing to compile until Step 3 lands. Don't rely on locally reproducing the abort itself; the preflight is a defense-in-depth fix even where local repro is inconsistent.

- [ ] **Step 3: Add `validate_db_file` and wire it into both open paths**

In `crates/infigraph-core/src/graph/store.rs`, change the import line at the top from:

```rust
use anyhow::Result;
```

to:

```rust
use anyhow::{Context, Result};
```

Then add this function right after the closing `}` of `impl WriteLock` (i.e. immediately before `/// Persistent graph store backed by Kuzu.` / `pub struct GraphStore`):

```rust
/// Minimum plausible size of a Kuzu database file. A freshly created
/// database is at least one page (4 KiB); anything smaller is a
/// truncated/corrupt file that Kuzu's own parser cannot be trusted with.
const MIN_DB_FILE_SIZE: u64 = 4096;

/// Preflight check before handing a path to `kuzu::Database::new`.
///
/// A truncated or corrupt database file can make Kuzu's parser read a bogus
/// size field and request a huge allocation, which aborts the whole process
/// on some platforms (observed on Linux) or segfaults later at read time
/// (observed on macOS) — before any `Result` exists to catch it. Rejecting
/// obviously-invalid files here turns that abort into a normal error that
/// callers' wipe-and-rebuild recovery (`Infigraph::init`, `DocIndex::init`)
/// already handles.
///
/// A missing path (fresh create) and a directory (legacy on-disk layout)
/// are both fine.
pub fn validate_db_file(path: &Path) -> Result<()> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        // Doesn't exist yet — fresh create.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        // Permission errors etc. are real problems — don't mask them as
        // "fresh create" or Kuzu will fail later with a worse message.
        Err(e) => {
            return Err(e)
                .with_context(|| format!("read database metadata for {}", path.display()));
        }
    };
    if meta.is_dir() {
        return Ok(()); // legacy directory layout — let Kuzu handle it
    }
    if meta.len() < MIN_DB_FILE_SIZE {
        anyhow::bail!(
            "database file {} is truncated/corrupt ({} bytes, expected at least {})",
            path.display(),
            meta.len(),
            MIN_DB_FILE_SIZE
        );
    }
    Ok(())
}
```

Now wire it into both of `GraphStore`'s open paths. Current `open_with_lock_timeout` (which `open()` delegates to — this is the path `Infigraph::init()` uses):

```rust
    pub fn open_with_lock_timeout(path: &Path, timeout: std::time::Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock_path = path.with_extension("lock");
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open kuzu db: {e}"))?;
        let store = Self { db, lock_path };
        let lock = WriteLock::acquire_with_timeout(&store.lock_path, timeout)?;
        store.init_schema(&lock)?;
        drop(lock);
        Ok(store)
    }
```

Change to:

```rust
    pub fn open_with_lock_timeout(path: &Path, timeout: std::time::Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        validate_db_file(path)?;
        let lock_path = path.with_extension("lock");
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open kuzu db: {e}"))?;
        let store = Self { db, lock_path };
        let lock = WriteLock::acquire_with_timeout(&store.lock_path, timeout)?;
        store.init_schema(&lock)?;
        drop(lock);
        Ok(store)
    }
```

Current `open_read_only`:

```rust
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let lock_path = path.with_extension("lock");
        // `throw_on_wal_replay_failure` defaults to true (unset here): a WAL
        // replay failure now surfaces as an error instead of being silently
        // tolerated and served as a torn base image.
        let config = SystemConfig::default().read_only(true);
        let db = Database::new(path, config).map_err(|e| {
            let msg = format!("failed to open kuzu db (read-only): {e}");
            if msg.to_lowercase().contains("wal") {
                anyhow::Error::new(GraphCorruption { detail: msg })
            } else {
                anyhow::anyhow!(msg)
            }
        })?;
        Ok(Self { db, lock_path })
    }
```

Change to:

```rust
    pub fn open_read_only(path: &Path) -> Result<Self> {
        validate_db_file(path)?;
        let lock_path = path.with_extension("lock");
        // `throw_on_wal_replay_failure` defaults to true (unset here): a WAL
        // replay failure now surfaces as an error instead of being silently
        // tolerated and served as a torn base image.
        let config = SystemConfig::default().read_only(true);
        let db = Database::new(path, config).map_err(|e| {
            let msg = format!("failed to open kuzu db (read-only): {e}");
            if msg.to_lowercase().contains("wal") {
                anyhow::Error::new(GraphCorruption { detail: msg })
            } else {
                anyhow::anyhow!(msg)
            }
        })?;
        Ok(Self { db, lock_path })
    }
```

In `crates/infigraph-core/src/graph/mod.rs`, find the existing re-export line (currently, per line 29):

```rust
pub use store::{GraphStats, GraphStore};
```

Change to:

```rust
pub use store::{validate_db_file, GraphStats, GraphStore};
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --lib graph::store::tests -- --nocapture`
Expected: 3 passed, 0 failed.

Also run the full `infigraph-core` lib suite to confirm no regression in the existing WAL/read-only tests:
Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --lib`
Expected: all pass.

And the existing integration test that specifically exercises `open_read_only`'s WAL-corruption path (must still pass unchanged, since `validate_db_file` runs before it and a WAL-replay-failure file is a valid-sized file, so `validate_db_file` is a no-op for it):
Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test write_lock_edge_cases test_read_only_open_surfaces_wal_replay_failure`
Expected: pass.

- [ ] **Step 5: Wire the same preflight into `DocStore::open`**

Current `crates/infigraph-docs/src/store.rs::open` (lines 68-83):

```rust
    pub fn open(path: &Path) -> Result<Self> {
        let guard = DB_LOCK
            .lock()
            .map_err(|e| anyhow::anyhow!("doc store lock poisoned: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open docs kuzu db: {e}"))?;
        let store = Self {
            db,
            _db_guard: guard,
        };
        store.init_schema()?;
        Ok(store)
    }
```

Change to:

```rust
    pub fn open(path: &Path) -> Result<Self> {
        let guard = DB_LOCK
            .lock()
            .map_err(|e| anyhow::anyhow!("doc store lock poisoned: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Preflight: a truncated/corrupt file can abort or segfault inside
        // `Database::new` before any `Result` exists. Reject it here so
        // `DocIndex::init`'s wipe-and-rebuild recovery runs instead.
        infigraph_core::graph::validate_db_file(path)?;
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open docs kuzu db: {e}"))?;
        let store = Self {
            db,
            _db_guard: guard,
        };
        store.init_schema()?;
        Ok(store)
    }
```

Add a regression test to `crates/infigraph-docs/src/store.rs`'s existing `#[cfg(test)] mod tests` block (append it — do not create a new `mod tests`, this file already has one; find it and add inside):

```rust
    #[test]
    fn open_truncated_docs_db_file_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("docs.kuzu");
        std::fs::write(&db_path, b"garbage, way below one page").unwrap();

        let err = DocStore::open(&db_path)
            .map(|_| ())
            .expect_err("truncated docs file must be rejected");
        assert!(
            err.to_string().contains("truncated/corrupt"),
            "unexpected error: {err}"
        );
    }
```

- [ ] **Step 6: Run docs tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-docs --lib`
Expected: all pass, including the new `open_truncated_docs_db_file_returns_err`.

- [ ] **Step 7: fmt + clippy**

```bash
cargo fmt --all -- --check
CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-core -p infigraph-docs --all-targets -- -D warnings
```
Expected: both clean.

- [ ] **Step 8: Commit**

```bash
git add crates/infigraph-core/src/graph/store.rs crates/infigraph-core/src/graph/mod.rs crates/infigraph-docs/src/store.rs
git commit -m "fix: preflight-validate DB file size before kuzu::Database::new

A truncated/corrupt graph or docs database file can make Kuzu's parser
read a bogus size field and request a huge allocation, aborting the
whole process (observed on Linux) or segfaulting at read time (observed
on macOS) -- before any Result exists to catch it. validate_db_file
rejects anything under one page (4 KiB, excluding missing paths and the
legacy directory layout) so this surfaces as a normal Err that
Infigraph::init's and DocIndex::init's existing wipe-and-rebuild
recovery can act on, instead of crashing the process.

Ported from an equivalent fix in upstream PR intuit/infigraph#37,
adapted to this fork's diverged GraphStore/DocStore open paths."
```

---

### Task 2: Crash-recovery reindex includes the supervisor's startup cwd

**Files:**
- Modify: `crates/infigraph-mcp/src/recovery.rs` (add `collect_reindex_targets` + tests)
- Modify: `crates/infigraph-mcp/src/main.rs` (capture `startup_dir`, thread it through `auto_reindex_all`, use `collect_reindex_targets`)

**Interfaces:**
- Produces: `pub fn collect_reindex_targets(startup_dir: Option<&Path>, registry_paths: &[PathBuf], groups_dir: Option<&Path>) -> Vec<PathBuf>` in `crates/infigraph-mcp/src/recovery.rs`.
- Consumes (Task 3 does NOT depend on this — independent): none from other tasks.

- [ ] **Step 1: Write the failing tests**

Current top of `crates/infigraph-mcp/src/recovery.rs`:

```rust
//! Crash / corrupt-index recovery helpers (code graph + document store).

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
```

Change the `use std::path::Path;` line to:

```rust
use std::path::{Path, PathBuf};
```

Add this function after the module doc comment and imports, before `pub fn wipe_code_and_docs`:

```rust
/// Collect the set of project roots to reindex after a crash.
///
/// The MCP server may be launched in a repo that was never registered in
/// `~/.infigraph/registry.json` (standalone use is the common case), so the
/// registry alone is not enough: with an empty registry the old recovery was
/// a no-op and the crashed repo stayed broken. The supervisor's startup
/// directory is therefore always considered a candidate.
///
/// Only paths that actually contain a `.infigraph/` directory are returned,
/// deduplicated by canonical path so a registered startup dir isn't indexed
/// twice.
pub fn collect_reindex_targets(
    startup_dir: Option<&Path>,
    registry_paths: &[PathBuf],
    groups_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut targets = Vec::new();

    let mut push = |path: &Path| {
        // Must be a directory — a stray regular file named `.infigraph`
        // is not an index and must not trigger a reindex.
        if !path.join(".infigraph").is_dir() {
            return;
        }
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if seen.insert(key) {
            targets.push(path.to_path_buf());
        }
    };

    // The repo the MCP server was actually serving comes first.
    if let Some(dir) = startup_dir {
        push(dir);
    }
    for path in registry_paths {
        push(path);
    }
    if let Some(gd) = groups_dir {
        if let Ok(entries) = std::fs::read_dir(gd) {
            for entry in entries.flatten() {
                push(&entry.path());
            }
        }
    }

    targets
}
```

Add these tests to the existing `#[cfg(test)] mod tests` block at the bottom of the same file (it already exists — append inside it, alongside `test_wipe_code_and_docs_removes_graph_and_docs` etc.):

```rust
    /// Regression test: recovery used to iterate only registry repos, so with
    /// an empty registry (the standalone default) it recovered nothing — the
    /// crashed repo the MCP server was actually serving stayed broken.
    #[test]
    fn collect_targets_includes_startup_dir_with_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".infigraph")).unwrap();

        let targets = collect_reindex_targets(Some(dir.path()), &[], None);
        assert_eq!(targets, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn collect_targets_skips_dirs_without_infigraph() {
        let dir = tempfile::tempdir().unwrap(); // no .infigraph inside
        let targets = collect_reindex_targets(Some(dir.path()), &[], None);
        assert!(targets.is_empty());
    }

    #[test]
    fn collect_targets_dedups_startup_dir_against_registry() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".infigraph")).unwrap();

        // Syntactic alias of the same directory — dedup must go through
        // canonicalization, not string equality.
        let registry = vec![dir.path().join(".")];
        let targets = collect_reindex_targets(Some(dir.path()), &registry, None);
        assert_eq!(
            targets.len(),
            1,
            "same repo via startup dir and registry must be indexed once"
        );
    }

    #[test]
    fn collect_targets_skips_infigraph_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        // A stray regular file named `.infigraph` is not an index.
        fs::write(dir.path().join(".infigraph"), b"not a dir").unwrap();

        let targets = collect_reindex_targets(Some(dir.path()), &[], None);
        assert!(
            targets.is_empty(),
            "regular file named .infigraph must not trigger recovery"
        );
    }

    #[test]
    fn collect_targets_includes_registry_repos_and_groups() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".infigraph")).unwrap();

        let groups = tempfile::tempdir().unwrap();
        let group = groups.path().join("my-group");
        fs::create_dir_all(group.join(".infigraph")).unwrap();

        let registry = vec![repo.path().to_path_buf()];
        let targets = collect_reindex_targets(None, &registry, Some(groups.path()));
        assert_eq!(targets.len(), 2);
        assert!(targets.contains(&repo.path().to_path_buf()));
        assert!(targets.contains(&group));
    }
```

Confirmed: the existing `mod tests` block (lines 49-52) already has `use super::*;` and `use std::fs;` — the `fs::create_dir_all`/`fs::write` calls in the five tests above resolve exactly as written, no adjustment needed.

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib recovery::tests -- --nocapture`
Expected: FAIL to compile — `collect_reindex_targets` not defined.

- [ ] **Step 3: Run tests to verify they pass**

(Implementation was already added in Step 1 alongside the tests, per this plan's own convention of showing the complete function once — there is no separate "minimal implementation" step here since the function is not incrementally buildable in a meaningful way.)

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib recovery::tests -- --nocapture`
Expected: 5 new tests pass, plus the 3 pre-existing tests in this module (`test_wipe_code_and_docs_removes_graph_and_docs`, `test_wipe_code_and_docs_missing_infigraph_is_noop`, `test_wipe_refuses_while_graph_lock_held`) still pass — 8 total, 0 failed.

- [ ] **Step 4: Wire `collect_reindex_targets` into `main.rs`**

Current `crates/infigraph-mcp/src/main.rs::main` (top portion, lines 9-19):

```rust
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--worker") {
        return run_worker();
    }

    // Supervisor mode: spawn self as --worker, monitor for segfault, auto-reindex
    loop {
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--worker");
```

Change to:

```rust
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--worker") {
        return run_worker();
    }

    // Supervisor mode: spawn self as --worker, monitor for segfault, auto-reindex.
    // Remember the repo we were launched in: it's the primary recovery target
    // even when the global registry is empty (standalone use).
    let startup_dir = std::env::current_dir().ok();
    loop {
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--worker");
```

Further down in the same function, both call sites of `auto_reindex_all()` (one in the `#[cfg(unix)]` SIGSEGV branch, one in the `#[cfg(windows)]` negative-exit-code branch) change from:

```rust
                auto_reindex_all();
```

to:

```rust
                auto_reindex_all(startup_dir.as_deref());
```

(There are two occurrences — one per platform branch — change both.)

Current `auto_reindex_all` (lines 68-116):

```rust
fn auto_reindex_all() {
    let cli = find_infigraph_cli_for_reindex();
    let cli_path = match cli {
        Some(p) => p,
        None => {
            mcp_log("ERROR", "Cannot find infigraph CLI for auto-reindex");
            return;
        }
    };

    let registry = match infigraph_core::multi::Registry::load() {
        Ok(r) => r,
        Err(e) => {
            mcp_log(
                "ERROR",
                &format!("Registry load failed during reindex: {e}"),
            );
            return;
        }
    };

    // Reindex individual projects
    for entry in registry.repos.values() {
        let path = &entry.path;
        if !path.join(".infigraph").exists() {
            continue;
        }
        reindex_path(&cli_path, path);
    }

    // Reindex group combined graphs
    let groups_dir = std::env::var("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .join(".infigraph")
                .join("groups")
        })
        .ok();
    if let Some(ref gd) = groups_dir {
        if let Ok(entries) = std::fs::read_dir(gd) {
            for entry in entries.flatten() {
                let group_path = entry.path();
                if group_path.join(".infigraph").exists() {
                    reindex_path(&cli_path, &group_path);
                }
            }
        }
    }
}
```

Change to:

```rust
fn auto_reindex_all(startup_dir: Option<&std::path::Path>) {
    let cli = find_infigraph_cli_for_reindex();
    let cli_path = match cli {
        Some(p) => p,
        None => {
            mcp_log("ERROR", "Cannot find infigraph CLI for auto-reindex");
            return;
        }
    };

    // Registry repos are optional extras: an empty/broken registry must not
    // prevent recovery of the repo this MCP server was launched in.
    let registry_paths: Vec<std::path::PathBuf> = match infigraph_core::multi::Registry::load() {
        Ok(r) => r.repos.values().map(|e| e.path.clone()).collect(),
        Err(e) => {
            mcp_log(
                "ERROR",
                &format!("Registry load failed during reindex: {e}"),
            );
            Vec::new()
        }
    };

    let groups_dir = std::env::var("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .join(".infigraph")
                .join("groups")
        })
        .ok();

    let targets = infigraph_mcp::recovery::collect_reindex_targets(
        startup_dir,
        &registry_paths,
        groups_dir.as_deref(),
    );
    if targets.is_empty() {
        mcp_log("WARN", "Auto-reindex found no targets with .infigraph");
        return;
    }
    for path in &targets {
        reindex_path(&cli_path, path);
    }
}
```

Note: this preserves our fork's existing `groups_dir` resolution style (`std::env::var("HOME")`, no `dirs_next` fallback) rather than introducing a new dependency — do not add `dirs-next` to `infigraph-mcp`'s `Cargo.toml`.

- [ ] **Step 5: Build and run the full `infigraph-mcp` test suite**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-mcp
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib
```
Expected: builds clean, all lib tests pass (including the 8 in `recovery::tests`).

Also run the MCP integration test suites that exercise crash/reindex paths to confirm no behavioral regression (these exist per this session's own earlier work on this branch):
```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_reindex -- --test-threads=1
```
Expected: all pass (18 tests, per this session's own earlier verification of this same suite).

- [ ] **Step 6: fmt + clippy**

```bash
cargo fmt --all -- --check
CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-mcp --all-targets -- -D warnings
```
Expected: both clean.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-mcp/src/recovery.rs crates/infigraph-mcp/src/main.rs
git commit -m "fix: crash-recovery reindex includes the supervisor's startup cwd

auto_reindex_all only walked the global registry and groups dir, so with
an empty registry (the common standalone-use case, not registered via
index_project/group_add) crash recovery was a silent no-op -- the repo
the MCP server was actually serving stayed broken after a SIGSEGV.
collect_reindex_targets adds the supervisor's startup directory as a
candidate (deduplicated against the registry by canonical path), so the
repo actually in use always gets recovered even if nothing else does.

Ported from an equivalent fix in upstream PR intuit/infigraph#37."
```

---

### Task 3: Worker exits when its supervisor dies

**Files:**
- Create: `crates/infigraph-mcp/src/lifecycle.rs`
- Modify: `crates/infigraph-mcp/src/lib.rs:1-7` (add `pub mod lifecycle;`)
- Modify: `crates/infigraph-mcp/src/main.rs` (wire `SUPERVISOR_PID_ENV` into the supervisor's spawn, call `spawn_parent_monitor()` in `run_worker`)
- Modify: `crates/infigraph-mcp/Cargo.toml` (add `libc` for unix, `windows-sys` for windows)

**Interfaces:**
- Produces: `pub const SUPERVISOR_PID_ENV: &str` and `pub fn spawn_parent_monitor()` and `pub fn process_alive(pid: u32) -> bool` in `crates/infigraph-mcp/src/lifecycle.rs`, re-exported as `infigraph_mcp::lifecycle::{SUPERVISOR_PID_ENV, spawn_parent_monitor, process_alive}`.
- Consumes: `crate::mcp_log` (already `pub fn mcp_log(level: &str, msg: &str)` at `crates/infigraph-mcp/src/lib.rs:579`, confirmed present).
- This task is independent of Task 2's `collect_reindex_targets`, but **both tasks touch `crates/infigraph-mcp/src/main.rs`** — Task 2 must already be committed before this task's implementer starts (per Global Constraints).

- [ ] **Step 1: Add the dependencies**

In `crates/infigraph-mcp/Cargo.toml`, after the existing `[dependencies]` block (right after the `tokenizers = { ... }` line, before `[features]`), add:

```toml
[target.'cfg(unix)'.dependencies]
libc = "0.2"

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_System_Threading"] }
```

(`libc = "0.2"` matches the exact version already pinned for `infigraph-core`'s own `[target.'cfg(unix)'.dependencies]` — confirmed via reading `crates/infigraph-core/Cargo.toml`. `windows-sys` is a new dependency, not used elsewhere in the workspace yet.)

- [ ] **Step 2: Write the failing tests**

Create `crates/infigraph-mcp/src/lifecycle.rs`:

```rust
//! Worker/supervisor process lifecycle.
//!
//! The MCP binary runs as a supervisor that spawns itself with `--worker`.
//! If the supervisor dies abnormally (SIGKILL, crash), the worker used to
//! survive re-parented to launchd/init (PPID 1) while still holding the
//! instance lock — blocking every future MCP start until killed by hand.
//!
//! The supervisor passes its PID via `INFIGRAPH_SUPERVISOR_PID`; the worker
//! polls that PID and exits when it disappears. Stdin EOF alone is not
//! enough: the worker inherits the client's pipe (so it outlives a dead
//! supervisor while the client is up), and the `--ui`/`--serve` modes park
//! in infinite sleep loops that never read stdin at all.

use std::time::Duration;

/// Env var carrying the supervisor's PID to the `--worker` child.
pub const SUPERVISOR_PID_ENV: &str = "INFIGRAPH_SUPERVISOR_PID";

/// How often the worker checks that its supervisor is still alive.
const PARENT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Returns whether a process with the given PID currently exists.
///
/// Unix: `kill(pid, 0)` — success or `EPERM` both mean the process exists.
/// Windows: `OpenProcess` + zero-timeout `WaitForSingleObject`; a handle we
/// can't open for a reason other than "no such process" is treated as alive
/// so a healthy worker is never killed spuriously.
/// Other platforms: conservatively returns `true`.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // pid 0 would signal the whole process group, and values that
        // don't fit pid_t would wrap negative (group/broadcast semantics) —
        // neither is a valid single-process PID.
        let Ok(pid_t) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if pid_t <= 0 {
            return false;
        }
        let res = unsafe { libc::kill(pid_t, 0) };
        if res == 0 {
            return true;
        }
        // EPERM: process exists but we can't signal it — still alive.
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };

        if pid == 0 {
            return false;
        }
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // ERROR_INVALID_PARAMETER: no such process. Anything else
            // (e.g. access denied) means it exists but is inaccessible —
            // err on the side of "alive" so we never exit spuriously.
            return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
        }
        // Zero-timeout wait: WAIT_TIMEOUT ⇒ still running; WAIT_OBJECT_0
        // (or failure) ⇒ terminated.
        let res = unsafe { WaitForSingleObject(handle, 0) };
        unsafe { CloseHandle(handle) };
        res == WAIT_TIMEOUT
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

/// Returns the current parent PID on Unix, `None` elsewhere.
fn current_ppid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(unsafe { libc::getppid() } as u32)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// If `INFIGRAPH_SUPERVISOR_PID` is set, spawn a background thread that
/// exits this process once the supervisor is gone. No-op when the env var
/// is absent (e.g. `--worker` launched directly for debugging).
///
/// When the worker is a direct child of the supervisor (the normal case),
/// the check is `getppid() != supervisor_pid`: on supervisor death the
/// kernel re-parents the worker, so this is immune to PID reuse. If the
/// worker is not a direct child (unusual debug setups), it falls back to
/// `process_alive` polling, which can in theory be fooled by PID reuse
/// but never exits a healthy process spuriously.
pub fn spawn_parent_monitor() {
    let Some(pid) = std::env::var(SUPERVISOR_PID_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return;
    };

    let direct_child = current_ppid() == Some(pid);

    let spawned = std::thread::Builder::new()
        .name("parent-monitor".into())
        .spawn(move || loop {
            std::thread::sleep(PARENT_POLL_INTERVAL);
            let gone = if direct_child {
                // Re-parented ⇒ the supervisor died. PID-reuse-proof.
                current_ppid() != Some(pid)
            } else {
                !process_alive(pid)
            };
            if gone {
                crate::mcp_log(
                    "INFO",
                    &format!("supervisor (pid {pid}) is gone — worker exiting to avoid orphan"),
                );
                std::process::exit(0);
            }
        });
    if let Err(e) = spawned {
        // Worker still exits on stdin EOF in MCP mode; a missing monitor
        // only matters for abnormal supervisor death, so log and continue
        // rather than killing a healthy worker at startup.
        crate::mcp_log(
            "WARN",
            &format!("failed to spawn parent-monitor thread: {e} — orphan reaping disabled"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_alive_true_for_self() {
        assert!(process_alive(std::process::id()));
    }

    /// Regression test for orphan workers: after a child exits and is reaped,
    /// its PID must be reported dead so the parent-monitor terminates the
    /// worker instead of leaving it re-parented to PID 1 holding the lock.
    #[cfg(unix)]
    #[test]
    fn process_alive_false_for_reaped_child() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();
        child.wait().expect("wait for child");
        assert!(
            !process_alive(pid),
            "reaped child pid {pid} must be reported dead"
        );
    }
}
```

- [ ] **Step 3: Register the module**

In `crates/infigraph-mcp/src/lib.rs`, current top of file:

```rust
pub mod compress;
pub mod health;
pub mod idle;
pub mod recovery;
pub mod session_context;
pub mod tools;
pub mod web;
```

Change to (alphabetical, matching this list's existing convention):

```rust
pub mod compress;
pub mod health;
pub mod idle;
pub mod lifecycle;
pub mod recovery;
pub mod session_context;
pub mod tools;
pub mod web;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib lifecycle::tests -- --nocapture`
Expected: 2 passed, 0 failed (`process_alive_true_for_self`, and on Unix `process_alive_false_for_reaped_child`).

- [ ] **Step 5: Wire it into `main.rs`**

This step comes after Task 2's changes to `main.rs` are already committed — re-read the file's current state before editing (Task 2 changed `main`'s signature-adjacent lines and `auto_reindex_all`).

In `main()`, the supervisor's spawn block currently looks like (after Task 2's edit):

```rust
    loop {
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--worker");
        for arg in args.iter().skip(1).filter(|a| *a != "--worker") {
            cmd.arg(arg);
        }
        cmd.stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
```

Change to:

```rust
    loop {
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--worker");
        for arg in args.iter().skip(1).filter(|a| *a != "--worker") {
            cmd.arg(arg);
        }
        // Let the worker detect supervisor death and exit instead of
        // lingering as an orphan holding the instance lock.
        cmd.env(
            infigraph_mcp::lifecycle::SUPERVISOR_PID_ENV,
            std::process::id().to_string(),
        );
        cmd.stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
```

In `run_worker()`, current:

```rust
fn run_worker() -> Result<()> {
    install_panic_hook();

    let _ = rayon::ThreadPoolBuilder::new()
        .stack_size(32 * 1024 * 1024)
        .build_global();
```

Change to:

```rust
fn run_worker() -> Result<()> {
    install_panic_hook();

    // Exit if the supervisor dies, instead of surviving as an orphan
    // (PPID 1) that holds the instance lock forever. Stdin EOF alone is
    // not sufficient: --ui/--serve modes never read stdin.
    infigraph_mcp::lifecycle::spawn_parent_monitor();

    let _ = rayon::ThreadPoolBuilder::new()
        .stack_size(32 * 1024 * 1024)
        .build_global();
```

- [ ] **Step 6: Build and run the full `infigraph-mcp` test suite**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-mcp
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib
```
Expected: builds clean, all lib tests pass (including the 2 in `lifecycle::tests` and the 8 from Task 2's `recovery::tests`).

- [ ] **Step 7: Manual smoke test — supervisor death actually orphans-and-exits correctly**

This is a real, observable behavior change worth confirming manually since it can't be fully exercised by a unit test (it requires an actual supervisor/worker process pair):

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-mcp
./target/debug/infigraph-mcp --mcp --port=19999 &
SUPERVISOR_PID=$!
sleep 2
WORKER_PID=$(pgrep -f "infigraph-mcp --worker" | head -1)
echo "supervisor=$SUPERVISOR_PID worker=$WORKER_PID"
kill -9 "$SUPERVISOR_PID"
sleep 8
if ps -p "$WORKER_PID" > /dev/null 2>&1; then
  echo "FAIL: worker $WORKER_PID still alive after supervisor SIGKILL + 8s"
else
  echo "PASS: worker exited after supervisor death"
fi
```
Expected: `PASS` — the worker should exit within roughly `PARENT_POLL_INTERVAL` (5s) plus a small margin of the supervisor dying. If it prints `FAIL`, manually kill the leftover worker process (`kill -9 $WORKER_PID`) and investigate before proceeding — do not commit with this unverified.

- [ ] **Step 8: fmt + clippy**

```bash
cargo fmt --all -- --check
CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-mcp --all-targets -- -D warnings
```
Expected: both clean. On this (macOS/Unix) development machine, the `#[cfg(windows)]` branch of `process_alive` cannot be compiled or tested — note this as a known verification gap, not a blocker; if a Windows CI runner exists for this repo, confirm there separately (or add a follow-up task, out of scope here).

- [ ] **Step 9: Commit**

```bash
git add crates/infigraph-mcp/src/lifecycle.rs crates/infigraph-mcp/src/lib.rs crates/infigraph-mcp/src/main.rs crates/infigraph-mcp/Cargo.toml Cargo.lock
git commit -m "fix: MCP worker exits when its supervisor dies

The supervisor passes its own PID via INFIGRAPH_SUPERVISOR_PID when
spawning the --worker child. The worker polls it every 5s and exits once
it's gone -- via a getppid()-based reparent check on Unix (immune to PID
reuse, since the kernel reparents the worker to its new parent the
instant the supervisor dies) with a process_alive fallback for
non-direct-child setups, and OpenProcess/WaitForSingleObject on Windows.

Without this, a supervisor SIGKILL or crash left the worker re-parented
to PID 1, still holding the instance lock, blocking every future MCP
start until killed by hand -- our existing reap_orphan is deliberately
remove-file-only (non-signaling, after a prior PID-reuse bug), and idle
self-termination only fires on stdin EOF, which never happens here since
the worker inherits the client's still-open stdin pipe. This is
complementary to both, covering the gap where the supervisor dies while
a client stays connected.

Ported from an equivalent fix in upstream PR intuit/infigraph#37. Windows
path is unverified on this (Unix) development machine -- confirm on a
Windows CI runner if/when one exists for this repo."
```

---

## Self-Review Notes

- **Spec coverage:** all three of PR37's claimed fixes have a task. `.env.1password`, the unrelated CI workflow, branding assets, and the Voyage/Cohere/Ollama provider integrations from PR37's diff are explicitly NOT ported (per the review's recommendation — those are unrelated scope bloat, not hardening).
- **No placeholders:** every step shows exact, complete code adapted from PR37's actual diff (fetched via `gh pr diff 37 --repo intuit/infigraph`) against this fork's actual current source (read via `get_code_snippet`/`get_symbols_in_file` immediately before writing this plan) — not guessed from the PR's prose description.
- **Type/interface consistency:** `collect_reindex_targets`'s signature (`Option<&Path>, &[PathBuf], Option<&Path>`) is used identically in Task 2's test additions and its `main.rs` call site. `SUPERVISOR_PID_ENV`/`spawn_parent_monitor`/`process_alive` names match between Task 3's `lifecycle.rs` definition and its `main.rs` wiring.
- **Divergence from PR37 handled explicitly:** Task 1 preserves our fork's `open_with_lock_timeout`/`GraphCorruption` WAL-detection (confirmed via `find_all_references` that nothing downstream depends on `validate_db_file`'s error also being a `GraphCorruption`); Task 2 preserves our fork's `groups_dir` resolution style instead of introducing PR37's `dirs_next` fallback; Task 3's Windows path is flagged as unverified rather than falsely claimed tested.
