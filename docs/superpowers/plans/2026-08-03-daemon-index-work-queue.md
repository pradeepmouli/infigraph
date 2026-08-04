# Daemon Index Work-Queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Coordinate the daemon watch loop's graph-mutating triggers (periodic reindex, watch-triggered file changes/removals, and four ad-hoc daemon-protocol write types) through one shared `IndexWorkQueue`, so overlapping work on the same file coalesces into a single execution instead of racing against a stale plan — closing a live-reproduced Kùzu duplicate-primary-key bug.

**Architecture:** A new `IndexWorkQueue` type (Task 1) accumulates pending work from all in-scope producers. A unified drain function (Task 2) executes one combined pass — extract, upsert, remove, resolve — against whatever's accumulated. Task 3 wires every producer in `watch_project_with_periodic` to push into the queue instead of executing inline, draining synchronously; this is the task that fixes the live bug. Task 4 adds `tokio`-based background draining so the loop stays responsive while a large drain executes, justified by lbug's verified multithreaded-connection safety guarantee.

**Tech Stack:** Rust, `infigraph-core`'s existing `watch`/`daemon_protocol` modules, `tokio` (new unconditional dependency, Task 4 only).

## Global Constraints

- Full design rationale, verified facts, and rejected alternatives live in `docs/superpowers/specs/2026-08-03-daemon-index-work-queue-design.md` — read it before starting if anything below seems under-explained.
- lbug (the actual dependency, `lbug = 0.16.0` aliased as `kuzu`) guarantees safe concurrent read/write queries from multiple threads/connections **within one process's Database object only** — verified against both `docs.kuzudb.com/concurrency` and `docs.ladybugdb.com/concurrency`. Never rely on this across processes; `.infigraph/index.lock` and `.infigraph/graph.lock` remain the cross-process mechanism.
- Scope is exactly: periodic reindex, watch-triggered batch reindex, watch-triggered file removal, and `WriteRequest::Index`/`UpsertFilesBulk`/`RemoveFiles`/`ResolveCalls`. The other 9 `WriteRequest` variants keep calling `serve_one_request` exactly as today — do not touch `serve_one_request`'s existing match arms for them.
- Never set `CARGO_TARGET_DIR` explicitly. Use `env -u INFIGRAPH_WATCH_DAEMON` for any watcher/daemon test run (globally exported in the dev machine's shell profile, breaks tests that assert on its absence). On `ENOSPC`, run `cargo clean --manifest-path <repo>/Cargo.toml --target-dir <repo>/.shared-target` (worktrees) or `cargo clean` (a non-worktree checkout, which has its own separate `target/`) and retry — never assume a full clean fixed a genuine compile bug.
- Every task's diff must leave `cargo clippy --all-targets -- -D warnings` and `cargo fmt -- --check` clean, and must not change the observable behavior of any `WriteRequest` variant outside the four in scope.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/infigraph-core/src/watch/queue.rs` (new) | `IndexWorkQueue`, `PendingIndexItem`, `Waiter`, `WaiterKind`, `DrainedQueue` — the pending-work data structure and its accumulation rules. No filesystem/database access, no `root` dependency — pure, unit-testable. |
| `crates/infigraph-core/src/watch/drain.rs` (new) | The unified drain execution function (Task 2) that turns a `DrainedQueue` into graph writes, and the reply-writing logic for waiters. |
| `crates/infigraph-core/src/lib.rs` | `Infigraph::index_via_backend` gets a scan/hash-diff/extract helper (`scan_changed_files`) extracted out of it (Task 2), reused by both `index_via_backend` (unchanged behavior) and the drain's `whole_project` handling. |
| `crates/infigraph-core/src/watch/mod.rs` | `watch_project_with_periodic`'s periodic/batch-flush/request-serving/removal blocks are rewired to push into a shared `IndexWorkQueue` instead of executing inline (Task 3), then upgraded to drain via a background `tokio` task (Task 4). |
| `crates/infigraph-core/Cargo.toml` | `tokio` added as an unconditional dependency, minimal features (Task 4). |

---

### Task 1: `IndexWorkQueue` core type

**Files:**
- Create: `crates/infigraph-core/src/watch/queue.rs`
- Modify: `crates/infigraph-core/src/watch/mod.rs:1-2` (register the new module)

**Interfaces:**
- Consumes: `crate::model::FileExtraction` (existing type, `.file: String` field — the relative path used as this queue's canonical key throughout).
- Produces: `pub(crate) struct IndexWorkQueue`, `pub(crate) enum PendingIndexItem { Raw, Structured(FileExtraction), ResolveOnly(FileExtraction) }`, `pub(crate) enum WaiterKind { Index, UpsertFilesBulk, RemoveFiles, ResolveCalls }`, `pub(crate) struct Waiter { pub kind: WaiterKind, pub use_learned: bool, pub reply_path: PathBuf }`, `pub(crate) struct DrainedQueue { pub items: HashMap<String, PendingIndexItem>, pub removals: HashSet<String>, pub whole_project: bool, pub waiters: Vec<Waiter> }`. Task 2 consumes `DrainedQueue` and `PendingIndexItem`/`Waiter`/`WaiterKind` directly.

**Design note, deviating from the spec's sketch:** the spec described `Raw(PathBuf)`. Keying by relative path (a `String`, matching `FileExtraction.file`'s type and how `GraphBackend::remove_file`/`get_file_hashes` already address files) is required for the eviction rules to actually compare "is this the same file" correctly across `Raw` entries (which start life as absolute fsevent paths) and `Structured`/`ResolveOnly` entries (keyed by `FileExtraction.file`, always relative). Normalizing to relative happens at the call site in Task 3 (where `root` is in scope), not inside this type — `IndexWorkQueue` never needs to know the project root. `Raw` therefore carries no payload (the map key already *is* the path); `index_files`'s existing extraction logic already accepts a relative path and joins it against `root` itself.

- [ ] **Step 1: Write the failing unit tests**

```rust
// crates/infigraph-core/src/watch/queue.rs
use crate::model::FileExtraction;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// A single file's pending index-shaped work, before it's known whether the
/// file needs a fresh disk read (`Raw`) or already carries pre-parsed
/// content from a client that did its own local parsing (`Structured`,
/// `ResolveOnly`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PendingIndexItem {
    /// Needs fresh extraction from disk at drain time.
    Raw,
    /// Pre-parsed by a client (`UpsertFilesBulk`); needs both upsert and resolve.
    Structured(FileExtraction),
    /// Pre-parsed by a client (`ResolveCalls`) whose content was already
    /// upserted by an earlier, separate drain -- needs resolve only, must
    /// not trigger a redundant re-upsert.
    ResolveOnly(FileExtraction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaiterKind {
    Index,
    UpsertFilesBulk,
    RemoveFiles,
    ResolveCalls,
}

/// An ad-hoc daemon-protocol caller blocked on a reply for the drain this
/// waiter was folded into.
#[derive(Debug, Clone)]
pub(crate) struct Waiter {
    pub kind: WaiterKind,
    /// `ResolveCalls` waiters only -- ignored for other kinds.
    pub use_learned: bool,
    pub reply_path: PathBuf,
}

/// The full state popped off an `IndexWorkQueue` by `drain()`, ready for
/// the unified drain execution (Task 2) to run against.
#[derive(Debug, Default)]
pub(crate) struct DrainedQueue {
    pub items: HashMap<String, PendingIndexItem>,
    pub removals: HashSet<String>,
    pub whole_project: bool,
    pub waiters: Vec<Waiter>,
}

/// Shared accumulator for index-shaped work across the daemon watch loop's
/// producers (periodic reindex, watch-triggered batch/removal, and four
/// ad-hoc `WriteRequest` variants). Has no timer of its own -- producers own
/// their own timing (see the design spec's "Debounce ownership" section);
/// this type just merges whatever's been contributed since the last drain.
#[derive(Debug, Default)]
pub(crate) struct IndexWorkQueue {
    items: HashMap<String, PendingIndexItem>,
    removals: HashSet<String>,
    whole_project: bool,
    waiters: Vec<Waiter>,
}

impl IndexWorkQueue {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Marks `rel_path` as needing fresh extraction from disk. Cancels any
    /// pending removal for the same path (the file apparently exists again)
    /// and supersedes any `Structured`/`ResolveOnly` entry -- freshness
    /// always wins over a possibly-stale pre-parsed extraction, matching
    /// the "reopen fresh rather than trust a cached view" precedent from
    /// `DaemonKuzuBackend`'s read-staleness fix.
    pub(crate) fn add_raw(&mut self, rel_path: String) {
        self.removals.remove(&rel_path);
        self.items.insert(rel_path, PendingIndexItem::Raw);
    }

    /// Adds a pre-parsed extraction that needs both upsert and resolve
    /// (`UpsertFilesBulk`). A no-op if a `Raw` entry already exists for this
    /// path (that entry will already trigger a fresh extraction, which
    /// supersedes this possibly-stale one). Overwrites any `ResolveOnly`
    /// entry, since `Structured` is the stronger requirement.
    pub(crate) fn add_structured(&mut self, extraction: FileExtraction) {
        let rel_path = extraction.file.clone();
        self.removals.remove(&rel_path);
        if matches!(self.items.get(&rel_path), Some(PendingIndexItem::Raw)) {
            return;
        }
        self.items.insert(rel_path, PendingIndexItem::Structured(extraction));
    }

    /// Adds a pre-parsed extraction that only needs resolution
    /// (`ResolveCalls`, whose content was already upserted by an earlier,
    /// separate request/drain). A no-op if *any* entry already exists for
    /// this path -- `Raw`/`Structured` will already resolve it as part of
    /// their own upsert; adding `ResolveOnly` on top would be redundant,
    /// never wrong-but-cheaper.
    pub(crate) fn add_resolve_only(&mut self, extraction: FileExtraction) {
        let rel_path = extraction.file.clone();
        self.removals.remove(&rel_path);
        self.items
            .entry(rel_path)
            .or_insert(PendingIndexItem::ResolveOnly(extraction));
    }

    /// Marks `rel_path` for removal. Always wins over any pending index
    /// intent for the same path -- the file is gone, indexing it makes no
    /// sense regardless of what was queued moments before.
    pub(crate) fn add_removal(&mut self, rel_path: String) {
        self.items.remove(&rel_path);
        self.removals.insert(rel_path);
    }

    /// The drain step will additionally compute the full changed-file set
    /// (a whole-project scan + hash-diff), same as `Infigraph::index()`
    /// does today, in addition to whatever's explicitly queued.
    pub(crate) fn mark_whole_project(&mut self) {
        self.whole_project = true;
    }

    /// Registers a reply target for the next drain this queue produces.
    pub(crate) fn add_waiter(&mut self, waiter: Waiter) {
        self.waiters.push(waiter);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
            && self.removals.is_empty()
            && !self.whole_project
            && self.waiters.is_empty()
    }

    /// Returns and clears the full accumulated state in one shot.
    pub(crate) fn drain(&mut self) -> DrainedQueue {
        DrainedQueue {
            items: std::mem::take(&mut self.items),
            removals: std::mem::take(&mut self.removals),
            whole_project: std::mem::replace(&mut self.whole_project, false),
            waiters: std::mem::take(&mut self.waiters),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extraction(file: &str) -> FileExtraction {
        FileExtraction {
            file: file.to_string(),
            language: "python".to_string(),
            content_hash: "deadbeef".to_string(),
            symbols: Vec::new(),
            relations: Vec::new(),
            statements: Vec::new(),
        }
    }

    #[test]
    fn add_raw_evicts_an_existing_structured_entry_for_the_same_path() {
        let mut q = IndexWorkQueue::new();
        q.add_structured(extraction("a.py"));
        q.add_raw("a.py".to_string());
        let drained = q.drain();
        assert_eq!(drained.items.get("a.py"), Some(&PendingIndexItem::Raw));
    }

    #[test]
    fn add_structured_after_add_raw_is_a_no_op() {
        let mut q = IndexWorkQueue::new();
        q.add_raw("a.py".to_string());
        q.add_structured(extraction("a.py"));
        let drained = q.drain();
        assert_eq!(
            drained.items.get("a.py"),
            Some(&PendingIndexItem::Raw),
            "Raw must survive a later add_structured for the same path"
        );
    }

    #[test]
    fn add_structured_overwrites_an_existing_resolve_only_entry() {
        let mut q = IndexWorkQueue::new();
        q.add_resolve_only(extraction("a.py"));
        q.add_structured(extraction("a.py"));
        let drained = q.drain();
        assert!(matches!(
            drained.items.get("a.py"),
            Some(PendingIndexItem::Structured(_))
        ));
    }

    #[test]
    fn add_resolve_only_is_a_no_op_when_a_raw_entry_already_exists() {
        let mut q = IndexWorkQueue::new();
        q.add_raw("a.py".to_string());
        q.add_resolve_only(extraction("a.py"));
        let drained = q.drain();
        assert_eq!(drained.items.get("a.py"), Some(&PendingIndexItem::Raw));
    }

    #[test]
    fn add_resolve_only_is_a_no_op_when_a_structured_entry_already_exists() {
        let mut q = IndexWorkQueue::new();
        q.add_structured(extraction("a.py"));
        q.add_resolve_only(extraction("a.py"));
        let drained = q.drain();
        assert!(matches!(
            drained.items.get("a.py"),
            Some(PendingIndexItem::Structured(_))
        ));
    }

    #[test]
    fn add_removal_clears_any_pending_index_entry_for_the_same_path() {
        let mut q = IndexWorkQueue::new();
        q.add_raw("a.py".to_string());
        q.add_removal("a.py".to_string());
        let drained = q.drain();
        assert!(!drained.items.contains_key("a.py"));
        assert!(drained.removals.contains("a.py"));
    }

    #[test]
    fn add_raw_cancels_a_pending_removal_for_the_same_path() {
        let mut q = IndexWorkQueue::new();
        q.add_removal("a.py".to_string());
        q.add_raw("a.py".to_string());
        let drained = q.drain();
        assert!(!drained.removals.contains("a.py"));
        assert_eq!(drained.items.get("a.py"), Some(&PendingIndexItem::Raw));
    }

    #[test]
    fn is_empty_and_drain_round_trip() {
        let mut q = IndexWorkQueue::new();
        assert!(q.is_empty());

        q.add_raw("a.py".to_string());
        assert!(!q.is_empty());

        let drained = q.drain();
        assert_eq!(drained.items.len(), 1);
        assert!(q.is_empty(), "drain must clear all accumulated state");
    }

    #[test]
    fn mark_whole_project_is_reflected_in_the_drained_snapshot_and_reset_after() {
        let mut q = IndexWorkQueue::new();
        q.mark_whole_project();
        assert!(!q.is_empty());
        let drained = q.drain();
        assert!(drained.whole_project);
        assert!(q.is_empty(), "whole_project flag must reset after drain");
    }

    #[test]
    fn waiters_accumulate_across_multiple_add_waiter_calls_before_one_drain() {
        let mut q = IndexWorkQueue::new();
        q.add_waiter(Waiter {
            kind: WaiterKind::Index,
            use_learned: false,
            reply_path: PathBuf::from("/tmp/a.result"),
        });
        q.add_waiter(Waiter {
            kind: WaiterKind::ResolveCalls,
            use_learned: true,
            reply_path: PathBuf::from("/tmp/b.result"),
        });
        let drained = q.drain();
        assert_eq!(drained.waiters.len(), 2);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail (module doesn't exist yet)**

Run: `cargo test -p infigraph-core watch::queue:: 2>&1 | head -20`
Expected: FAIL with a compile error — `watch::queue` module not found.

- [ ] **Step 3: Register the module**

Modify `crates/infigraph-core/src/watch/mod.rs`, the top of the file:

```rust
pub mod batch;
pub mod daemon;
pub(crate) mod queue;
```

(This is a one-line addition after the existing `pub mod daemon;` line — the file's other contents are untouched by this step.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core watch::queue::`
Expected: `test result: ok. 10 passed; 0 failed`

- [ ] **Step 5: Clippy and fmt**

Run: `cargo clippy -p infigraph-core --all-targets -- -D warnings && cargo fmt -- --check`
Expected: both clean.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/watch/queue.rs crates/infigraph-core/src/watch/mod.rs
git commit -m "feat: add IndexWorkQueue core type for daemon index coordination"
```

---

### Task 2: Scan-helper extraction + unified drain execution function

**Files:**
- Modify: `crates/infigraph-core/src/lib.rs` (extract `scan_changed_files` out of `index_via_backend`, `crates/infigraph-core/src/lib.rs:460-567`)
- Create: `crates/infigraph-core/src/watch/drain.rs`
- Modify: `crates/infigraph-core/src/watch/mod.rs` (register the new module)
- Test: `crates/infigraph-core/tests/watch_drain.rs` (new)

**Interfaces:**
- Consumes: `DrainedQueue`/`PendingIndexItem`/`Waiter`/`WaiterKind` (Task 1); `Infigraph::backend()`, `GraphBackend::{get_file_hashes, upsert_files_bulk, remove_file, resolve_calls}` (all existing, unchanged signatures); `crate::daemon_protocol::{WriteResult, write_atomic}` (existing).
- Produces: `Infigraph::scan_changed_files(&self, backend: &dyn GraphBackend) -> Result<ScanResult>` (new, `pub(crate)`) where `ScanResult { total_files: usize, extractions: Vec<FileExtraction>, stale_files: Vec<String> }`; `pub(crate) fn execute_drain(infigraph: &Infigraph, drained: DrainedQueue) -> Result<DrainOutcome>` in the new `watch/drain.rs`, where `DrainOutcome` carries enough to feed the batch-flush path's existing downstream steps (embedding updates, `on_event` callbacks) — Task 3 consumes this.

**Why this order:** this task makes the coalescing logic itself correct and testable *before* Task 3 wires it into the real loop, so the fix's core claim (one combined execution, no stale-plan collision) has its own gate, independent of the loop-restructuring risk in Task 3.

- [ ] **Step 1: Extract `scan_changed_files` from `index_via_backend`, behavior-preserving**

Read the current `crates/infigraph-core/src/lib.rs:460-567` (`index_via_backend`) before editing — this step must not change what it returns or what it writes, only where the scan logic lives. Replace it with:

```rust
    /// Scans every file under `self.root`, hash-diffs against what the
    /// backend already has, and extracts only what changed. Does not write
    /// anything -- callers (both `index_via_backend` and the daemon drain's
    /// whole-project handling) own the upsert/resolve/prune steps, so this
    /// stays a pure "what needs work" computation reusable by both.
    fn scan_changed_files(&self, backend: &dyn graph::GraphBackend) -> Result<ScanResult> {
        let files = self.collect_files()?;
        let total = files.len();

        let existing_hashes = backend.get_file_hashes().unwrap_or_default();

        let ns = &self.namespace;
        let done = std::sync::atomic::AtomicUsize::new(0);
        let extractions: Vec<FileExtraction> = files
            .par_iter()
            .filter_map(|path| {
                let raw_rel = path
                    .strip_prefix(&self.root)
                    .ok()?
                    .to_string_lossy()
                    .replace('\\', "/");
                let rel_path = match ns {
                    Some(prefix) => format!("{}/{}", prefix, raw_rel),
                    None => raw_rel.clone(),
                };
                let source = std::fs::read(path).ok()?;
                let hash = {
                    let mut h = Sha256::new();
                    h.update(&source);
                    format!("{:x}", h.finalize())
                };
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let pct = n * 100 / total;
                let prev_pct = (n - 1) * 100 / total;
                if (pct / 25) > (prev_pct / 25) || n == total {
                    eprintln!("Parsing: {}/{} ({}%)", n, total, pct);
                }
                if existing_hashes.get(&rel_path).map(|s| s.as_str()) == Some(hash.as_str()) {
                    return None;
                }
                let pack = self.registry.for_file_with_content(&rel_path, &source)?;
                extract::extract_file(&rel_path, &source, pack).ok()
            })
            .collect();

        let current_files: std::collections::HashSet<String> = files
            .iter()
            .filter_map(|p| {
                p.strip_prefix(&self.root).ok().map(|r| {
                    let raw = r.to_string_lossy().replace('\\', "/");
                    match ns {
                        Some(prefix) => format!("{}/{}", prefix, raw),
                        None => raw,
                    }
                })
            })
            .collect();
        let stale_files: Vec<String> = existing_hashes
            .keys()
            .filter(|k| !current_files.contains(k.as_str()))
            .cloned()
            .collect();

        Ok(ScanResult {
            total_files: total,
            extractions,
            stale_files,
        })
    }

    fn index_via_backend(&self, backend: &dyn graph::GraphBackend) -> Result<IndexResult> {
        let scan = self.scan_changed_files(backend)?;
        let indexed = scan.extractions.len();

        if !scan.extractions.is_empty() {
            eprintln!("Writing: {} files (backend bulk)", indexed);
            let write_start = std::time::Instant::now();
            backend.upsert_files_bulk(&scan.extractions, backend.get_file_hashes().unwrap_or_default().is_empty())?;
            eprintln!("Write complete: {}s", write_start.elapsed().as_secs());
        }

        if !scan.stale_files.is_empty() {
            eprintln!("[index] pruning {} stale file(s) from graph", scan.stale_files.len());
            for f in &scan.stale_files {
                let _ = backend.remove_file(f);
            }
        }

        let resolve_start = std::time::Instant::now();
        if !scan.extractions.is_empty() {
            eprintln!("Resolving: calls + inheritance for {} files", indexed);
        }
        let resolve_stats = backend
            .resolve_calls(&scan.extractions, None)
            .unwrap_or_else(|e| {
                eprintln!("warning: call resolution failed: {e}");
                resolve::ResolveStats {
                    total_calls: 0,
                    resolved: 0,
                    unresolved: 0,
                    learned_resolved: 0,
                    inherits_resolved: 0,
                }
            });
        if !scan.extractions.is_empty() {
            eprintln!(
                "Resolve complete: {}s ({} resolved, {} unresolved)",
                resolve_start.elapsed().as_secs(),
                resolve_stats.resolved,
                resolve_stats.unresolved
            );
        }

        Ok(IndexResult {
            total_files: scan.total_files,
            indexed_files: indexed,
            extractions: scan.extractions,
            resolve_stats,
        })
    }
```

Add the `ScanResult` type near `IndexResult`'s existing definition in `lib.rs` (same visibility as `IndexResult` — check its exact declaration at the top of `lib.rs` and match it; it's `pub struct IndexResult { ... }` with public fields, consumed outside the crate, so `ScanResult` only needs `pub(crate)` since it's an internal seam):

```rust
pub(crate) struct ScanResult {
    pub total_files: usize,
    pub extractions: Vec<FileExtraction>,
    pub stale_files: Vec<String>,
}
```

**Note on the one behavior change this introduces:** `index_via_backend`'s original code computed `existing_hashes.is_empty()` once and reused that single boolean for the `upsert_files_bulk` call. The extracted version above recomputes `backend.get_file_hashes()` a second time to get that boolean, since `scan_changed_files` doesn't expose its internal `existing_hashes` map. This is an extra read-only backend call on every `index_via_backend` invocation — cheap (it's the same call `scan_changed_files` already made once), but not free. If this shows up as a real cost in Task 2's testing, thread `existing_hashes_empty: bool` through `ScanResult` instead of recomputing; the code above chose recomputation for a smaller diff, since correctness doesn't depend on which approach is used.

- [ ] **Step 2: Run existing tests to verify the extraction is behavior-preserving**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib 2>&1 | tail -20`
Expected: all passing, same pass count as before this change (this refactor must not change `index_via_backend`'s observable behavior — if any test that exercises `index()`/`index_via_backend` fails, the extraction introduced a real bug, not just a refactor).

- [ ] **Step 3: Write the failing regression test for the unified drain (the actual bug this whole plan fixes)**

```rust
// crates/infigraph-core/tests/watch_drain.rs
//! Regression coverage for the daemon index-work-queue coalescing fix,
//! driving IndexWorkQueue + execute_drain directly -- no real daemon
//! process needed to prove the coalescing logic itself is correct. See
//! docs/superpowers/specs/2026-08-03-daemon-index-work-queue-design.md.

use infigraph_core::lang::LanguageRegistry;
use infigraph_core::Infigraph;
use std::fs;

fn open_project(root: &std::path::Path) -> Infigraph {
    let mut prism = Infigraph::open(root, LanguageRegistry::new()).unwrap();
    prism.init().unwrap();
    prism
}

#[test]
fn a_raw_entry_and_an_index_waiter_for_the_same_new_file_coalesce_into_one_execution() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("existing.py"), "def existing():\n    pass\n").unwrap();

    let prism = open_project(root);
    // Bootstrap: one file already indexed, matching the live repro's setup
    // (a project that already has SOME content before the race file appears).
    prism.index().unwrap();

    // The exact scenario from the live repro: a new file appears, and
    // BEFORE the watcher's own debounce would have settled, an ad-hoc
    // Index request also targets it -- both land in the SAME queue before
    // any execution happens.
    fs::write(root.join("fourth.py"), "def fourth():\n    pass\n").unwrap();

    let mut queue = infigraph_core::watch::queue::IndexWorkQueue::new();
    queue.add_raw("fourth.py".to_string());

    let result_path = tmp.path().join("waiter.result");
    queue.add_waiter(infigraph_core::watch::queue::Waiter {
        kind: infigraph_core::watch::queue::WaiterKind::Index,
        use_learned: false,
        reply_path: result_path.clone(),
    });

    let drained = queue.drain();
    let outcome = infigraph_core::watch::drain::execute_drain(&prism, drained).unwrap();

    // Exactly one extraction/upsert occurred for fourth.py -- the whole
    // point of this test. Before the fix, this scenario (a Raw entry
    // already queued, plus a second, independent decision to index the
    // same file) produced TWO separate index_files calls and a duplicate
    // primary key error.
    assert_eq!(outcome.extractions.len(), 1);
    assert_eq!(outcome.extractions[0].file, "fourth.py");

    // The waiter's reply reflects the real combined execution.
    let reply_contents = fs::read_to_string(&result_path).unwrap();
    let reply: infigraph_core::daemon_protocol::WriteResult =
        serde_json::from_str(&reply_contents).unwrap();
    match reply {
        infigraph_core::daemon_protocol::WriteResult::Ok { indexed_files, .. } => {
            assert_eq!(indexed_files, 1);
        }
        other => panic!("expected WriteResult::Ok, got {other:?}"),
    }
}

#[test]
fn resolve_only_extractions_do_not_trigger_a_redundant_upsert() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("a.py"), "def a():\n    pass\n").unwrap();
    let prism = open_project(root);
    prism.index().unwrap();

    let backend = prism.backend().unwrap();
    let existing_hash_count_before = backend.get_file_hashes().unwrap().len();

    let mut queue = infigraph_core::watch::queue::IndexWorkQueue::new();
    let extraction = infigraph_core::model::FileExtraction {
        file: "a.py".to_string(),
        language: "python".to_string(),
        content_hash: "whatever".to_string(),
        symbols: Vec::new(),
        relations: Vec::new(),
        statements: Vec::new(),
    };
    queue.add_resolve_only(extraction);
    let drained = queue.drain();
    let outcome = infigraph_core::watch::drain::execute_drain(&prism, drained).unwrap();

    // resolve_only contributes to the resolve pass, not the upsert pass --
    // outcome.extractions (the upsert set) must be empty, even though the
    // resolve step ran against a.py.
    assert!(
        outcome.extractions.is_empty(),
        "ResolveOnly items must not appear in the upsert extraction set"
    );
    assert_eq!(
        backend.get_file_hashes().unwrap().len(),
        existing_hash_count_before,
        "a redundant upsert would not change the hash count here, but IS a real wasted write -- \
         this assertion documents the guarantee this test exists to protect"
    );
}
```

- [ ] **Step 4: Run the tests to verify they fail (`watch::drain` doesn't exist yet)**

Run: `cargo test -p infigraph-core --test watch_drain 2>&1 | head -20`
Expected: FAIL with a compile error — `infigraph_core::watch::drain` not found.

- [ ] **Step 5: Write `execute_drain`**

```rust
// crates/infigraph-core/src/watch/drain.rs
use crate::daemon_protocol::{write_atomic, WriteResult};
use crate::model::FileExtraction;
use crate::resolve::ResolveStats;
use crate::watch::queue::{DrainedQueue, PendingIndexItem, Waiter, WaiterKind};
use crate::Infigraph;
use anyhow::Result;

/// What the unified drain actually did, for the batch-flush path's
/// existing downstream steps (embedding updates, cross-file-call event
/// emission) to consume -- mirrors the subset of `IndexResult` those steps
/// already use.
pub(crate) struct DrainOutcome {
    pub extractions: Vec<FileExtraction>,
    pub resolve_stats: ResolveStats,
}

/// Runs one combined pass -- extract, upsert, remove, resolve -- against
/// everything a `DrainedQueue` accumulated, and replies to every waiter
/// folded into it. This is the fix for the coalescing bug: there is
/// exactly one execution here, computed fresh against the drained
/// snapshot, so no operation plans against information another operation
/// has since made stale.
pub(crate) fn execute_drain(infigraph: &Infigraph, drained: DrainedQueue) -> Result<DrainOutcome> {
    let backend = infigraph
        .backend()
        .ok_or_else(|| anyhow::anyhow!("graph not initialized"))?;

    let mut to_extract: Vec<String> = Vec::new();
    let mut pre_parsed: Vec<FileExtraction> = Vec::new();
    let mut resolve_only: Vec<FileExtraction> = Vec::new();

    for (path, item) in drained.items {
        match item {
            PendingIndexItem::Raw => to_extract.push(path),
            PendingIndexItem::Structured(extraction) => pre_parsed.push(extraction),
            PendingIndexItem::ResolveOnly(extraction) => resolve_only.push(extraction),
        }
    }

    let mut whole_project_stale: Vec<String> = Vec::new();
    if drained.whole_project {
        let scan = infigraph.scan_changed_files(backend)?;
        for extraction in scan.extractions {
            // A whole-project scan finding a change for a path that ALSO
            // has an explicit pending item takes a back seat to that
            // explicit item (it's more specific/recent); only add what
            // isn't already covered.
            if !pre_parsed.iter().any(|e| e.file == extraction.file)
                && !to_extract.contains(&extraction.file)
            {
                to_extract.push(extraction.file.clone());
                let _ = extraction; // path already recorded above; re-extracted fresh below
            }
        }
        whole_project_stale = scan.stale_files;
    }

    let freshly_extracted: Vec<FileExtraction> = if to_extract.is_empty() {
        Vec::new()
    } else {
        infigraph.extract_paths(&to_extract)
    };

    let mut extractions = freshly_extracted;
    extractions.extend(pre_parsed);

    let mut removals: Vec<String> = drained.removals.into_iter().collect();
    removals.extend(whole_project_stale);
    for path in &removals {
        let _ = backend.remove_file(path);
    }

    if !extractions.is_empty() {
        let existing_hashes_empty = backend.get_file_hashes().unwrap_or_default().is_empty();
        backend.upsert_files_bulk(&extractions, existing_hashes_empty)?;
    }

    let use_learned = drained
        .waiters
        .iter()
        .any(|w| w.kind == WaiterKind::ResolveCalls && w.use_learned);
    let learned = if use_learned {
        Some(crate::learned::LearnedStore::load(infigraph.root()))
    } else {
        None
    };
    let mut resolve_extractions = extractions.clone();
    resolve_extractions.extend(resolve_only);
    let resolve_stats = backend
        .resolve_calls(&resolve_extractions, learned.as_ref())
        .unwrap_or_else(|e| {
            eprintln!("warning: call resolution failed: {e}");
            ResolveStats {
                total_calls: 0,
                resolved: 0,
                unresolved: 0,
                learned_resolved: 0,
                inherits_resolved: 0,
            }
        });

    for waiter in &drained.waiters {
        let result = match waiter.kind {
            WaiterKind::Index => WriteResult::Ok {
                total_files: extractions.len(),
                indexed_files: extractions.len(),
            },
            WaiterKind::UpsertFilesBulk => WriteResult::Ok {
                total_files: extractions.len(),
                indexed_files: extractions.len(),
            },
            WaiterKind::RemoveFiles => WriteResult::Ok {
                total_files: removals.len(),
                indexed_files: removals.len(),
            },
            WaiterKind::ResolveCalls => WriteResult::ResolveOk(resolve_stats.clone()),
        };
        write_atomic(&waiter.reply_path, &serde_json::to_string(&result)?)?;
    }

    Ok(DrainOutcome {
        extractions,
        resolve_stats,
    })
}
```

This references two `Infigraph` methods that need small additions in `lib.rs`:

```rust
    /// `pub(crate)` seam for the daemon drain -- exposes `scan_changed_files`
    /// beyond `index_via_backend`, which is the only other caller.
    pub(crate) fn scan_changed_files(&self, backend: &dyn graph::GraphBackend) -> Result<ScanResult> {
        // (already added in Step 1 above -- this note just confirms visibility;
        // if Step 1's version was written as a private `fn`, widen it to
        // `pub(crate) fn` so `watch/drain.rs` can call it.)
```

And a small new helper (extraction without hash-diffing, for paths the queue already decided are dirty and need reparsing regardless of hash):

```rust
    /// Extracts `rel_paths` fresh from disk, unconditionally (no hash
    /// comparison -- the caller has already decided these need re-parsing).
    /// Used by the daemon drain for `Raw` queue entries.
    pub(crate) fn extract_paths(&self, rel_paths: &[String]) -> Vec<FileExtraction> {
        rel_paths
            .par_iter()
            .filter_map(|rel_path| {
                let abs = self.root.join(rel_path);
                let source = std::fs::read(&abs).ok()?;
                let pack = self.registry.for_file_with_content(rel_path, &source)?;
                extract::extract_file(rel_path, &source, pack).ok()
            })
            .collect()
    }
```

Add this method to `Infigraph`'s `impl` block in `lib.rs`, near `index_via_backend`/`scan_changed_files`. Register the new module in `crates/infigraph-core/src/watch/mod.rs`:

```rust
pub mod batch;
pub mod daemon;
pub(crate) mod drain;
pub(crate) mod queue;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test watch_drain`
Expected: `test result: ok. 2 passed; 0 failed`

- [ ] **Step 7: Run the full lib test suite again to confirm no regression from the new helpers**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib`
Expected: same pass count as Step 2, still green.

- [ ] **Step 8: Clippy and fmt**

Run: `cargo clippy -p infigraph-core --all-targets -- -D warnings && cargo fmt -- --check`
Expected: both clean.

- [ ] **Step 9: Commit**

```bash
git add crates/infigraph-core/src/lib.rs crates/infigraph-core/src/watch/mod.rs crates/infigraph-core/src/watch/drain.rs crates/infigraph-core/tests/watch_drain.rs
git commit -m "feat: unified drain execution for coalesced index-shaped work"
```

---

### Task 3: Wire all producers into the shared queue (the correctness fix)

**Files:**
- Modify: `crates/infigraph-core/src/watch/mod.rs:90-457` (`watch_project_with_periodic`)
- Test: `crates/infigraph-core/tests/daemon_kuzu_e2e.rs` (extend)
- Test: `crates/infigraph-core/tests/watch_daemon.rs` (extend, for the removal-lock regression)

**Interfaces:**
- Consumes: `IndexWorkQueue`/`Waiter`/`WaiterKind` (Task 1), `execute_drain`/`DrainOutcome` (Task 2), `infigraph_core::daemon_protocol::{WriteRequest, serve_one_request}` (existing, unchanged).
- Produces: the actual bug fix — `watch_project_with_periodic` no longer executes periodic/batch/request-serving/removal work inline; every in-scope trigger source shares one `IndexWorkQueue` per watch session, drained synchronously (this task does not yet add the Task 4 background-task decoupling).

This is the task that makes the live repro (`INFIGRAPH_INDEX_VIA_DAEMON=1 infigraph index` immediately after creating a file) stop failing. Read `crates/infigraph-core/src/watch/mod.rs:90-457` in full before starting — this task rewrites large parts of it, and the exact current line numbers may have drifted since this plan was written; match against the structure, not blindly against line numbers.

- [ ] **Step 1: Write the failing E2E regression test first**

Add to `crates/infigraph-core/tests/daemon_kuzu_e2e.rs` (matching the file's existing helpers — `start_real_daemon`, `cli_binary`, `ENV_LOCK` — read the existing tests in this file for the exact helper signatures before writing this one, since they're already established elsewhere in the file):

```rust
/// The exact scenario reproduced live while confirming the predecessor fix
/// (fix/daemonkuzu-index-routing): under INFIGRAPH_INDEX_VIA_DAEMON=1,
/// creating a file and immediately running `infigraph index` -- no
/// settling delay for the daemon's own watcher debounce -- used to produce
/// a Kuzu duplicate-primary-key error, because the daemon's own
/// autonomous watcher and the client's explicit Index request each
/// independently decided the new file needed indexing. Proves the fix:
/// this must complete successfully, not error.
#[test]
fn ad_hoc_index_request_racing_the_watchers_own_debounce_does_not_duplicate_key() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("main.py"), "def main():\n    pass\n").unwrap();

    let mut daemon = start_real_daemon(project.path());

    // Bootstrap: one file already indexed before the race file appears,
    // matching the live repro's setup.
    let bootstrap = std::process::Command::new(cli_binary())
        .arg("--root")
        .arg(project.path())
        .arg("index")
        .arg("--no-embed")
        .env_remove("INFIGRAPH_BACKEND")
        .status()
        .unwrap();
    assert!(bootstrap.success());

    // The race: create a new file, then IMMEDIATELY (no settling delay)
    // submit an ad-hoc Index request via the opt-in whole-job-to-daemon
    // mode -- the exact combination that reproduced the bug live.
    std::fs::write(project.path().join("second.py"), "def second():\n    pass\n").unwrap();

    let output = std::process::Command::new(cli_binary())
        .arg("--root")
        .arg(project.path())
        .arg("index")
        .arg("--no-embed")
        .env("INFIGRAPH_BACKEND", "daemon")
        .env("INFIGRAPH_INDEX_VIA_DAEMON", "1")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected the racing Index request to succeed, not duplicate-key error:\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !stderr.contains("duplicated primary key"),
        "the coalescing bug reproduced:\n{stderr}"
    );

    daemon.kill().ok();
}
```

- [ ] **Step 2: Run the test to verify it fails (or hangs — kill and note it if so) against the current, unfixed loop**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo build -p infigraph-cli --bin infigraph && cargo test -p infigraph-core --test daemon_kuzu_e2e ad_hoc_index_request_racing -- --test-threads=1 --nocapture 2>&1 | tail -30`
Expected: FAIL — either the duplicate-primary-key error directly, or a flaky pass (the race is timing-dependent; if it passes on this run, that's not proof the fix works, just that this particular run didn't hit the window — do not treat a flaky pass here as satisfying this step, the point is confirming the assertion text/mechanism is right, not that it reliably repros every time before the fix).

