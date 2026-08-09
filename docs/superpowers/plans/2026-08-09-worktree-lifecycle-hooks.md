# Git Worktree Lifecycle Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Native git-worktree awareness in infigraph — new worktrees bootstrap by cloning the main worktree's index and incrementally reindexing (instead of parsing from scratch), removed worktrees have their registry entry evicted automatically, both triggered near-instantly by a Claude Code hook and, as a fallback, by a periodic/manual reconciliation sweep.

**Architecture:** Five new pieces layered on existing machinery: a general-purpose `infigraph clone` copy primitive; a `worktree` detection module (git worktree list parsing, shared by both the action commands and `doctor`); three new `infigraph worktree {init,teardown,reconcile}` subcommands; a `doctor` check reusing the same detection; and a Claude Code PostToolUse hook (installed via the existing `infigraph install` hook-installer pattern) that calls `worktree init`/`teardown` right after `git worktree add`/`remove` succeeds.

**Tech Stack:** Rust (infigraph-core, infigraph-cli), clap `Subcommand` derive, bash (hook script, mirroring the existing `ENFORCE_HOOK_SCRIPT` idiom in `crates/infigraph-cli/src/hooks.rs`).

**Spec:** `docs/superpowers/specs/2026-08-09-worktree-lifecycle-hooks-design.md`

## Global Constraints

- **Backend scope: local (Kuzu/DaemonKuzu) mode only.** Nothing in this plan touches the Neo4j/remote backend.
- `infigraph clone` copies `.infigraph/` **excluding** `graph.lock`, `watch.lock`, `mcp.lock`, `index.lock`, and anything under `.infigraph/logs/` — a copied lock file would falsely claim the destination is held by the source's (possibly still-live) watcher PID.
- `infigraph clone` never indexes; indexing is always a separate, explicit step by the caller.
- `worktree teardown` **never deletes `.infigraph/`** — eviction from `registry.json` only. This was an explicit design decision (not the more destructive full-delete option).
- `worktree reconcile --global`: teardown candidates are **acted on** (safe, reversible); bootstrap candidates are **reported only**, never auto-indexed — auto-triggering an unknown number of index runs during a sweep is out of scope by design.
- `doctor` remains 100% read-only — its new check only detects and reports (with a remediation naming the exact subcommand to run), never mutates anything.
- Branch: `feat/hardening`, main checkout (no worktree needed — this work doesn't touch build artifacts in a way that needs isolation).
- Run tests per-crate (`cargo test -p infigraph-core --test <name>`, `cargo test -p infigraph-cli --test <name>`), not `--all`, per this machine's disk constraints. `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND` prefix on any test run that touches watchers/backend selection.
- `cargo fmt --all` before every commit (pre-commit hook enforces it).

---

### Task 1: `infigraph clone <src> <dst>` — the copy primitive

**Files:**
- Create: `crates/infigraph-core/src/clone.rs`
- Modify: `crates/infigraph-core/src/lib.rs` (add `pub mod clone;` alongside the other `pub mod` declarations)
- Create: `crates/infigraph-cli/src/clone_commands.rs`
- Modify: `crates/infigraph-cli/src/main.rs` (add `mod clone_commands;` near the other `mod` declarations at the top; add a `Clone { src: PathBuf, dst: PathBuf }` variant to the `Commands` enum, placed near `Delete` at line 631; wire dispatch in `run()`, lines 841-1107 — mirror the existing `Commands::Delete => ...` arm's shape, calling `clone_commands::cmd_clone(&src, &dst)`)
- Test: `crates/infigraph-core/tests/clone.rs`

**Interfaces:**
- Produces: `pub fn clone_infigraph_dir(src_root: &Path, dst_root: &Path) -> anyhow::Result<()>` in `infigraph_core::clone` — copies `<src_root>/.infigraph/` to `<dst_root>/.infigraph/` recursively, skipping `graph.lock`, `watch.lock`, `mcp.lock`, `index.lock`, and the `logs/` subdirectory entirely. Errors with a clear message if `<src_root>/.infigraph/` doesn't exist. Creates `<dst_root>/.infigraph/` if needed.
- Produces: `pub(crate) fn cmd_clone(src: &Path, dst: &Path) -> anyhow::Result<()>` in `infigraph_cli::clone_commands` — thin CLI wrapper calling `clone_infigraph_dir`, printing a summary line on success.
- Consumed by: Task 4 (`worktree init`).

- [ ] **Step 1: Write the failing tests**

```rust
// crates/infigraph-core/tests/clone.rs
use infigraph_core::clone::clone_infigraph_dir;
use std::fs;

fn write_file(path: &std::path::Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn clone_copies_graph_and_sidecars() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    write_file(&src.path().join(".infigraph/graph/data.kz"), "graph-bytes");
    write_file(&src.path().join(".infigraph/embeddings.bin"), "emb-bytes");
    write_file(&src.path().join(".infigraph/docs_embeddings.bin"), "doc-emb-bytes");

    clone_infigraph_dir(src.path(), dst.path()).unwrap();

    assert_eq!(
        fs::read_to_string(dst.path().join(".infigraph/graph/data.kz")).unwrap(),
        "graph-bytes"
    );
    assert_eq!(
        fs::read_to_string(dst.path().join(".infigraph/embeddings.bin")).unwrap(),
        "emb-bytes"
    );
    assert_eq!(
        fs::read_to_string(dst.path().join(".infigraph/docs_embeddings.bin")).unwrap(),
        "doc-emb-bytes"
    );
}

#[test]
fn clone_excludes_lock_files_and_logs() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    write_file(&src.path().join(".infigraph/graph.lock"), "pid:123");
    write_file(&src.path().join(".infigraph/watch.lock"), "pid:123");
    write_file(&src.path().join(".infigraph/mcp.lock"), "pid:123");
    write_file(&src.path().join(".infigraph/index.lock"), "pid:123");
    write_file(&src.path().join(".infigraph/logs/watch.log"), "log line");
    write_file(&src.path().join(".infigraph/embeddings.bin"), "emb-bytes");

    clone_infigraph_dir(src.path(), dst.path()).unwrap();

    assert!(!dst.path().join(".infigraph/graph.lock").exists());
    assert!(!dst.path().join(".infigraph/watch.lock").exists());
    assert!(!dst.path().join(".infigraph/mcp.lock").exists());
    assert!(!dst.path().join(".infigraph/index.lock").exists());
    assert!(!dst.path().join(".infigraph/logs").exists());
    assert!(dst.path().join(".infigraph/embeddings.bin").exists());
}

#[test]
fn clone_leaves_source_untouched() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(&src.path().join(".infigraph/embeddings.bin"), "emb-bytes");

    clone_infigraph_dir(src.path(), dst.path()).unwrap();

    assert_eq!(
        fs::read_to_string(src.path().join(".infigraph/embeddings.bin")).unwrap(),
        "emb-bytes"
    );
}

#[test]
fn clone_errors_clearly_when_source_has_no_infigraph_dir() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    let err = clone_infigraph_dir(src.path(), dst.path()).unwrap_err();
    assert!(err.to_string().contains(".infigraph"), "error should mention .infigraph: {err}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test clone`
Expected: FAIL to compile — `infigraph_core::clone` module doesn't exist yet.

- [ ] **Step 3: Implement `clone_infigraph_dir`**

```rust
// crates/infigraph-core/src/clone.rs
use std::path::Path;

use anyhow::{Context, Result};

/// Paths, relative to `.infigraph/`, excluded from a clone: lock files (a copied
/// lock would falsely claim the destination is held by the source's possibly-live
/// process) and logs (source-specific, meaningless at the destination).
const EXCLUDED_RELATIVE_PATHS: &[&str] = &[
    "graph.lock",
    "watch.lock",
    "mcp.lock",
    "index.lock",
    "logs",
];

/// Copy `<src_root>/.infigraph/` to `<dst_root>/.infigraph/`, excluding lock files
/// and logs. Does not index — the caller runs `infigraph index` afterward.
pub fn clone_infigraph_dir(src_root: &Path, dst_root: &Path) -> Result<()> {
    let src_dir = src_root.join(".infigraph");
    anyhow::ensure!(
        src_dir.is_dir(),
        "nothing to clone from: {} has no .infigraph directory",
        src_root.display()
    );

    let dst_dir = dst_root.join(".infigraph");
    std::fs::create_dir_all(&dst_dir)
        .with_context(|| format!("create {}", dst_dir.display()))?;

    copy_dir_excluding(&src_dir, &dst_dir, &src_dir)
}

fn copy_dir_excluding(src: &Path, dst: &Path, base: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if EXCLUDED_RELATIVE_PATHS.iter().any(|ex| rel == *ex) {
            continue;
        }

        let dst_path = dst.join(entry.file_name());
        if path.is_dir() {
            std::fs::create_dir_all(&dst_path)
                .with_context(|| format!("create {}", dst_path.display()))?;
            copy_dir_excluding(&path, &dst_path, base)?;
        } else {
            std::fs::copy(&path, &dst_path)
                .with_context(|| format!("copy {} to {}", path.display(), dst_path.display()))?;
        }
    }
    Ok(())
}
```

Add `pub mod clone;` to `crates/infigraph-core/src/lib.rs` next to the other `pub mod` declarations (find the existing list and insert alphabetically or near `pub mod doctor;`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test clone`
Expected: 4 passed.

- [ ] **Step 5: Add the CLI command**

```rust
// crates/infigraph-cli/src/clone_commands.rs
use std::path::Path;

use anyhow::Result;
use infigraph_core::clone::clone_infigraph_dir;

pub(crate) fn cmd_clone(src: &Path, dst: &Path) -> Result<()> {
    clone_infigraph_dir(src, dst)?;
    println!(
        "Cloned .infigraph/ from {} to {} (locks and logs excluded).",
        src.display(),
        dst.display()
    );
    Ok(())
}
```

In `crates/infigraph-cli/src/main.rs`:
1. Add `mod clone_commands;` near the top's existing `mod` list (alongside `mod agent;`, `mod analysis_commands;`, etc.).
2. In the `Commands` enum, near the `Delete` variant (around line 631), add:
   ```rust
   /// Copy an existing project's .infigraph/ index into a new location (excludes locks/logs, doesn't index)
   Clone {
       /// Source project root (must already have an indexed .infigraph/ directory)
       src: PathBuf,
       /// Destination project root
       dst: PathBuf,
   },
   ```
3. In `run()` (lines 841-1107), find the existing `Commands::Delete => { cmd_delete_project(...)?; }`-shaped match arm and add a sibling arm following the same pattern:
   ```rust
   Commands::Clone { src, dst } => {
       clone_commands::cmd_clone(&src, &dst)?;
   }
   ```

- [ ] **Step 6: Verify the CLI builds and the new command runs**

Run: `cargo build -p infigraph-cli && cargo fmt --all -- --check && cargo clippy -p infigraph-core -p infigraph-cli --all-targets -- -D warnings`
Expected: clean build, no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-core/src/clone.rs crates/infigraph-core/src/lib.rs \
        crates/infigraph-core/tests/clone.rs crates/infigraph-cli/src/clone_commands.rs \
        crates/infigraph-cli/src/main.rs
git commit -m "feat: add infigraph clone -- copy .infigraph/ excluding locks/logs"
```

---

### Task 2: `worktree` detection module

**Files:**
- Create: `crates/infigraph-core/src/worktree.rs`
- Modify: `crates/infigraph-core/src/lib.rs` (add `pub mod worktree;`)
- Test: `crates/infigraph-core/tests/worktree.rs`

**Interfaces:**
- Consumes: nothing new from Task 1.
- Produces (all `pub` in `infigraph_core::worktree`):
  - `pub fn git_common_dir(path: &Path) -> Result<PathBuf>` — runs `git rev-parse --git-common-dir` from `path`, resolves the result to an absolute path (git may print a relative path). Returns `Err` if `path` isn't inside a git repo.
  - `pub fn list_worktree_paths(path: &Path) -> Result<Vec<PathBuf>>` — runs `git worktree list --porcelain` from `path`, parses the `worktree <abs-path>` lines (one per entry, blank-line-separated in porcelain format) into an ordered `Vec<PathBuf>`. **The first element is always the main worktree** — this is git's documented behavior (main worktree listed first, unconditionally), but confirm it against this machine's actual `git --version` output while writing this task's tests, per the design doc's own flagged caveat.
  - `pub fn main_worktree_path(path: &Path) -> Result<PathBuf>` — `list_worktree_paths(path)?.into_iter().next()`, erroring clearly if the list is empty (shouldn't happen for a real repo, but don't panic).
  - `pub struct WorktreeDrift { pub bootstrap_candidates: Vec<PathBuf>, pub teardown_candidates: Vec<PathBuf> }`
  - `pub fn find_worktree_drift(registry: &infigraph_core::multi::Registry, repo_scope: Option<&Path>) -> WorktreeDrift` — groups `registry.repos.values()` by their `git_common_dir()` (skipping entries where that call errors — not every registered project is a git worktree candidate). When `repo_scope` is `Some(p)`, only considers the group whose common-dir matches `git_common_dir(p)`; when `None`, considers every group (the `--global` case). For each group: worktrees `list_worktree_paths` reports for that repo but with no matching registry entry **and no `.infigraph/` directory yet** go into `bootstrap_candidates` (an `.infigraph/`-having-but-unregistered directory is the pre-existing `check_project_registration` FAIL case in `doctor.rs` — not this feature's concern, don't duplicate it). Registry entries under that repo whose path is absent from `list_worktree_paths`'s result go into `teardown_candidates`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/infigraph-core/tests/worktree.rs
use infigraph_core::multi::{Registry, RepoEntry};
use infigraph_core::worktree::{find_worktree_drift, git_common_dir, list_worktree_paths, main_worktree_path};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

fn repo_entry(name: &str, path: &std::path::Path) -> RepoEntry {
    RepoEntry {
        name: name.to_string(),
        path: path.to_path_buf(),
        languages: vec!["python".to_string()],
        symbol_count: 1,
        module_count: 1,
        last_indexed_commit: None,
    }
}

fn empty_registry() -> Registry {
    Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    }
}

fn git(args: &[&str], cwd: &std::path::Path) {
    let status = Command::new("git").args(args).current_dir(cwd).status().unwrap();
    assert!(status.success(), "git {:?} failed in {}", args, cwd.display());
}

fn init_repo_with_worktree() -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
    let main = tempfile::tempdir().unwrap();
    git(&["init"], main.path());
    git(&["config", "user.email", "t@t.com"], main.path());
    git(&["config", "user.name", "t"], main.path());
    std::fs::write(main.path().join("a.txt"), "hello").unwrap();
    git(&["add", "a.txt"], main.path());
    git(&["commit", "-m", "init"], main.path());

    let parent = tempfile::tempdir().unwrap();
    let wt_path = parent.path().join("wt1");
    git(
        &["worktree", "add", "-b", "feature", wt_path.to_str().unwrap()],
        main.path(),
    );
    (main, parent, wt_path)
}

#[test]
fn list_worktree_paths_puts_main_worktree_first() {
    let (main, _parent, wt_path) = init_repo_with_worktree();
    let paths = list_worktree_paths(main.path()).unwrap();
    assert_eq!(paths.len(), 2);
    assert_eq!(paths[0].canonicalize().unwrap(), main.path().canonicalize().unwrap());
    assert!(paths.iter().any(|p| p.canonicalize().unwrap() == wt_path.canonicalize().unwrap()));
}

#[test]
fn main_worktree_path_resolves_from_the_linked_worktree_too() {
    let (main, _parent, wt_path) = init_repo_with_worktree();
    let resolved = main_worktree_path(&wt_path).unwrap();
    assert_eq!(resolved.canonicalize().unwrap(), main.path().canonicalize().unwrap());
}

#[test]
fn git_common_dir_matches_between_main_and_linked_worktree() {
    let (main, _parent, wt_path) = init_repo_with_worktree();
    assert_eq!(git_common_dir(main.path()).unwrap(), git_common_dir(&wt_path).unwrap());
}

#[test]
fn find_worktree_drift_flags_unindexed_worktree_as_bootstrap_candidate() {
    let (main, _parent, wt_path) = init_repo_with_worktree();
    // Registry knows about the main worktree only.
    let mut registry = empty_registry();
    registry.repos.insert("main-repo".to_string(), repo_entry("main-repo", main.path()));

    let drift = find_worktree_drift(&registry, Some(main.path()));
    assert_eq!(drift.bootstrap_candidates.len(), 1);
    assert_eq!(
        drift.bootstrap_candidates[0].canonicalize().unwrap(),
        wt_path.canonicalize().unwrap()
    );
    assert!(drift.teardown_candidates.is_empty());
}

#[test]
fn find_worktree_drift_flags_removed_worktree_as_teardown_candidate() {
    let (main, _parent, wt_path) = init_repo_with_worktree();
    git(&["worktree", "remove", "--force", wt_path.to_str().unwrap()], main.path());

    let mut registry = empty_registry();
    registry.repos.insert("main-repo".to_string(), repo_entry("main-repo", main.path()));
    registry.repos.insert("wt1".to_string(), repo_entry("wt1", &wt_path));

    let drift = find_worktree_drift(&registry, Some(main.path()));
    assert_eq!(drift.teardown_candidates.len(), 1);
    assert_eq!(drift.teardown_candidates[0], wt_path);
}
```

(`repo_entry`/`empty_registry` are the helpers defined at the top of this test file, above.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test worktree`
Expected: FAIL to compile — `infigraph_core::worktree` doesn't exist yet.

- [ ] **Step 3: Implement**

```rust
// crates/infigraph-core/src/worktree.rs
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::multi::Registry;

pub fn git_common_dir(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(path)
        .output()
        .context("run git rev-parse --git-common-dir")?;
    anyhow::ensure!(
        output.status.success(),
        "not a git repository (or git rev-parse failed): {}",
        path.display()
    );
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let common_dir = PathBuf::from(raw);
    let resolved = if common_dir.is_absolute() {
        common_dir
    } else {
        path.join(common_dir)
    };
    resolved
        .canonicalize()
        .with_context(|| format!("canonicalize git common dir for {}", path.display()))
}

pub fn list_worktree_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(path)
        .output()
        .context("run git worktree list --porcelain")?;
    anyhow::ensure!(
        output.status.success(),
        "git worktree list failed for {}",
        path.display()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut paths = Vec::new();
    for line in stdout.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            paths.push(PathBuf::from(p));
        }
    }
    Ok(paths)
}

pub fn main_worktree_path(path: &Path) -> Result<PathBuf> {
    list_worktree_paths(path)?
        .into_iter()
        .next()
        .context("git worktree list returned no entries")
}

#[derive(Debug, Default)]
pub struct WorktreeDrift {
    pub bootstrap_candidates: Vec<PathBuf>,
    pub teardown_candidates: Vec<PathBuf>,
}

pub fn find_worktree_drift(registry: &Registry, repo_scope: Option<&Path>) -> WorktreeDrift {
    let mut drift = WorktreeDrift::default();

    // Group registry entries by their repo's common-dir (skip non-git or unresolvable entries).
    let mut by_repo: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for entry in registry.repos.values() {
        if let Ok(common) = git_common_dir(&entry.path) {
            by_repo.entry(common).or_default().push(entry.path.clone());
        }
    }

    let scope_common = repo_scope.and_then(|p| git_common_dir(p).ok());

    for (common_dir, registered_paths) in &by_repo {
        if let Some(ref scope) = scope_common {
            if scope != common_dir {
                continue;
            }
        }
        // Any registered path under this repo can be used to list live worktrees.
        let Some(probe) = registered_paths.first() else { continue };
        let Ok(live) = list_worktree_paths(probe) else { continue };

        for live_path in &live {
            let registered = registered_paths.iter().any(|p| p == live_path);
            let has_infigraph = live_path.join(".infigraph").is_dir();
            if !registered && !has_infigraph {
                drift.bootstrap_candidates.push(live_path.clone());
            }
        }
        for reg_path in registered_paths {
            if !live.iter().any(|p| p == reg_path) {
                drift.teardown_candidates.push(reg_path.clone());
            }
        }
    }

    drift
}
```

Add `pub mod worktree;` to `crates/infigraph-core/src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test worktree`
Expected: 5 passed. If `list_worktree_paths_puts_main_worktree_first` fails on this machine's git version, that's a real, important finding — stop and report it rather than reordering the assertion to make it pass; it falsifies a load-bearing assumption the whole feature depends on (see the design doc's flagged caveat).

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/worktree.rs crates/infigraph-core/src/lib.rs \
        crates/infigraph-core/tests/worktree.rs
git commit -m "feat: add worktree detection module (git worktree list parsing, drift-finding)"
```

---

### Task 3: Extract `Registry::deregister_by_path`

**Files:**
- Modify: `crates/infigraph-core/src/multi/mod.rs` (find `impl Registry` — search for where `Registry::load`/`Registry::save` are defined — and add the new method there)
- Modify: `crates/infigraph-cli/src/info_commands.rs:744-797` (`cmd_delete_project`) — refactor to call the new method instead of its current inline filter+remove block
- Test: extend `crates/infigraph-core/tests/` with a focused test (create `crates/infigraph-core/tests/registry_deregister.rs` if no existing registry test file is a natural fit — check first)

**Interfaces:**
- Produces: `pub fn deregister_by_path(&mut self, path: &Path) -> Vec<String>` on `Registry` — removes every entry whose `path` equals `path`, or canonicalizes to the same location, returning the names removed (empty `Vec` if none matched). Does **not** call `save()` itself — caller decides when to persist, matching how `cmd_delete_project` already separately calls `registry.save()?` after its inline removal today.
- Consumed by: Task 5 (`worktree teardown`).

- [ ] **Step 1: Write the failing test**

First, read `cmd_delete_project` at `crates/infigraph-cli/src/info_commands.rs:744-797` to see its exact current inline logic (already shown in this plan's research, but re-read the live file before editing — line numbers may have shifted). Then write:

```rust
// crates/infigraph-core/tests/registry_deregister.rs
use infigraph_core::multi::{Registry, RepoEntry};
use std::collections::HashMap;
use std::path::PathBuf;

fn entry(name: &str, path: &str) -> RepoEntry {
    RepoEntry {
        name: name.to_string(),
        path: PathBuf::from(path),
        languages: vec!["python".to_string()],
        symbol_count: 1,
        module_count: 1,
        last_indexed_commit: None,
    }
}

fn empty_registry() -> Registry {
    Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    }
}

#[test]
fn deregister_by_path_removes_matching_entry_and_returns_its_name() {
    let mut registry = empty_registry();
    registry.repos.insert("proj-a".to_string(), entry("proj-a", "/tmp/proj-a"));
    registry.repos.insert("proj-b".to_string(), entry("proj-b", "/tmp/proj-b"));

    let removed = registry.deregister_by_path(&PathBuf::from("/tmp/proj-a"));

    assert_eq!(removed, vec!["proj-a".to_string()]);
    assert!(!registry.repos.contains_key("proj-a"));
    assert!(registry.repos.contains_key("proj-b"));
}

#[test]
fn deregister_by_path_returns_empty_when_nothing_matches() {
    let mut registry = empty_registry();
    registry.repos.insert("proj-a".to_string(), entry("proj-a", "/tmp/proj-a"));

    let removed = registry.deregister_by_path(&PathBuf::from("/tmp/does-not-exist"));

    assert!(removed.is_empty());
    assert_eq!(registry.repos.len(), 1);
}
```

(`Registry { repos, groups }` and `RepoEntry`'s 6 fields are the exact shapes already used by this codebase's own `crates/infigraph-core/tests/doctor.rs::repo_entry` helper and its `check_registry_global_scope_*` tests — confirmed during this plan's research, not guessed.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test registry_deregister`
Expected: FAIL to compile — `deregister_by_path` doesn't exist yet.

- [ ] **Step 3: Implement, extracted from `cmd_delete_project`'s existing logic**

In `crates/infigraph-core/src/multi/mod.rs`, inside `impl Registry` (find the block containing `load`/`save`), add:

```rust
    /// Remove every entry whose path matches `path` (raw or canonicalized).
    /// Returns the names removed. Does not persist -- call `save()` after.
    pub fn deregister_by_path(&mut self, path: &Path) -> Vec<String> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let to_remove: Vec<String> = self
            .repos
            .iter()
            .filter(|(_, entry)| {
                entry.path == path
                    || entry.path == canonical
                    || entry
                        .path
                        .canonicalize()
                        .map(|p| p == canonical)
                        .unwrap_or(false)
            })
            .map(|(name, _)| name.clone())
            .collect();

        for name in &to_remove {
            self.repos.remove(name);
        }
        to_remove
    }
```

Then refactor `cmd_delete_project` in `crates/infigraph-cli/src/info_commands.rs` to replace its inline filter+remove block with a call to this method:

```rust
    // Unregister from the global registry
    use infigraph_core::multi::Registry;
    let mut registry = Registry::load()?;
    let to_remove = registry.deregister_by_path(&project_path);
    registry.save()?;
```

(Keep the surrounding `println!` branches on `to_remove.is_empty()` exactly as they are today — only the removal logic itself moves.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test registry_deregister && cargo test -p infigraph-cli --lib info_commands`
Expected: new tests pass; no existing `info_commands`/delete-related tests regress. If there's an existing integration test exercising `infigraph delete` end-to-end (search for one before assuming none exists), run it too and confirm it still passes unmodified — this refactor must not change `Delete`'s observable behavior at all.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/multi/mod.rs crates/infigraph-cli/src/info_commands.rs \
        crates/infigraph-core/tests/registry_deregister.rs
git commit -m "refactor: extract Registry::deregister_by_path from cmd_delete_project"
```

---

### Task 4: `infigraph worktree init <path>`

**Files:**
- Create: `crates/infigraph-cli/src/worktree_commands.rs`
- Modify: `crates/infigraph-cli/src/main.rs` (add `mod worktree_commands;`; add a `Worktree { #[command(subcommand)] action: WorktreeAction }` variant to `Commands`, mirroring the existing `Pipeline { #[command(subcommand)] action: PipelineAction }` variant's shape; define `WorktreeAction` with an `Init { path: PathBuf }` case for now — `Teardown`/`Reconcile` are added in Tasks 5-6; wire dispatch in `run()`)
- Test: `crates/infigraph-cli/tests/worktree_init.rs`

**Interfaces:**
- Consumes: `infigraph_core::clone::clone_infigraph_dir` (Task 1), `infigraph_core::worktree::main_worktree_path` (Task 2).
- Produces: `pub(crate) fn cmd_worktree_init(path: &Path) -> anyhow::Result<()>` in `infigraph_cli::worktree_commands`.

- [ ] **Step 1: Write the failing integration test**

This mirrors the established pattern in `crates/infigraph-cli/tests/index_registers_repo.rs`
exactly: invoke the compiled binary via `env!("CARGO_BIN_EXE_infigraph")` (Cargo's
compile-time path to the workspace binary — no custom lookup helper needed), with
`HOME` overridden to a fake temp directory so the test never touches the real
`~/.infigraph/registry.json`.

```rust
// crates/infigraph-cli/tests/worktree_init.rs
use std::process::Command;

fn git(args: &[&str], cwd: &std::path::Path) {
    let status = Command::new("git").args(args).current_dir(cwd).status().unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn run_index(root: &std::path::Path, fake_home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["--root", root.to_str().unwrap(), "index"])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .unwrap()
}

fn run_worktree(action: &str, path: &std::path::Path, fake_home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["worktree", action, path.to_str().unwrap()])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .unwrap()
}

#[test]
fn worktree_init_clones_and_incrementally_indexes() {
    let fake_home = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    git(&["init"], main.path());
    git(&["config", "user.email", "t@t.com"], main.path());
    git(&["config", "user.name", "t"], main.path());
    std::fs::write(main.path().join("a.py"), "def foo():\n    pass\n").unwrap();
    git(&["add", "a.py"], main.path());
    git(&["commit", "-m", "init"], main.path());

    // Index the main worktree first (full embeddings, not --no-embed, since this
    // test verifies embedding-clone correctness).
    let out = run_index(main.path(), fake_home.path());
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(main.path().join(".infigraph/graph").exists());
    let main_embeddings = std::fs::read(main.path().join(".infigraph/embeddings.bin")).unwrap();

    // Create a linked worktree with identical content (no new commits on it yet).
    let parent = tempfile::tempdir().unwrap();
    let wt_path = parent.path().join("wt1");
    git(&["worktree", "add", "-b", "feature", wt_path.to_str().unwrap()], main.path());

    let out = run_worktree("init", &wt_path, fake_home.path());
    assert!(out.status.success(), "worktree init failed: {}", String::from_utf8_lossy(&out.stderr));

    assert!(wt_path.join(".infigraph/graph").exists());

    // Content-correctness check: since the new worktree has zero file differences
    // from main at creation time, its cloned-then-incrementally-reindexed
    // embeddings.bin must be byte-for-byte identical to main's -- not just
    // "close," which would be the case if vectors were regenerated instead of
    // reused (float re-embedding of identical text is deterministic here, but
    // exact byte equality is still the strongest available signal that the
    // clone was actually used rather than silently discarded).
    let wt_embeddings = std::fs::read(wt_path.join(".infigraph/embeddings.bin")).unwrap();
    assert_eq!(main_embeddings, wt_embeddings);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-cli --test worktree_init`
Expected: FAIL — `worktree` subcommand doesn't exist yet (clap will report an unrecognized subcommand, or the build fails once the `todo!()` above is replaced with real code and `worktree init` still isn't wired).

- [ ] **Step 3: Implement**

```rust
// crates/infigraph-cli/src/worktree_commands.rs
use std::path::Path;

use anyhow::Result;
use infigraph_core::clone::clone_infigraph_dir;
use infigraph_core::worktree::main_worktree_path;

use crate::index::cmd_index;

pub(crate) fn cmd_worktree_init(path: &Path) -> Result<()> {
    let main = main_worktree_path(path)?;

    if main != path && main.join(".infigraph").is_dir() {
        clone_infigraph_dir(&main, path)?;
        println!(
            "Cloned .infigraph/ from main worktree {} into {}.",
            main.display(),
            path.display()
        );
    }

    // Incremental index: content-hash comparison against the (possibly just-cloned)
    // graph means unchanged files are skipped automatically -- no separate "seeded"
    // code path needed here. Real signature confirmed in
    // crates/infigraph-cli/src/index.rs: fn cmd_index(root: &Path, full: bool, no_embed: bool).
    cmd_index(path, false, false)?;
    println!("Indexed {}.", path.display());
    Ok(())
}
```

`cmd_index` is currently `pub(crate)` in `crates/infigraph-cli/src/index.rs` — confirm it's visible from `worktree_commands.rs` (same crate, so `pub(crate)` should already suffice; if the module path requires it, add `pub(crate) use crate::index::cmd_index;` or call it as `crate::index::cmd_index(...)`).

In `crates/infigraph-cli/src/main.rs`:
1. Add `mod worktree_commands;`.
2. Add to `Commands`:
   ```rust
   /// Git worktree lifecycle: bootstrap, teardown, and reconcile registry+index state
   Worktree {
       #[command(subcommand)]
       action: WorktreeAction,
   },
   ```
3. Define, near wherever `PipelineAction` is defined:
   ```rust
   #[derive(Subcommand)]
   pub(crate) enum WorktreeAction {
       /// Bootstrap a newly created worktree: clone the main worktree's index, then incrementally reindex
       Init {
           /// Path to the new worktree
           path: PathBuf,
       },
   }
   ```
4. In `run()`, add:
   ```rust
   Commands::Worktree { action } => match action {
       WorktreeAction::Init { path } => {
           worktree_commands::cmd_worktree_init(&path)?;
       }
   },
   ```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo build -p infigraph-cli && cargo test -p infigraph-cli --test worktree_init`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-cli/src/worktree_commands.rs crates/infigraph-cli/src/main.rs \
        crates/infigraph-cli/tests/worktree_init.rs
git commit -m "feat: add infigraph worktree init -- clone main worktree + incremental reindex"
```

---

### Task 5: `infigraph worktree teardown <path>`

**Files:**
- Modify: `crates/infigraph-cli/src/worktree_commands.rs` (add `cmd_worktree_teardown`)
- Modify: `crates/infigraph-cli/src/main.rs` (add `WorktreeAction::Teardown { path: PathBuf }`, wire dispatch)
- Test: `crates/infigraph-cli/tests/worktree_teardown.rs`

**Interfaces:**
- Consumes: `Registry::deregister_by_path` (Task 3).
- Produces: `pub(crate) fn cmd_worktree_teardown(path: &Path) -> anyhow::Result<()>` in `infigraph_cli::worktree_commands`.

- [ ] **Step 1: Write the failing test**

`Registry::load`/`save` resolve a fixed `~/.infigraph/registry.json` path with no
in-process test-isolation seam (confirmed absent — see
`docs/superpowers/specs/2026-07-21-remaining-hardening-design.md`'s R-NEW.1). Do not
call `Registry::load()`/`.save()` directly from the test process, which would mutate
the real global registry. Instead use the same subprocess-with-`HOME`-override pattern
`worktree_init.rs` and `index_registers_repo.rs` already establish — the registry file
these subprocesses read/write lives under the fake `HOME`, never touching the real one.

```rust
// crates/infigraph-cli/tests/worktree_teardown.rs
use std::process::Command;

fn git(args: &[&str], cwd: &std::path::Path) {
    let status = Command::new("git").args(args).current_dir(cwd).status().unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn run_index(root: &std::path::Path, fake_home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["--root", root.to_str().unwrap(), "index"])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .unwrap()
}

fn run_worktree(action: &str, path: &std::path::Path, fake_home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["worktree", action, path.to_str().unwrap()])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .unwrap()
}

#[test]
fn worktree_teardown_evicts_registry_entry_but_keeps_infigraph_dir() {
    let fake_home = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    git(&["init"], main.path());
    git(&["config", "user.email", "t@t.com"], main.path());
    git(&["config", "user.name", "t"], main.path());
    std::fs::write(main.path().join("a.py"), "def foo():\n    pass\n").unwrap();
    git(&["add", "a.py"], main.path());
    git(&["commit", "-m", "init"], main.path());

    let parent = tempfile::tempdir().unwrap();
    let wt_path = parent.path().join("wt1");
    git(&["worktree", "add", "-b", "feature", wt_path.to_str().unwrap()], main.path());

    // Register the worktree the honest way: index it for real.
    let out = run_index(&wt_path, fake_home.path());
    assert!(out.status.success());
    let registry_path = fake_home.path().join(".infigraph/registry.json");
    let before = std::fs::read_to_string(&registry_path).unwrap();
    assert!(before.contains(wt_path.file_name().unwrap().to_str().unwrap()));

    let out = run_worktree("teardown", &wt_path, fake_home.path());
    assert!(out.status.success(), "teardown failed: {}", String::from_utf8_lossy(&out.stderr));

    let after = std::fs::read_to_string(&registry_path).unwrap();
    assert!(
        !after.contains(&wt_path.to_string_lossy().to_string()),
        "registry.json should no longer reference {}:\n{after}",
        wt_path.display()
    );
    assert!(
        wt_path.join(".infigraph/embeddings.bin").exists(),
        ".infigraph/ must survive teardown"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-cli --test worktree_teardown`
Expected: FAIL — `teardown` isn't a recognized `worktree` subcommand yet.

- [ ] **Step 3: Implement**

```rust
// Add to crates/infigraph-cli/src/worktree_commands.rs
use infigraph_core::multi::Registry;

pub(crate) fn cmd_worktree_teardown(path: &Path) -> Result<()> {
    // Stop the watcher, if any, before touching the registry -- mirrors the same
    // sentinel-based stop cmd_delete_project already uses in info_commands.rs.
    let lock_path = path.join(".infigraph").join("watch.lock");
    if crate::info_commands::watcher_is_alive(&lock_path) {
        let sentinel = path.join(".infigraph").join("watch.stop");
        let _ = std::fs::write(&sentinel, b"");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    let mut registry = Registry::load()?;
    let removed = registry.deregister_by_path(path);
    registry.save()?;

    if removed.is_empty() {
        println!("{} was not in the registry (nothing to evict).", path.display());
    } else {
        println!(
            "Evicted '{}' from the registry. .infigraph/ left on disk.",
            removed.join(", ")
        );
    }
    Ok(())
}
```

`watcher_is_alive` is currently private to `info_commands.rs` (used unqualified inside `cmd_delete_project`) — check its visibility and either make it `pub(crate)` in `info_commands.rs` or import it as shown, adjusting based on what actually compiles.

In `main.rs`, add to `WorktreeAction`:
```rust
    /// Clean up a removed worktree: stop its watcher, evict its registry entry (never deletes .infigraph/)
    Teardown {
        /// Path to the removed worktree
        path: PathBuf,
    },
```
And extend the `Commands::Worktree { action }` match:
```rust
    WorktreeAction::Teardown { path } => {
        worktree_commands::cmd_worktree_teardown(&path)?;
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo build -p infigraph-cli && cargo test -p infigraph-cli --test worktree_teardown`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-cli/src/worktree_commands.rs crates/infigraph-cli/src/main.rs \
        crates/infigraph-cli/tests/worktree_teardown.rs
git commit -m "feat: add infigraph worktree teardown -- stop watcher, evict registry entry"
```

---

### Task 6: `infigraph worktree reconcile [--global]` + `doctor`'s new check

**Files:**
- Modify: `crates/infigraph-cli/src/worktree_commands.rs` (add `cmd_worktree_reconcile`)
- Modify: `crates/infigraph-cli/src/main.rs` (add `WorktreeAction::Reconcile { global: bool }`, wire dispatch)
- Modify: `crates/infigraph-core/src/doctor.rs` (add `check_worktrees`)
- Test: `crates/infigraph-cli/tests/worktree_reconcile.rs`, extend `crates/infigraph-core/tests/doctor.rs`

**Interfaces:**
- Consumes: `find_worktree_drift` (Task 2), `cmd_worktree_teardown`'s underlying logic (Task 5 — call the same registry-eviction path, not a re-implementation).
- Produces: `pub(crate) fn cmd_worktree_reconcile(global: bool) -> anyhow::Result<()>` in `infigraph_cli::worktree_commands`; `pub fn check_worktrees(ctx: &DoctorContext) -> Vec<CheckResult>` in `infigraph_core::doctor`.

- [ ] **Step 1: Write the failing tests**

Reuses the `git()`/`run_index()`/`run_worktree()` helpers established in
`worktree_init.rs`/`worktree_teardown.rs` (copy them into this file the same way each
prior test file did — this codebase's convention is per-file helper duplication for
integration tests, not a shared test-utils crate; check whether that's still true by
looking at how `worktree_init.rs` and `worktree_teardown.rs` each define their own
copies before assuming a shared helper module exists).

```rust
// crates/infigraph-cli/tests/worktree_reconcile.rs
use std::process::Command;

fn git(args: &[&str], cwd: &std::path::Path) {
    let status = Command::new("git").args(args).current_dir(cwd).status().unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn run_index(root: &std::path::Path, fake_home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["--root", root.to_str().unwrap(), "index"])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .unwrap()
}

fn run_worktree_reconcile(cwd: &std::path::Path, fake_home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["worktree", "reconcile"])
        .current_dir(cwd)
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .unwrap()
}

#[test]
fn reconcile_acts_on_teardown_but_only_reports_bootstrap() {
    let fake_home = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    git(&["init"], main.path());
    git(&["config", "user.email", "t@t.com"], main.path());
    git(&["config", "user.name", "t"], main.path());
    std::fs::write(main.path().join("a.py"), "def foo():\n    pass\n").unwrap();
    git(&["add", "a.py"], main.path());
    git(&["commit", "-m", "init"], main.path());
    let out = run_index(main.path(), fake_home.path());
    assert!(out.status.success());

    let parent = tempfile::tempdir().unwrap();

    // Teardown candidate: register it, then remove the worktree without telling infigraph.
    let removed_wt = parent.path().join("removed-wt");
    git(&["worktree", "add", "-b", "gone", removed_wt.to_str().unwrap()], main.path());
    let out = run_index(&removed_wt, fake_home.path());
    assert!(out.status.success());
    git(&["worktree", "remove", "--force", removed_wt.to_str().unwrap()], main.path());

    // Bootstrap candidate: create a worktree but never index it.
    let new_wt = parent.path().join("new-wt");
    git(&["worktree", "add", "-b", "fresh", new_wt.to_str().unwrap()], main.path());

    let out = run_worktree_reconcile(main.path(), fake_home.path());
    assert!(out.status.success(), "reconcile failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Teardown candidate: actually evicted.
    let registry_content = std::fs::read_to_string(fake_home.path().join(".infigraph/registry.json")).unwrap();
    assert!(!registry_content.contains(&removed_wt.to_string_lossy().to_string()));

    // Bootstrap candidate: reported, but not auto-indexed.
    assert!(stdout.contains(&format!("infigraph worktree init {}", new_wt.display())));
    assert!(!new_wt.join(".infigraph").exists(), "reconcile must not auto-bootstrap");
}
```

```rust
// Add to crates/infigraph-core/tests/doctor.rs -- uses this file's own existing
// repo_entry() helper and ctx_for() helper (both already present, used by
// check_registry_global_scope_* above) plus a real temp git repo, since drift
// detection genuinely shells out to git.
#[test]
fn check_worktrees_warns_on_teardown_candidate() {
    let main = tempfile::TempDir::new().unwrap();
    let git = |args: &[&str]| {
        assert!(std::process::Command::new("git").args(args).current_dir(main.path()).status().unwrap().success());
    };
    git(&["init"]);
    git(&["config", "user.email", "t@t.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(main.path().join("a.py"), "x = 1\n").unwrap();
    git(&["add", "a.py"]);
    git(&["commit", "-m", "init"]);

    let parent = tempfile::TempDir::new().unwrap();
    let wt_path = parent.path().join("wt1");
    assert!(std::process::Command::new("git")
        .args(["worktree", "add", "-b", "gone", wt_path.to_str().unwrap()])
        .current_dir(main.path())
        .status()
        .unwrap()
        .success());
    assert!(std::process::Command::new("git")
        .args(["worktree", "remove", "--force", wt_path.to_str().unwrap()])
        .current_dir(main.path())
        .status()
        .unwrap()
        .success());

    let mut registry = Registry { repos: HashMap::new(), groups: HashMap::new() };
    registry.repos.insert("main-repo".to_string(), repo_entry("main-repo", main.path().to_str().unwrap()));
    registry.repos.insert("wt1".to_string(), repo_entry("wt1", wt_path.to_str().unwrap()));

    let ctx = ctx_for(DoctorScope::Project(main.path().to_path_buf()), registry);
    let results = check_worktrees(&ctx);

    let teardown_warning = results
        .iter()
        .find(|r| r.name.contains("wt1"))
        .expect("must flag the removed-but-registered worktree");
    assert_eq!(teardown_warning.status, CheckStatus::Warn);
    assert!(teardown_warning
        .remediation
        .as_ref()
        .unwrap()
        .contains("infigraph worktree teardown"));
}
```

This reuses `repo_entry`/`ctx_for`/`Registry`/`DoctorScope`/`CheckStatus` exactly as this
file's existing `check_registry_global_scope_*` tests already do (confirmed during this
plan's research) — check that file's imports at the top before adding this test, since
some of these may need adding to an existing `use` block rather than introduced fresh.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-cli --test worktree_reconcile && cargo test -p infigraph-core --test doctor check_worktrees`
Expected: FAIL — `reconcile` subcommand and `check_worktrees` don't exist yet.

- [ ] **Step 3: Implement**

```rust
// Add to crates/infigraph-cli/src/worktree_commands.rs
use infigraph_core::worktree::find_worktree_drift;

pub(crate) fn cmd_worktree_reconcile(global: bool) -> Result<()> {
    let registry = Registry::load()?;
    let scope = if global {
        None
    } else {
        Some(std::env::current_dir()?)
    };
    let drift = find_worktree_drift(&registry, scope.as_deref());

    for path in &drift.teardown_candidates {
        cmd_worktree_teardown(path)?;
    }

    if drift.bootstrap_candidates.is_empty() {
        println!("No unindexed worktrees found.");
    } else {
        println!("{} unindexed worktree(s) found:", drift.bootstrap_candidates.len());
        for path in &drift.bootstrap_candidates {
            println!("  run `infigraph worktree init {}` to bootstrap it", path.display());
        }
    }
    Ok(())
}
```

Add to `WorktreeAction` in `main.rs`:
```rust
    /// Reconcile the registry against live git worktrees: evict removed worktrees, report unindexed ones
    Reconcile {
        /// Sweep every registered project's repo instead of just the current one
        #[arg(long)]
        global: bool,
    },
```
And the match arm:
```rust
    WorktreeAction::Reconcile { global } => {
        worktree_commands::cmd_worktree_reconcile(global)?;
    }
```

In `crates/infigraph-core/src/doctor.rs`, add near `check_sidecars`/`check_one_sidecar`:
```rust
const WORKTREE_CATEGORY: &str = "worktrees";

pub fn check_worktrees(ctx: &DoctorContext) -> Vec<CheckResult> {
    let drift = crate::worktree::find_worktree_drift(&ctx.registry, None);
    let mut results = Vec::new();

    for path in &drift.bootstrap_candidates {
        results.push(CheckResult::warn(
            WORKTREE_CATEGORY,
            format!("{}: unindexed worktree", path.display()),
            "git worktree exists but has not been indexed",
            format!("run `infigraph worktree init {}`", path.display()),
        ));
    }
    for path in &drift.teardown_candidates {
        results.push(CheckResult::warn(
            WORKTREE_CATEGORY,
            format!("{}: removed worktree still registered", path.display()),
            "git no longer lists this worktree, but it has a registry entry",
            format!("run `infigraph worktree teardown {}`", path.display()),
        ));
    }
    results
}
```

Check `CheckResult::warn`'s exact parameter order/names against an existing call (e.g. inside `check_one_sidecar`, already shown earlier in this plan's research) before finalizing — match it exactly. Wire `check_worktrees(ctx)` into wherever `check_sidecars`/`check_project_registration` get called into the overall `run_doctor` results list (find that orchestration point in `doctor.rs` and add a sibling call).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo build -p infigraph-cli && cargo test -p infigraph-cli --test worktree_reconcile && cargo test -p infigraph-core --test doctor`
Expected: PASS, no regressions in `doctor.rs`'s existing tests.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-cli/src/worktree_commands.rs crates/infigraph-cli/src/main.rs \
        crates/infigraph-core/src/doctor.rs crates/infigraph-cli/tests/worktree_reconcile.rs \
        crates/infigraph-core/tests/doctor.rs
git commit -m "feat: add infigraph worktree reconcile --global and a matching doctor check"
```

---

### Task 7: Claude Code PostToolUse hook

**Files:**
- Modify: `crates/infigraph-cli/src/hooks.rs` (add `WORKTREE_HOOK_SCRIPT` const + `install_worktree_lifecycle_hook`)
- Modify: `crates/infigraph-cli/src/install.rs` (`cmd_install`, call the new installer alongside the existing `install_*_hook(&home)?` calls)

**Interfaces:**
- Produces: `pub(crate) fn install_worktree_lifecycle_hook(home: &Path) -> anyhow::Result<()>` in `infigraph_cli::hooks`, following the exact same shape as `install_enforcement_hook` (write script to `~/.claude/hooks/`, merge an entry into `~/.claude/settings.json`, idempotent).

- [ ] **Step 1: Write the failing tests**

Add to `crates/infigraph-cli/src/hooks.rs`'s existing `#[cfg(test)]` module (same file `install_enforcement_hook_creates_file_and_settings` lives in — this codebase keeps hook installer tests inline, not in a separate integration test file):

```rust
#[test]
fn install_worktree_lifecycle_hook_creates_file_and_settings() {
    let (_tmp, home) = setup_home();
    install_worktree_lifecycle_hook(&home).unwrap();

    let hook_path = home.join(".claude/hooks/infigraph-worktree.sh");
    assert!(hook_path.exists());
    let content = std::fs::read_to_string(&hook_path).unwrap();
    assert!(content.contains("worktree init"));
    assert!(content.contains("worktree teardown"));

    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let post_tool = settings["hooks"]["PostToolUse"].as_array().unwrap();
    assert_eq!(post_tool.len(), 1);
    assert_eq!(post_tool[0]["matcher"].as_str().unwrap(), "Bash");
}

#[test]
fn install_worktree_lifecycle_hook_idempotent() {
    let (_tmp, home) = setup_home();
    install_worktree_lifecycle_hook(&home).unwrap();
    install_worktree_lifecycle_hook(&home).unwrap();

    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let post_tool = settings["hooks"]["PostToolUse"].as_array().unwrap();
    assert_eq!(post_tool.len(), 1, "second install must not duplicate the hook entry");
}
```

`setup_home()` is the existing test helper `install_enforcement_hook_creates_file_and_settings` already uses — reuse it, don't redefine it.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-cli --lib hooks::tests::install_worktree_lifecycle_hook`
Expected: FAIL to compile — `install_worktree_lifecycle_hook` doesn't exist yet.

- [ ] **Step 3: Implement**

```rust
// Add to crates/infigraph-cli/src/hooks.rs, modeled directly on install_enforcement_hook
pub(crate) const WORKTREE_HOOK_SCRIPT: &str = r#"#!/usr/bin/env bash
# Infigraph PostToolUse hook -- git worktree lifecycle.
# Fires after every Bash tool call; only acts when the command looks like a
# `git worktree add|remove|prune` invocation that actually succeeded (exit 0).
# Rather than parse the triggering command's own arguments (which may use a
# default worktree name, a relative path, or --force flags in any order), it
# re-runs `git worktree list --porcelain` before and after and diffs the two
# -- the authoritative source of what actually changed, regardless of how the
# command was phrased.
input=$(cat)

tool=$(echo "$input" | jq -r '.tool_name // empty')
[ "$tool" = "Bash" ] || exit 0

cmd=$(echo "$input" | jq -r '.tool_input.command // empty')
echo "$cmd" | grep -qE '(^|\s)git\s+worktree\s+(add|remove|prune)(\s|$)' || exit 0

exit_code=$(echo "$input" | jq -r '.tool_response.exitCode // 0')
[ "$exit_code" = "0" ] || exit 0

cwd=$(echo "$input" | jq -r '.cwd // empty')
[ -n "$cwd" ] || exit 0

command -v infigraph >/dev/null 2>&1 || exit 0
git -C "$cwd" rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0

infigraph worktree reconcile >/dev/null 2>&1 &
disown
exit 0
"#;

pub(crate) fn install_worktree_lifecycle_hook(home: &std::path::Path) -> Result<()> {
    let hooks_dir = home.join(".claude").join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;

    let hook_path = hooks_dir.join("infigraph-worktree.sh");
    std::fs::write(&hook_path, WORKTREE_HOOK_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    }
    println!("  Installed worktree lifecycle hook: {}", hook_path.display());

    let settings_path = home.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.is_file() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    } else {
        json!({})
    };
    if settings.get("hooks").is_none() {
        settings["hooks"] = json!({});
    }

    let hook_entry = json!({
        "matcher": "Bash",
        "hooks": [{
            "type": "command",
            "command": hook_path.to_string_lossy(),
            "timeout": 5
        }]
    });

    let post_tool = settings["hooks"]
        .get("PostToolUse")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let already_exists = post_tool.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("infigraph-worktree"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !already_exists {
        let mut arr = post_tool;
        arr.push(hook_entry);
        settings["hooks"]["PostToolUse"] = serde_json::Value::Array(arr);
        let pretty = serde_json::to_string_pretty(&settings)?;
        std::fs::write(&settings_path, pretty)?;
        println!("  Added PostToolUse hook to {}", settings_path.display());
    } else {
        println!(
            "  PostToolUse worktree hook already configured in {}",
            settings_path.display()
        );
    }

    Ok(())
}
```

Note the deliberate simplification versus the original design doc: rather than the hook computing exactly which path was added/removed and calling `init`/`teardown` with it directly, it calls `infigraph worktree reconcile` (unscoped, so it defaults to the current repo per Task 6) in the background (`&`/`disown`, so the hook returns immediately and doesn't add latency to the triggering tool call). This is simpler to implement correctly in bash and just as effective: `reconcile` already does exactly "diff and act," which is exactly what just happened. If this feels like it defeats the "near-instant, scoped" framing from the design doc, that's a legitimate point to raise with the user before merging this task — flag it in the task's self-review rather than silently deviating.

- [ ] **Step 4: Wire into `cmd_install`**

In `crates/infigraph-cli/src/install.rs`, in `cmd_install()`, add alongside the existing hook-install calls:
```rust
    crate::hooks::install_worktree_lifecycle_hook(&home)?;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p infigraph-cli --lib hooks`
Expected: all `hooks` module tests pass, including the two new ones.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-cli/src/hooks.rs crates/infigraph-cli/src/install.rs
git commit -m "feat: install a PostToolUse hook that reconciles worktree state after git worktree add/remove"
```

---

### Task 8: Full verification sweep + docs

**Files:**
- Modify: `docs/CODE-PARSING.md` or `README.md` (brief mention of `infigraph clone`/`worktree` commands, matching the existing style of documenting CLI commands in this repo)
- Modify: `docs/DESIGN-hardening.md` (optional: note this closes part of the registry-rot problem R7.1 was tracking for the worktree case specifically — check whether R7.1's text should be annotated, don't invent a new requirement number)

- [ ] **Step 1: Full workspace verification**

```bash
cargo fmt --all -- --check
cargo clippy -p infigraph-core -p infigraph-cli --all-targets -- -D warnings
cargo test -p infigraph-core --test clone --test worktree --test registry_deregister --test doctor
cargo test -p infigraph-cli --test worktree_init --test worktree_teardown --test worktree_reconcile --lib hooks
```
Expected: all clean.

- [ ] **Step 2: Manual verification of the hook (named gap from the design doc)**

Run `infigraph install` in a scratch directory, inspect `~/.claude/hooks/infigraph-worktree.sh` and `~/.claude/settings.json`'s `PostToolUse` entry by hand, then in a real git repo run `git worktree add ../scratch-wt` followed by `git worktree remove ../scratch-wt` and confirm (via `infigraph doctor` or `cat ~/.infigraph/registry.json`) that reconcile actually ran. This step cannot be automated by the Rust test suite — record the result in the task's completion notes rather than skipping it silently.

- [ ] **Step 3: Docs**

Add a short section to whichever doc already lists CLI commands (check `docs/CODE-PARSING.md`'s existing "Registry" section, or `README.md`'s command table) describing `infigraph clone` and `infigraph worktree {init,teardown,reconcile}` in 2-3 sentences each, matching that doc's existing tone.

- [ ] **Step 4: Commit**

```bash
git add docs/
git commit -m "docs: document infigraph clone and worktree init/teardown/reconcile"
```