- [ ] **Step 3: Rewrite `watch_project_with_periodic`'s state and the three inline execution blocks**

Add to `watch_project_with_periodic`'s local state (near the existing `let mut batch = ChangeBatch::new(1000);` and `let mut held_prism: Option<Infigraph> = None;` declarations):

```rust
    let mut queue = crate::watch::queue::IndexWorkQueue::new();
```

Replace the periodic-reindex block (the `if periodic_secs > 0 && changes_since_periodic > 0 && ... { if let Some(ref cb) = on_periodic { ... } else { ... } }` block) — the `Some(ref cb)` arm changes from directly calling `prism.index()` to marking the queue:

```rust
        if periodic_secs > 0
            && changes_since_periodic > 0
            && last_periodic.elapsed() >= Duration::from_secs(periodic_secs)
        {
            if on_periodic.is_some() {
                queue.mark_whole_project();
                changes_since_periodic = 0;
                last_periodic = std::time::Instant::now();
            } else {
                changes_since_periodic = 0;
                last_periodic = std::time::Instant::now();
            }
        }
```

(`on_periodic`'s callback itself — the `IndexResult` reporting hook — is invoked later, from the shared drain step below, using the drain's real `DrainOutcome`; it's no longer called from inside this block. The old block's own `begin_index_op`/`watch_db`/error-handling for periodic specifically is now subsumed by the shared drain, added below.)

Replace the request-serving block's per-request dispatch — the loop body inside `if serve_requests { ... for entry in entries.flatten() { ... } }` — from calling `serve_one_request` unconditionally to sniffing the request type first:

```rust
        if serve_requests {
            let requests_dir = root.join(".infigraph").join("requests");
            if let Ok(entries) = std::fs::read_dir(&requests_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "request") {
                        route_or_serve_request(root, &path, &mut queue, &make_registry, &mut held_prism);
                    }
                }
            }
        }
```

Add the new `route_or_serve_request` free function (near `watch_db`/`poison_watch_db` in the same file):

```rust
/// Parses a `.request` file and either enqueues it (for the four
/// index-shaped `WriteRequest` variants this design coordinates) or falls
/// through to the existing, unmodified `serve_one_request` for everything
/// else. Enqueued requests' `.request` file is deleted immediately (the
/// daemon has already accepted responsibility for serving it the moment
/// it's queued) -- the reply arrives later, written by `execute_drain`.
fn route_or_serve_request<MR>(
    root: &Path,
    path: &std::path::Path,
    queue: &mut crate::watch::queue::IndexWorkQueue,
    make_registry: &MR,
    held: &mut Option<Infigraph>,
) where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    let reply_path = path.with_extension("result");
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return, // transient; will be retried next tick if the file reappears
    };
    let request: crate::daemon_protocol::WriteRequest = match serde_json::from_str(&contents) {
        Ok(r) => r,
        Err(_) => {
            // Malformed request JSON -- not this design's concern to
            // recover; hand off to serve_one_request, whose existing
            // corrupt-JSON handling (WriteResult::Err) already covers it.
            if let Ok(prism) = watch_db(root, make_registry, held) {
                let _ = crate::daemon_protocol::serve_one_request(prism, path);
            }
            return;
        }
    };

    use crate::daemon_protocol::WriteRequest;
    use crate::watch::queue::{Waiter, WaiterKind};

    match request {
        WriteRequest::Index { paths: None } => {
            queue.mark_whole_project();
            queue.add_waiter(Waiter {
                kind: WaiterKind::Index,
                use_learned: false,
                reply_path,
            });
            std::fs::remove_file(path).ok();
        }
        WriteRequest::Index { paths: Some(paths) } => {
            for p in paths {
                let rel = p.to_string_lossy().replace('\\', "/");
                queue.add_raw(rel);
            }
            queue.add_waiter(Waiter {
                kind: WaiterKind::Index,
                use_learned: false,
                reply_path,
            });
            std::fs::remove_file(path).ok();
        }
        WriteRequest::UpsertFilesBulk { extractions_path, .. } => {
            match crate::daemon_protocol::read_extractions_json(&extractions_path) {
                Ok(extractions) => {
                    for extraction in extractions {
                        queue.add_structured(extraction);
                    }
                    queue.add_waiter(Waiter {
                        kind: WaiterKind::UpsertFilesBulk,
                        use_learned: false,
                        reply_path,
                    });
                    std::fs::remove_file(&extractions_path).ok();
                    std::fs::remove_file(path).ok();
                }
                Err(_) => {
                    // Sibling extractions file missing/corrupt -- fall
                    // through to serve_one_request's existing error path.
                    if let Ok(prism) = watch_db(root, make_registry, held) {
                        let _ = crate::daemon_protocol::serve_one_request(prism, path);
                    }
                }
            }
        }
        WriteRequest::RemoveFiles { files } => {
            for f in files {
                queue.add_removal(f);
            }
            queue.add_waiter(Waiter {
                kind: WaiterKind::RemoveFiles,
                use_learned: false,
                reply_path,
            });
            std::fs::remove_file(path).ok();
        }
        WriteRequest::ResolveCalls { extractions_path, use_learned } => {
            match crate::daemon_protocol::read_extractions_json(&extractions_path) {
                Ok(extractions) => {
                    for extraction in extractions {
                        queue.add_resolve_only(extraction);
                    }
                    queue.add_waiter(Waiter {
                        kind: WaiterKind::ResolveCalls,
                        use_learned,
                        reply_path,
                    });
                    std::fs::remove_file(&extractions_path).ok();
                    std::fs::remove_file(path).ok();
                }
                Err(_) => {
                    if let Ok(prism) = watch_db(root, make_registry, held) {
                        let _ = crate::daemon_protocol::serve_one_request(prism, path);
                    }
                }
            }
        }
        _ => {
            // The other 9 variants: unchanged behavior, immediate execution.
            if let Ok(prism) = watch_db(root, make_registry, held) {
                let _ = crate::daemon_protocol::serve_one_request(prism, path);
            }
        }
    }
}
```

Replace the batch-flush block (`if !batch.is_empty() && batch.is_ready() { ... }`) — it now feeds the queue instead of calling `index_files` directly, and no longer does its own `begin_index_op`/lock/execution (that's now the shared drain's job):

```rust
        if !batch.is_empty() && batch.is_ready() {
            let paths = batch.drain();
            for path in paths {
                let rel = path
                    .strip_prefix(root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"));
                queue.add_raw(rel);
            }
        }
```

Replace the `WatchEventKind::Removed` match arm (inside the `rx.recv_timeout` event-processing loop) — no longer touches `prism` directly, no longer bypasses locking:

```rust
                        WatchEventKind::Removed => {
                            let rel = path
                                .strip_prefix(root)
                                .map(|r| r.to_string_lossy().replace('\\', "/"))
                                .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"));
                            queue.add_removal(rel);
                            changes_since_periodic += 1;
                            on_event(WatchEvent {
                                kind: watch_kind.clone(),
                                path,
                                has_cross_file_calls: false,
                            });
                        }
```

Finally, add the shared drain step, once per loop iteration, after all four producers above have had a chance to run in this tick (place it right after the `WatchEventKind::Removed`/batch-flush handling, before the loop's `match rx.recv_timeout(...)` closes out the iteration — or, more simply, at the very end of the `loop { ... }` body, right before it implicitly continues to the next iteration):

```rust
        if !queue.is_empty() {
            if let Ok(prism) = watch_db(root, &make_registry, &mut held_prism) {
                match crate::watch::drain::execute_drain(prism, queue.drain()) {
                    Ok(outcome) => {
                        changes_since_periodic += outcome.extractions.len();
                        if let Some(ref cb) = on_periodic {
                            if !outcome.extractions.is_empty() {
                                cb(&crate::IndexResult {
                                    total_files: outcome.extractions.len(),
                                    indexed_files: outcome.extractions.len(),
                                    extractions: outcome.extractions.clone(),
                                    resolve_stats: outcome.resolve_stats.clone(),
                                });
                            }
                        }
                        if let Some(backend) = prism.backend() {
                            let changed: Vec<&str> =
                                outcome.extractions.iter().map(|e| e.file.as_str()).collect();
                            if !changed.is_empty() {
                                if let Err(e) = crate::embed::update_embeddings(backend, root, &changed)
                                {
                                    eprintln!("[watch] embedding update failed: {e}");
                                }
                            }
                        }
                        for extraction in &outcome.extractions {
                            let cross = has_cross_file_calls(prism, &extraction.file);
                            let abs_path = root.join(&extraction.file);
                            on_event(WatchEvent {
                                kind: WatchEventKind::Modified,
                                path: abs_path,
                                has_cross_file_calls: cross,
                            });
                        }
                    }
                    Err(e) => {
                        eprintln!("[watch] drain failed: {e}");
                        poison_watch_db(&mut held_prism);
                    }
                }
            } else {
                eprintln!("[watch] failed to reopen graph connection, will retry");
            }
        }
```

**Note:** this step deliberately does not yet acquire `begin_index_op`/`index.lock` around the drain — Task 2's `execute_drain` doesn't take it either. Add that now, wrapping the `execute_drain` call:

```rust
        if !queue.is_empty() {
            match crate::ops::begin_index_op(root, "infigraph daemon", Duration::from_secs(30)) {
                Ok(crate::ops::IndexOpOutcome::Acquired(_guard)) => {
                    if let Ok(prism) = watch_db(root, &make_registry, &mut held_prism) {
                        match crate::watch::drain::execute_drain(prism, queue.drain()) {
                            // ... (Ok/Err arms exactly as above)
```

(Full function: combine the lock-acquisition wrapper shown here with the complete match body from the block immediately above it — the lock guard's scope wraps the entire `if let Ok(prism) = watch_db(...) { match execute_drain(...) { ... } }` expression, dropping when that block ends, same pattern as today's existing batch-flush block's `guard`.) The `AlreadyRunning`/`Err` arms of the outer `begin_index_op` match, matching today's batch-flush block's existing handling:

```rust
                Ok(o @ crate::ops::IndexOpOutcome::AlreadyRunning(_)) => {
                    eprintln!(
                        "[watch] index operation busy ({}), retrying next tick",
                        o.skip_note().unwrap_or_default()
                    );
                }
                Err(e) => {
                    eprintln!("[watch] index operation busy ({e}), retrying next tick");
                }
            }
        }
```

(On busy/error, the queue's contents are *not* cleared — they were never drained in the first place, since `queue.drain()` is only called inside the `Acquired` arm. This matches the spec's Error Handling section: "the queue's contents are not cleared... they remain queued for the next tick's drain attempt.")

- [ ] **Step 4: Run the E2E regression test from Step 1 to verify it now passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo build -p infigraph-cli --bin infigraph && cargo test -p infigraph-core --test daemon_kuzu_e2e -- --test-threads=1 --nocapture 2>&1 | tail -40`
Expected: all tests in the file pass, including `ad_hoc_index_request_racing_the_watchers_own_debounce_does_not_duplicate_key`.

- [ ] **Step 5: Write and run the watch-triggered-removal-now-takes-the-lock regression test**

Add to `crates/infigraph-core/tests/watch_daemon.rs` (read the file's existing test setup helpers first — it already has patterns for spawning a real daemon and asserting on `.infigraph/index.lock` contention):

```rust
#[test]
fn watch_triggered_file_removal_contends_with_a_held_index_lock() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project = tempfile::tempdir().unwrap();
    let file_path = project.path().join("doomed.py");
    std::fs::write(&file_path, "def doomed():\n    pass\n").unwrap();

    // Hold index.lock externally, simulating another in-flight operation.
    let held = infigraph_core::ops::begin_index_op(
        project.path(),
        "test-holder",
        std::time::Duration::ZERO,
    )
    .unwrap();
    let _guard = match held {
        infigraph_core::ops::IndexOpOutcome::Acquired(g) => g,
        _ => panic!("expected to acquire the lock in this fresh test dir"),
    };

    // Removing the file while the lock is externally held must not
    // silently proceed unlocked (the pre-existing gap this design closes)
    // -- verified indirectly: the file's watch-triggered removal event
    // fires but the graph write is deferred (queue.add_removal, not an
    // immediate backend.remove_file call), so the lock's identity file
    // still shows "test-holder" as the sole holder throughout, and the
    // watcher's own drain attempt logs a busy/retry rather than silently
    // succeeding.
    //
    // (Precise assertion mechanism depends on what test hooks exist in
    // this file already for observing watcher log output or drain
    // attempts -- follow this file's established pattern for asserting on
    // watcher-loop behavior rather than introducing a new one here.)
}
```

**Implementer note:** this test's exact assertion mechanism is intentionally left to match whatever pattern `watch_daemon.rs` already uses elsewhere in the file for observing loop-internal behavior (stderr capture, a test-only hook, or timing-based polling) — read the file's other tests before finalizing this one's body, since the plan author did not have file-level access to confirm the exact idiom at planning time.

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test watch_daemon -- --test-threads=1`
Expected: all tests pass, including the new one.

- [ ] **Step 6: Run the full daemon-related suite**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test daemon_kuzu_backend --test daemon_protocol_serve --test backend_selection --test watch_daemon --test daemon_kuzu_e2e -- --test-threads=1`
Expected: all passing, no regressions in any of the 9 out-of-scope `WriteRequest` variants' existing coverage.

- [ ] **Step 7: Clippy and fmt**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: both clean.

- [ ] **Step 8: Commit**

```bash
git add crates/infigraph-core/src/watch/mod.rs crates/infigraph-core/tests/daemon_kuzu_e2e.rs crates/infigraph-core/tests/watch_daemon.rs
git commit -m "fix: coalesce daemon watch-loop index-shaped work through IndexWorkQueue"
```

---

### Task 4: Background-task draining via tokio

**Files:**
- Modify: `crates/infigraph-core/Cargo.toml` (add `tokio`)
- Modify: `crates/infigraph-core/src/watch/mod.rs` (drain scheduling)
- Test: `crates/infigraph-core/tests/daemon_kuzu_e2e.rs` (extend)

**Interfaces:**
- Consumes: `IndexWorkQueue`, `execute_drain` (unchanged from Tasks 1-2).
- Produces: the loop stays responsive (keeps accepting fsevents/periodic ticks/other-write-type requests) while a drain executes on a background `tokio::task::spawn_blocking` task, instead of blocking the whole loop for the drain's duration.

This task is additive on top of Task 3's already-correct, already-tested synchronous behavior — it changes *when* the drain runs relative to the rest of the loop, not *what* it computes.

- [ ] **Step 1: Add the `tokio` dependency**

Modify `crates/infigraph-core/Cargo.toml`, in the `[dependencies]` section (near the existing `rayon = "1"` line):

```toml
tokio = { version = "1", default-features = false, features = ["rt", "rt-multi-thread", "sync"] }
```

- [ ] **Step 2: Write the failing concurrency test**

Add to `crates/infigraph-core/tests/daemon_kuzu_e2e.rs`:

```rust
/// The actual behavioral claim Task 4 makes: the watch loop keeps
/// accepting new work while a drain is in flight, rather than blocking
/// the whole loop for the drain's duration. Proven by creating enough
/// files that a whole-project drain takes measurably long, then asserting
/// a SEPARATE, small ad-hoc request submitted while that drain is still
/// running gets served by the *next* drain rather than timing out because
/// the loop was blocked.
#[test]
fn producers_keep_accepting_work_while_a_drain_is_in_flight() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project = tempfile::tempdir().unwrap();

    // Enough files that scanning + extracting takes non-trivial wall time,
    // giving a real window for a second request to land mid-drain.
    for i in 0..200 {
        std::fs::write(
            project.path().join(format!("f{i}.py")),
            format!("def f{i}():\n    pass\n"),
        )
        .unwrap();
    }

    let mut daemon = start_real_daemon(project.path());

    // Kick off a whole-project index (a slow drain) without waiting for it.
    let mut slow = std::process::Command::new(cli_binary())
        .arg("--root")
        .arg(project.path())
        .arg("index")
        .arg("--no-embed")
        .env("INFIGRAPH_BACKEND", "daemon")
        .env("INFIGRAPH_INDEX_VIA_DAEMON", "1")
        .spawn()
        .unwrap();

    // Immediately, a second, unrelated file + index request -- if the loop
    // were still fully blocked by the slow drain (pre-Task-4 behavior),
    // this would have to wait out the ENTIRE first drain before even being
    // accepted into the queue, not just before being executed. This test
    // can't directly observe "was it accepted immediately" from outside
    // the process, so it instead asserts on the OUTCOME: both requests
    // complete successfully within a bounded time that would be
    // implausible if they were fully serialized loop-tick-by-loop-tick
    // rather than pipelined.
    std::fs::write(project.path().join("late.py"), "def late():\n    pass\n").unwrap();
    let second = std::process::Command::new(cli_binary())
        .arg("--root")
        .arg(project.path())
        .arg("index")
        .arg("--no-embed")
        .env("INFIGRAPH_BACKEND", "daemon")
        .env("INFIGRAPH_INDEX_VIA_DAEMON", "1")
        .output()
        .unwrap();

    let slow_status = slow.wait().unwrap();
    assert!(slow_status.success(), "the slow whole-project index must still succeed");
    assert!(
        second.status.success(),
        "the second request must succeed rather than timing out:\nstderr={}",
        String::from_utf8_lossy(&second.stderr)
    );

    daemon.kill().ok();
}

/// A panic inside the drain task must surface as an error to every waiter
/// folded into it, not hang until the client's own timeout.
#[test]
fn drain_task_panic_surfaces_as_write_result_err_not_a_hang() {
    // Implementer note: forcing a real panic inside execute_drain from an
    // external-process test is impractical without a test-only injection
    // hook. Prefer a lower-level test in watch_drain.rs (Task 2's test
    // file) that calls the drain-scheduling wrapper directly with a
    // panicking extraction function substituted via cfg(test) hook, OR
    // add a narrow, explicitly-test-only environment variable
    // (INFIGRAPH_TEST_PANIC_IN_DRAIN) that execute_drain checks at its
    // entry and panics if set -- follow whichever injection pattern this
    // codebase already uses elsewhere for testing panic-recovery paths
    // (check crates/infigraph-core/tests for an existing precedent before
    // inventing a new one).
}
```

- [ ] **Step 3: Run to verify the first test fails or times out with the current (Task 3, still-synchronous) implementation**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo build -p infigraph-cli --bin infigraph && cargo test -p infigraph-core --test daemon_kuzu_e2e producers_keep_accepting -- --test-threads=1 --nocapture 2>&1 | tail -30`
Expected: either an outright failure/timeout, or a pass that's suspiciously slow (both requests fully serialized) — note the wall-clock time here as a baseline to compare against after Step 5.

- [ ] **Step 4: Add a tokio runtime to the watch loop and upgrade the queue to `Arc<Mutex<_>>`**

Modify `watch_project_with_periodic`'s setup (before the `loop { ... }` starts):

```rust
    let queue = std::sync::Arc::new(std::sync::Mutex::new(crate::watch::queue::IndexWorkQueue::new()));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let mut drain_in_flight: Option<tokio::task::JoinHandle<()>> = None;
```

Every call site from Task 3 that did `queue.add_raw(...)`/`queue.add_structured(...)`/etc. directly on a plain `IndexWorkQueue` now locks first:

```rust
    queue.lock().unwrap().add_raw(rel);
```

(Apply this same `.lock().unwrap()` wrapping to every `queue.add_*`/`queue.mark_whole_project`/`queue.is_empty` call introduced in Task 3 — `route_or_serve_request` additionally needs `queue: &std::sync::Arc<std::sync::Mutex<IndexWorkQueue>>` as its parameter type instead of `&mut IndexWorkQueue`, cloning the `Arc` in, not borrowing.)

- [ ] **Step 5: Replace the synchronous drain call with background-task scheduling**

Replace Task 3's Step 3 final block (`if !queue.is_empty() { match begin_index_op(...) { ... } }`) with:

```rust
        let ready = drain_in_flight
            .as_ref()
            .map(|h| h.is_finished())
            .unwrap_or(true);
        if ready {
            if let Some(handle) = drain_in_flight.take() {
                if let Err(e) = rt.block_on(handle) {
                    eprintln!("[watch] drain task panicked: {e}");
                }
            }
            let should_spawn = !queue.lock().unwrap().is_empty();
            if should_spawn {
                let queue = std::sync::Arc::clone(&queue);
                let root = root.to_path_buf();
                let make_registry = make_registry_arc.clone();
                let on_periodic = on_periodic_arc.clone();
                let on_event = on_event_arc.clone();
                drain_in_flight = Some(rt.spawn_blocking(move || {
                    run_one_drain(&root, &queue, &make_registry, on_periodic.as_deref(), &*on_event);
                }));
            }
        }
```

This requires `make_registry`, `on_periodic`, and `on_event` to be shareable across the `spawn_blocking` closure (currently they're plain generic parameters/closures owned by the outer function, not `Send + Sync + 'static` wrapped). Wrap them once, near the top of `watch_project_with_periodic`, before the loop:

```rust
    let make_registry_arc: std::sync::Arc<dyn Fn() -> Result<crate::lang::LanguageRegistry> + Send + Sync> =
        std::sync::Arc::new(make_registry);
    let on_periodic_arc: Option<std::sync::Arc<dyn Fn(&crate::IndexResult) + Send + Sync>> =
        on_periodic.map(|cb| std::sync::Arc::new(cb) as std::sync::Arc<dyn Fn(&crate::IndexResult) + Send + Sync>);
    let on_event_arc: std::sync::Arc<dyn Fn(WatchEvent) + Send + Sync> = std::sync::Arc::new(on_event);
```

**Implementer note, flagged rather than resolved here:** `watch_project_with_periodic`'s existing generic signature (`MR: Fn() -> Result<...>`, `F: Fn(&IndexResult)`, `impl Fn(WatchEvent)`) does not currently require `Send + Sync + 'static` on any of these — the wrapping above requires that bound to exist for the `Arc<dyn Fn... + Send + Sync>` casts to type-check. This means the function's generic bounds need `Send + Sync + 'static` added, which is a signature change every caller must satisfy. Before finishing this step, grep all callers of `watch_project_with_periodic`/`watch_project`/`watch_project_auto_resolve` (which wraps it) and confirm none pass a closure capturing non-`Send`/`Sync` state — if one does, this step needs a different approach (e.g. keeping the drain's callback invocations on the *main* loop thread after `rt.block_on`-ing the join, rather than inside the `spawn_blocking` closure itself, trading some responsiveness for avoiding the bound change). Do not silently loosen this by wrapping in `unsafe impl Send`.

`run_one_drain` (new function, doing the lock-acquire-and-execute work that used to be inline in the loop, called from inside the `spawn_blocking` closure so `begin_index_op`'s blocking file-lock wait doesn't block the tokio runtime's async side):

```rust
fn run_one_drain(
    root: &Path,
    queue: &std::sync::Arc<std::sync::Mutex<crate::watch::queue::IndexWorkQueue>>,
    make_registry: &(dyn Fn() -> Result<crate::lang::LanguageRegistry> + Send + Sync),
    on_periodic: Option<&(dyn Fn(&crate::IndexResult) + Send + Sync)>,
    on_event: &(dyn Fn(WatchEvent) + Send + Sync),
) {
    match crate::ops::begin_index_op(root, "infigraph daemon", Duration::from_secs(30)) {
        Ok(crate::ops::IndexOpOutcome::Acquired(_guard)) => {
            let registry = match make_registry() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[watch] failed to build registry for drain: {e}");
                    return;
                }
            };
            let prism = match crate::Infigraph::open(root, registry).and_then(|mut p| {
                p.init()?;
                Ok(p)
            }) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[watch] failed to reopen graph connection for drain: {e}");
                    return;
                }
            };
            let drained = queue.lock().unwrap().drain();
            match crate::watch::drain::execute_drain(&prism, drained) {
                Ok(outcome) => {
                    if let Some(cb) = on_periodic {
                        if !outcome.extractions.is_empty() {
                            cb(&crate::IndexResult {
                                total_files: outcome.extractions.len(),
                                indexed_files: outcome.extractions.len(),
                                extractions: outcome.extractions.clone(),
                                resolve_stats: outcome.resolve_stats.clone(),
                            });
                        }
                    }
                    if let Some(backend) = prism.backend() {
                        let changed: Vec<&str> =
                            outcome.extractions.iter().map(|e| e.file.as_str()).collect();
                        if !changed.is_empty() {
                            if let Err(e) = crate::embed::update_embeddings(backend, root, &changed) {
                                eprintln!("[watch] embedding update failed: {e}");
                            }
                        }
                    }
                    for extraction in &outcome.extractions {
                        let cross = has_cross_file_calls(&prism, &extraction.file);
                        let abs_path = root.join(&extraction.file);
                        on_event(WatchEvent {
                            kind: WatchEventKind::Modified,
                            path: abs_path,
                            has_cross_file_calls: cross,
                        });
                    }
                }
                Err(e) => {
                    eprintln!("[watch] drain failed: {e}");
                }
            }
        }
        Ok(o @ crate::ops::IndexOpOutcome::AlreadyRunning(_)) => {
            eprintln!(
                "[watch] index operation busy ({}), retrying next tick",
                o.skip_note().unwrap_or_default()
            );
        }
        Err(e) => {
            eprintln!("[watch] index operation busy ({e}), retrying next tick");
        }
    }
}
```

Note this opens its **own** `Infigraph`/connection per drain (via `Infigraph::open`), rather than sharing the loop's `held_prism` — this is the "own connection from the daemon's already-open Database object" the design spec calls for, safe per lbug's verified multithreaded-connection guarantee. `held_prism` (used elsewhere for other write types still running inline on the main thread) is untouched by this task.

For the drain-task-panic handling from the spec's Error Handling section: `execute_drain`'s `Err` path already covers ordinary failures. A genuine Rust panic inside `run_one_drain` is caught by `rt.spawn_blocking`'s `JoinHandle` returning `Err` on `.await`/`block_on` (tokio catches panics in blocking tasks and reports them as a `JoinError`) — Step 5's `if let Err(e) = rt.block_on(handle) { eprintln!(...) }` above already logs this, but per the spec, waiters folded into a panicked drain must get `WriteResult::Err`, not silence. Since `queue.lock().unwrap().drain()` already happened *inside* the panicking task (removing those waiters from the queue before the panic), the outer loop has no way to still see them — move the `drain()` call to happen **outside** the spawned closure instead, passing the already-drained `DrainedQueue` in, so the outer loop retains the waiter list and can reply to them itself if the task panics:

```rust
            let should_spawn = !queue.lock().unwrap().is_empty();
            if should_spawn {
                let drained = queue.lock().unwrap().drain();
                let waiters_for_panic_recovery: Vec<_> = drained
                    .waiters
                    .iter()
                    .map(|w| (w.kind, w.reply_path.clone()))
                    .collect();
                let root = root.to_path_buf();
                drain_in_flight = Some(rt.spawn_blocking(move || {
                    run_one_drain_prepared(&root, drained, /* ... */);
                }));
                // stash `waiters_for_panic_recovery` alongside `drain_in_flight` so the
                // join-check block (Step 5's `if let Err(e) = rt.block_on(handle)`) can
                // write WriteResult::Err to each of these paths on panic.
            }
```

**Implementer note:** this restructuring (draining outside the spawned task, threading a "waiters to notify on panic" list alongside the `JoinHandle`) is described here at the level of intent rather than fully compiled code, since it changes `run_one_drain`'s signature to take an already-`DrainedQueue` (`run_one_drain_prepared`) instead of locking-and-draining the queue itself. Finish this wiring carefully — the goal is: **the panic-recovery path must reply to every waiter that was in the drain that panicked**, using the same `WriteResult::Err`-writing logic `execute_drain` already uses for ordinary failures. Write the actual test from Step 2's second test body (`drain_task_panic_surfaces_as_write_result_err_not_a_hang`) against this before considering the step done — a description of intent is not a substitute for the test proving it.

- [ ] **Step 6: Run both new tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test daemon_kuzu_e2e -- --test-threads=1 --nocapture 2>&1 | tail -40`
Expected: all tests pass, including both new ones from Step 2. Compare `producers_keep_accepting_work_while_a_drain_is_in_flight`'s wall-clock time against Step 3's baseline — it should be meaningfully faster (the two requests overlapping) rather than fully additive (fully serialized).

- [ ] **Step 7: Run the full daemon-related suite once more, end to end**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test daemon_kuzu_backend --test daemon_protocol_serve --test backend_selection --test watch_daemon --test daemon_kuzu_e2e --test watch_drain --lib -- --test-threads=1`
Expected: everything green — this is the full regression sweep across every file this plan touched.

- [ ] **Step 8: Clippy and fmt**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt -- --check`
Expected: both clean.

- [ ] **Step 9: Commit**

```bash
git add crates/infigraph-core/Cargo.toml crates/infigraph-core/src/watch/mod.rs crates/infigraph-core/tests/daemon_kuzu_e2e.rs
git commit -m "feat: drain the daemon index work-queue on a background tokio task"
```

---

## Self-Review

**Spec coverage:**
- IndexWorkQueue type + eviction rules → Task 1. ✅
- Debounce ownership (producers keep their own timers, queue has none) → Task 3 (each producer block's existing timer logic is untouched; only what happens *after* the timer fires changes). ✅
- Unified drain execution (extract, upsert, remove, resolve, reply-per-waiter-type) → Task 2. ✅
- All 4 in-scope `WriteRequest` variants + periodic + batch + removal wired → Task 3. ✅
- `index.lock` still acquired, narrower in-process role explicit → Task 3, Step 3's final block. ✅
- Background-task draining via tokio, `Arc<Mutex<>>`, `spawn_blocking` → Task 4. ✅
- Drain task panic handling → Task 4, Step 5 (flagged as needing careful finishing, not fully mechanical — the one place in this plan where "no placeholders" is knowingly stretched, because the exact JoinHandle/panic-recovery wiring is genuinely a design decision an implementer should make with the actual compiler in front of them, not blind text substitution).
- `ResolveOnly` refinement (avoiding redundant re-upsert for `ResolveCalls` requests) → Task 1/2, a deliberate, documented deviation from the spec's simpler `Structured`-only sketch.

**Placeholder scan:** Task 4 Step 5's `run_one_drain_prepared`/panic-recovery wiring is the one spot flagged above as intent-level rather than fully mechanical — everything else in this plan is complete, runnable code. This is a deliberate exception, not an oversight: the exact shape of "thread a waiters-to-notify list alongside a JoinHandle" has several reasonable implementations and the plan author judged it more honest to flag the decision than to guess one and present it as settled.

**Type consistency:** `PendingIndexItem`, `Waiter`, `WaiterKind`, `DrainedQueue` (Task 1) are consumed with identical names/shapes in Task 2's `execute_drain` and Task 3's `route_or_serve_request`. `ScanResult`/`scan_changed_files`/`extract_paths` (Task 2) are consumed identically in Task 2's own `execute_drain` and reused, unchanged, by Task 4's `run_one_drain`. `DrainOutcome` (Task 2) is consumed identically by Task 3's inline drain block and Task 4's `run_one_drain`.
