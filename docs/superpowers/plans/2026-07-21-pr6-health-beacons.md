# PR 6: Health Beacons on MCP Tool Responses — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tool responses append a one-line ⚠ footer per degraded condition — silent when healthy — generalizing the existing "✓ Auto-started watcher" footer pattern (spec: `docs/superpowers/specs/2026-07-20-write-safety-locks-design.md` §PR 6).

**Architecture:** Signals are collected in `infigraph-core` where they originate (a slow-lock-wait registry in `lockfile`, a trigram-fallback flag + HNSW-gap check in `embed`), plus process-local facts in `infigraph-mcp` (initialize-seen flag, watcher probe). A new `infigraph-mcp/src/health.rs` splits the work into `gather_signals` (thin collectors) and `compose_footer` (pure function, unit-testable), wired into `handle_tools_call` *after* compression so footers are never mangled or deduped away. Conditions ship only where their signal exists today: SCIP-staleness is deferred to R3.3.4 generation tracking per the spec ("conditions ship incrementally as their signals exist").

**Tech Stack:** Rust (edition 2021), `fs2` flocks, existing `lockfile`/`ops` modules from PRs 1/3.

## Global Constraints

- Branch: `feat/health-beacons` off `feat/index-operation-lock` (tip `2c10131`) — stacked on PR 3, which is merge-ready. Spec's dependency floor is PR 1 (lock identity), but the slow-wait instrumentation in `lockfile::acquire` also serves `index.lock` waits introduced by PR 3, so the stack order is the natural base. Commits go only to this branch.
- Every cargo command runs with `CARGO_PROFILE_DEV_DEBUG=0` (hard rule for this repo — mixing debug settings spawns multi-GB duplicate lbug cmake trees / ENOSPC).
- Commit with `--no-verify` after running `cargo fmt` manually (repo pre-commit hook runs `cargo fmt --check`).
- No CLI surface changes: beacons are MCP-only per spec. No new state stores: conditions derive from lock files, sidecar files, and process-local flags.
- The five spec conditions map as: worker-restarted → Task 4; watcher-inactive → Tasks 3+4; trigram/HNSW → Tasks 2+4; slow lock wait → Tasks 1+4; SCIP-stale → **deferred** (needs R3.3.4 generation counters, which don't exist; note in PR description).
- Footer strings are exact as given in Task 4 — tests assert on them.

---

### Task 1: `lockfile` slow-wait registry

**Files:**
- Modify: `crates/infigraph-core/src/lockfile.rs`
- Test: `crates/infigraph-core/tests/lockfile.rs` (extend)

**Interfaces:**
- Consumes: existing `lockfile::{try_acquire, acquire}` (PR 1).
- Produces: `pub struct SlowWait { pub lock_path: PathBuf, pub waited: Duration }`, `pub fn take_slow_waits() -> Vec<SlowWait>`, `pub fn slow_wait_threshold() -> Duration`. Task 4's `gather_signals` drains `take_slow_waits()`.

- [ ] **Step 1: Write the failing test**

Append to `crates/infigraph-core/tests/lockfile.rs`:

```rust
#[test]
fn test_slow_wait_recorded_and_drained() {
    // Edition 2021: set_var is safe. 50ms threshold keeps the test fast;
    // other tests in this binary never successfully acquire after a
    // >50ms contended wait (contended tests end in Busy, which must NOT
    // record), so cross-test interference is limited to extra entries we
    // filter out by path.
    std::env::set_var("INFIGRAPH_SLOW_LOCK_MS", "50");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("slow.lock");

    let held = lockfile::try_acquire(&path, "holder").unwrap().unwrap();
    let path2 = path.clone();
    let waiter = std::thread::spawn(move || {
        lockfile::acquire(&path2, "waiter", std::time::Duration::from_secs(5)).unwrap()
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    drop(held);
    let _guard = waiter.join().unwrap();

    let waits = lockfile::take_slow_waits();
    assert!(
        waits
            .iter()
            .any(|w| w.lock_path == path && w.waited >= std::time::Duration::from_millis(50)),
        "expected a recorded slow wait for {}, got {waits:?}",
        path.display()
    );
    // Drained: our path must not appear again.
    assert!(
        lockfile::take_slow_waits().iter().all(|w| w.lock_path != path),
        "take_slow_waits must drain recorded events"
    );
    std::env::remove_var("INFIGRAPH_SLOW_LOCK_MS");
}

#[test]
fn test_fast_acquire_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fast.lock");
    let g = lockfile::acquire(&path, "solo", std::time::Duration::from_secs(1)).unwrap();
    drop(g);
    assert!(
        lockfile::take_slow_waits().iter().all(|w| w.lock_path != path),
        "uncontended acquire must not record a slow wait"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test lockfile test_slow_wait -- --nocapture`
Expected: COMPILE ERROR — `take_slow_waits` not found in `lockfile`.

- [ ] **Step 3: Implement the registry**

In `crates/infigraph-core/src/lockfile.rs`, add after the `Busy` impl block (`std::sync::Mutex` needs importing; `PathBuf`, `Duration` already imported):

```rust
/// A lock acquisition that succeeded only after waiting longer than the
/// slow-wait threshold. The caller saw success, so without this record the
/// contention would be invisible; the MCP health footer drains these to
/// report "lock contention while serving this call".
#[derive(Debug, Clone)]
pub struct SlowWait {
    pub lock_path: PathBuf,
    pub waited: Duration,
}

static SLOW_WAITS: std::sync::Mutex<Vec<SlowWait>> = std::sync::Mutex::new(Vec::new());

/// Cap on buffered events between drains — processes that never drain
/// (the CLI) must not grow this without bound.
const SLOW_WAITS_CAP: usize = 16;

/// Threshold above which a successful-but-slow acquisition is recorded.
/// Milliseconds, overridable via `INFIGRAPH_SLOW_LOCK_MS` (tests).
pub fn slow_wait_threshold() -> Duration {
    std::env::var("INFIGRAPH_SLOW_LOCK_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(2))
}

fn record_slow_wait(path: &Path, waited: Duration) {
    if waited < slow_wait_threshold() {
        return;
    }
    if let Ok(mut buf) = SLOW_WAITS.lock() {
        if buf.len() < SLOW_WAITS_CAP {
            buf.push(SlowWait {
                lock_path: path.to_path_buf(),
                waited,
            });
        }
    }
}

/// Drain all slow-wait events recorded since the previous drain.
pub fn take_slow_waits() -> Vec<SlowWait> {
    SLOW_WAITS
        .lock()
        .map(|mut b| std::mem::take(&mut *b))
        .unwrap_or_default()
}
```

In `acquire`, record elapsed on the success path — change the loop head:

```rust
    loop {
        if let Some(guard) = try_acquire(path, role)? {
            record_slow_wait(path, start.elapsed());
            return Ok(guard);
        }
```

Note `try_acquire` (the zero-wait path) intentionally never records — a coalescing skip is not a wait, and the `Busy` failure path already surfaces itself as an error.

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test lockfile -- --test-threads=4`
Expected: PASS, including all pre-existing lockfile tests (no behavior change on any acquire path — recording is side-channel only).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/infigraph-core/src/lockfile.rs crates/infigraph-core/tests/lockfile.rs
git commit --no-verify -m "feat: lockfile records slow successful acquisitions for health beacons"
```

---

### Task 2: `embed` signals — trigram-fallback flag + HNSW-gap check

**Files:**
- Modify: `crates/infigraph-core/src/embed/mod.rs`
- Modify: `crates/infigraph-core/src/search/mod.rs` (~L226, DRY the duplicated threshold const)
- Test: `crates/infigraph-core/tests/health_signals.rs` (create)

**Interfaces:**
- Consumes: existing `embedding_count(root)` (reads the 4-byte LE u32 count header of `embeddings.bin`), `init_embedder`/`best_embedder` fallback arms.
- Produces: `pub const HNSW_THRESHOLD: usize = 200_000` (module-level in `embed`), `pub fn trigram_fallback_active() -> bool`, `pub fn hnsw_expected_but_missing(root: &Path) -> bool`. Task 4 reads both functions.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/tests/health_signals.rs`:

```rust
use infigraph_core::embed;

/// Forge an embeddings.bin whose count header claims `count` vectors.
/// embedding_count reads only the leading LE u32, so a bare header is
/// enough (format per save_embeddings: [count:u32][entries...]).
fn write_embeddings_header(dir: &std::path::Path, count: u32) {
    let tg = dir.join(".infigraph");
    std::fs::create_dir_all(&tg).unwrap();
    std::fs::write(tg.join("embeddings.bin"), count.to_le_bytes()).unwrap();
}

#[test]
fn hnsw_gap_only_above_threshold() {
    let dir = tempfile::tempdir().unwrap();

    // No embeddings at all: not degraded.
    assert!(!embed::hnsw_expected_but_missing(dir.path()));

    // Below threshold: linear scan is the *designed* fast path, not a gap.
    write_embeddings_header(dir.path(), 1_000);
    assert!(!embed::hnsw_expected_but_missing(dir.path()));

    // At threshold with no index file: degraded.
    write_embeddings_header(dir.path(), embed::HNSW_THRESHOLD as u32);
    assert!(embed::hnsw_expected_but_missing(dir.path()));

    // Index file present: healthy again.
    std::fs::write(
        dir.path().join(".infigraph").join("hnsw_index.usearch"),
        b"stub",
    )
    .unwrap();
    assert!(!embed::hnsw_expected_but_missing(dir.path()));
}

#[test]
fn trigram_flag_defaults_false_and_latches() {
    // Default: no fallback observed (this test binary never inits an
    // embedder before this point).
    assert!(!embed::trigram_fallback_active());
    embed::note_trigram_fallback();
    assert!(embed::trigram_fallback_active());
}
```

Add `tempfile` to `[dev-dependencies]` in `crates/infigraph-core/Cargo.toml` only if not already present (it is used by existing test files — check first; do not duplicate the entry).

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test health_signals`
Expected: COMPILE ERROR — `hnsw_expected_but_missing`, `HNSW_THRESHOLD`, `trigram_fallback_active`, `note_trigram_fallback` not found.

- [ ] **Step 3: Implement**

In `crates/infigraph-core/src/embed/mod.rs`:

(a) Add imports for atomics to the existing `use` block: `use std::sync::atomic::{AtomicBool, Ordering};`

(b) Add module-level items (place near the `CODE_EMBEDDER` statics, ~L307):

```rust
/// Below this many embeddings, brute-force rayon dot-product beats HNSW
/// (index load + search overhead). Shared by the index build gate, search,
/// and the health footer's "HNSW missing" check.
pub const HNSW_THRESHOLD: usize = 200_000;

/// Process-wide latch: set when embedder construction fell back to trigram
/// hashing because Model2Vec was unavailable. Read by the MCP health footer.
static TRIGRAM_FALLBACK: AtomicBool = AtomicBool::new(false);

/// Record that an embedder was constructed on the trigram fallback path.
pub fn note_trigram_fallback() {
    TRIGRAM_FALLBACK.store(true, Ordering::Relaxed);
}

pub fn trigram_fallback_active() -> bool {
    TRIGRAM_FALLBACK.load(Ordering::Relaxed)
}

/// True when this project is above the HNSW threshold but the sidecar index
/// file is missing — vector search is silently on a linear scan that no
/// longer pays off at this scale. Below the threshold a missing index is by
/// design, not degradation.
pub fn hnsw_expected_but_missing(root: &Path) -> bool {
    let hnsw = root.join(".infigraph").join("hnsw_index.usearch");
    !hnsw.exists() && embedding_count(root) >= HNSW_THRESHOLD
}
```

(c) In BOTH fallback arms — `init_embedder` (~L314) and `best_embedder` (~L335) — insert `note_trigram_fallback();` as the first statement of the `Err(e) =>` arm, before the `eprintln!`.

(d) In `update_embeddings` (~L488), delete the fn-local `const HNSW_THRESHOLD: usize = 200_000;` line (the comment above it moves to the module const, which already carries it — delete the local comment lines too).

In `crates/infigraph-core/src/search/mod.rs` (~L224-227): delete the fn-local `const HNSW_THRESHOLD: usize = 200_000;` and its two comment lines; change the use site to `let use_hnsw = symbol_embeddings.len() >= embed::HNSW_THRESHOLD;` (the module already imports `crate::embed` — verify the existing import style at the top of the file and match it).

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test health_signals && CARGO_PROFILE_DEV_DEBUG=0 cargo check -p infigraph-core`
Expected: PASS both tests; clean check (the two deleted consts have no other references).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/infigraph-core/src/embed/mod.rs crates/infigraph-core/src/search/mod.rs crates/infigraph-core/tests/health_signals.rs
git commit --no-verify -m "feat: embed exposes trigram-fallback latch and HNSW-gap check; DRY HNSW_THRESHOLD"
```

---

### Task 3: shared `watcher_running` probe; search's stale-warning moves out

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/watch.rs`
- Modify: `crates/infigraph-mcp/src/tools/search.rs:506-529`
- Test: existing `crates/infigraph-mcp/tests/watcher_concurrency.rs` (behavioral net must not change for the covered cases)

**Interfaces:**
- Consumes: `is_watching(&str)` (in-process map), the `.infigraph/watch.lock` flock probe currently inlined in `tool_search`.
- Produces: `pub fn watcher_running(root: &Path) -> bool` in `tools::watch` — used here by `tool_search` and in Task 4 by `gather_signals`.

- [ ] **Step 1: Add the probe helper**

In `crates/infigraph-mcp/src/tools/watch.rs`, after `is_watching` (~L65):

```rust
/// True when a code watcher is running for `root` — either in this process
/// (WATCHERS map) or in another process (CLI `infigraph watch` or another
/// MCP worker holding `.infigraph/watch.lock`). Probe-only: opens the lock
/// file and immediately releases the trial flock; never creates the file.
pub fn watcher_running(root: &std::path::Path) -> bool {
    let root_str = root.to_string_lossy().replace('\\', "/");
    if is_watching(&root_str) {
        return true;
    }
    use fs2::FileExt;
    let lock_path = root.join(".infigraph").join("watch.lock");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&lock_path)
        .ok()
        .map(|f| {
            let locked = f.try_lock_exclusive().is_err();
            let _ = fs2::FileExt::unlock(&f);
            locked
        })
        .unwrap_or(false)
}
```

- [ ] **Step 2: Rewire `tool_search`**

Replace `crates/infigraph-mcp/src/tools/search.rs:506-529` (the whole `if !super::watch::is_watching(...)` block) with:

```rust
    if !super::watch::watcher_running(&root) {
        if let Some(msg) = super::watch::auto_start_watch_opportunistic(path) {
            out.push_str(&format!("\n✓ Auto-started watcher: {msg}"));
        }
        super::docs::auto_start_doc_watch_opportunistic(path);
    }
```

The removed `⚠ No file watcher running — results may be stale...` else-branch is NOT lost: Task 4's health footer emits the identical line for **every** tool (search included) whenever no watcher runs — search's auto-start usually heals the condition before the footer looks, so the warning now only appears when auto-start actually failed, for any tool.

- [ ] **Step 3: Run the watcher test suites**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test watcher_concurrency -- --test-threads=4`
Expected: PASS. The three probe-sensitive tests hold: `test_search_auto_starts_watcher_when_none_running` (auto-start message still emitted, no warning), `test_no_stale_warning_with_mcp_watcher` and `test_no_stale_warning_with_cli_watcher` (warning absent — trivially, since `tool_search` no longer emits it at all). If any test in `watcher_concurrency.rs` or `watcher_reindex.rs` asserts the *presence* of the old warning string in `tool_search` output, update that assertion to call the Task 4 footer path instead — but per current reading, none does. Known pre-existing failure to ignore (NOT caused by this change): `test_graph_tools_with_group_watchers`.

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add crates/infigraph-mcp/src/tools/watch.rs crates/infigraph-mcp/src/tools/search.rs
git commit --no-verify -m "refactor: extract cross-process watcher_running probe; search defers stale warning to health footer"
```

---

### Task 4: `health` module + wiring into initialize / tools-call

**Files:**
- Create: `crates/infigraph-mcp/src/health.rs`
- Modify: `crates/infigraph-mcp/src/lib.rs` (module decl ~L1-5; `handle_initialize` ~L607; `handle_tools_call` success arm ~L684-694)
- Test: `crates/infigraph-mcp/tests/health_beacons.rs` (create)

**Interfaces:**
- Consumes: `lockfile::take_slow_waits()` (Task 1), `embed::{trigram_fallback_active, hnsw_expected_but_missing}` (Task 2), `tools::watch::watcher_running` (Task 3), `tools::helpers::resolve_project_path(&str) -> String` (existing).
- Produces: `health::HEALTH.mark_initialized()`, `health::health_footer(tool_name, &args) -> Option<String>`, plus test-facing `HealthState::new()`, `Signals`, `gather_signals`, `compose_footer`.

- [ ] **Step 1: Write the failing tests**

Create `crates/infigraph-mcp/tests/health_beacons.rs`:

```rust
use infigraph_mcp::health::{compose_footer, gather_signals, HealthState, Signals};

#[test]
fn healthy_signals_produce_no_footer() {
    assert!(compose_footer(&Signals::default()).is_none());
}

#[test]
fn each_condition_renders_one_warning_line() {
    let sig = Signals {
        worker_restarted: true,
        watcher_missing: true,
        trigram_fallback: true,
        hnsw_missing: true,
        slow_waits: vec![("graph.lock".to_string(), 5)],
    };
    let footer = compose_footer(&sig).unwrap();
    let lines: Vec<&str> = footer.lines().collect();
    assert_eq!(lines.len(), 5, "one line per degraded condition: {footer}");
    assert!(lines.iter().all(|l| l.starts_with('⚠')), "{footer}");
    assert!(footer.contains("worker restarted since your previous call"));
    assert!(footer.contains("No file watcher running — results may be stale"));
    assert!(footer.contains("trigram fallback"));
    assert!(footer.contains("HNSW index missing"));
    assert!(footer.contains("waited 5s for graph.lock"));
}

#[test]
fn restart_beacon_fires_exactly_once_per_worker() {
    let state = HealthState::new();
    assert!(gather_signals(&state, "search", None).worker_restarted);
    assert!(
        !gather_signals(&state, "search", None).worker_restarted,
        "second call must not repeat the restart warning"
    );
}

#[test]
fn initialized_worker_never_fires_restart_beacon() {
    let state = HealthState::new();
    state.mark_initialized();
    assert!(!gather_signals(&state, "search", None).worker_restarted);
}

#[test]
fn watcher_beacon_from_durable_state() {
    let state = HealthState::new();
    state.mark_initialized();
    let dir = tempfile::tempdir().unwrap();
    let tg = dir.path().join(".infigraph");
    std::fs::create_dir_all(&tg).unwrap();

    // No watcher anywhere: beacon fires.
    assert!(gather_signals(&state, "search", Some(dir.path())).watcher_missing);

    // Another process (simulated: separate fd) holds watch.lock: healthy.
    use fs2::FileExt;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(tg.join("watch.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    assert!(!gather_signals(&state, "search", Some(dir.path())).watcher_missing);
}

#[test]
fn watcher_lifecycle_tools_are_exempt() {
    let state = HealthState::new();
    state.mark_initialized();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
    assert!(
        !gather_signals(&state, "stop_watch", Some(dir.path())).watcher_missing,
        "a 'no watcher' beacon right after a deliberate stop_watch is noise"
    );
}

#[test]
fn no_project_means_no_project_scoped_beacons() {
    let state = HealthState::new();
    state.mark_initialized();
    let sig = gather_signals(&state, "compress", None);
    assert!(!sig.watcher_missing);
    assert!(!sig.hnsw_missing);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test health_beacons`
Expected: COMPILE ERROR — `infigraph_mcp::health` module does not exist.

- [ ] **Step 3: Implement `health.rs`**

Create `crates/infigraph-mcp/src/health.rs`:

```rust
//! Health beacons: one-line ⚠ footers appended to MCP tool responses only
//! when a degraded condition exists — silent when healthy, so no
//! steady-state token cost (spec: write-safety-locks-design §PR 6).
//! Ground-truth rule: every condition derives from durable state (lock
//! files, sidecar files) or directly-observed process facts, never from
//! cached beliefs about what should be running.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use infigraph_core::{embed, lockfile};

/// Process-local health flags for this worker incarnation.
pub struct HealthState {
    /// Set when this process serves an `initialize` request. A tools/call
    /// arriving before any initialize means the client's MCP session
    /// predates this process: the previous worker died and the supervisor
    /// respawned us mid-session with inherited stdio (the I-13
    /// crash-was-invisible failure mode, de-cloaked).
    initialized: AtomicBool,
    /// The restart beacon fires once per incarnation — after the first
    /// warning the client knows, and repeating it is pure token cost.
    restart_emitted: AtomicBool,
}

pub static HEALTH: HealthState = HealthState::new();

impl HealthState {
    pub const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            restart_emitted: AtomicBool::new(false),
        }
    }

    pub fn mark_initialized(&self) {
        self.initialized.store(true, Ordering::Relaxed);
    }

    /// True exactly once: the first call served by a worker that never saw
    /// `initialize`. Short-circuit keeps the latch untouched on
    /// initialized workers.
    fn restart_beacon(&self) -> bool {
        !self.initialized.load(Ordering::Relaxed)
            && !self.restart_emitted.swap(true, Ordering::Relaxed)
    }
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything the footer depends on, gathered up front so composition is a
/// pure function.
#[derive(Debug, Default)]
pub struct Signals {
    pub worker_restarted: bool,
    pub watcher_missing: bool,
    pub trigram_fallback: bool,
    pub hnsw_missing: bool,
    /// (lock file name, whole seconds waited) per slow acquisition drained
    /// for this call.
    pub slow_waits: Vec<(String, u64)>,
}

/// Tools whose output *is* watcher-lifecycle state — a "no watcher" beacon
/// on them is noise (e.g. immediately after a deliberate stop_watch).
const WATCHER_LIFECYCLE_TOOLS: &[&str] = &[
    "watch_project",
    "stop_watch",
    "get_watch_status",
    "watch_docs",
    "stop_watch_docs",
];

fn is_remote_mode() -> bool {
    std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false)
}

pub fn gather_signals(state: &HealthState, tool_name: &str, project: Option<&Path>) -> Signals {
    let mut sig = Signals {
        worker_restarted: state.restart_beacon(),
        trigram_fallback: embed::trigram_fallback_active(),
        ..Default::default()
    };
    sig.slow_waits = lockfile::take_slow_waits()
        .into_iter()
        .map(|w| {
            let name = w
                .lock_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| w.lock_path.display().to_string());
            (name, w.waited.as_secs())
        })
        .collect();
    if let Some(root) = project {
        // Watcher and HNSW are local-filesystem concepts; in remote mode
        // (Neo4j backend) neither applies. A project without .infigraph
        // has nothing to be stale against.
        if !is_remote_mode() && root.join(".infigraph").is_dir() {
            if !WATCHER_LIFECYCLE_TOOLS.contains(&tool_name) {
                sig.watcher_missing = !crate::tools::watch::watcher_running(root);
            }
            sig.hnsw_missing = embed::hnsw_expected_but_missing(root);
        }
    }
    sig
}

/// Pure: render one ⚠ line per degraded condition, `None` when healthy.
pub fn compose_footer(sig: &Signals) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if sig.worker_restarted {
        lines.push(
            "⚠ worker restarted since your previous call — in-memory state \
             (watchers, session context) was reset"
                .to_string(),
        );
    }
    if sig.watcher_missing {
        lines.push(
            "⚠ No file watcher running — results may be stale. \
             Run `infigraph watch` or re-index to refresh."
                .to_string(),
        );
    }
    if sig.trigram_fallback {
        lines.push(
            "⚠ semantic search degraded: Model2Vec model unavailable, using trigram fallback"
                .to_string(),
        );
    }
    if sig.hnsw_missing {
        lines.push(
            "⚠ HNSW index missing — vector search is on a linear scan this \
             project has outgrown; re-index to rebuild"
                .to_string(),
        );
    }
    for (name, secs) in &sig.slow_waits {
        lines.push(format!(
            "⚠ lock contention: waited {secs}s for {name} while serving this call"
        ));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Footer for one tool call against the process-global state; `None` when
/// fully healthy.
pub fn health_footer(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    let project = args
        .get("path")
        .and_then(|p| p.as_str())
        .map(|p| std::path::PathBuf::from(crate::tools::helpers::resolve_project_path(p)));
    compose_footer(&gather_signals(&HEALTH, tool_name, project.as_deref()))
}
```

Note for the implementer: check `tools::helpers::resolve_project_path`'s exact signature before use — it is called in `search.rs` as `resolve_project_path(args.get("path").and_then(|p| p.as_str()).unwrap_or("."))` returning something that binds as `&String`-like; adapt the one call above to its real return type if it differs from `String`.

- [ ] **Step 4: Wire into `lib.rs`**

(a) Add `pub mod health;` to the module list at the top (~L1-5, alphabetical: after `compress`).

(b) In `handle_initialize` (~L608), first line of the function body:

```rust
    health::HEALTH.mark_initialized();
```

(c) In `handle_tools_call`'s `Ok(Ok(content))` arm (~L691), the footer goes on AFTER compression — compression must never mangle or dedup a live warning — and BEFORE token accounting, so the cost is still counted. Change:

```rust
            let compressed = compress::compress_pipeline_safe(&content, tool_name, &args);
            let comp_tokens = estimate_tokens(&compressed);
```

to:

```rust
            let mut compressed = compress::compress_pipeline_safe(&content, tool_name, &args);
            if let Some(footer) = health::health_footer(tool_name, &args) {
                compressed.push('\n');
                compressed.push_str(&footer);
            }
            let comp_tokens = estimate_tokens(&compressed);
```

Error and panic arms get no footer — those responses already announce their own failure.

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --test health_beacons -- --test-threads=4`
Expected: PASS (7 tests). Note the tests deliberately use fresh `HealthState` instances, not `HEALTH`, so parallel test threads cannot interfere through the global; `gather_signals` does drain the global slow-wait buffer, but no assertion here reads `slow_waits`, so cross-test drains are harmless.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/infigraph-mcp/src/health.rs crates/infigraph-mcp/src/lib.rs crates/infigraph-mcp/tests/health_beacons.rs
git commit --no-verify -m "feat: health beacons — degraded-only ⚠ footers on MCP tool responses"
```

---

### Task 5: Full verification + stacked fork PR

**Files:**
- None created; runs suites, pushes, opens PR.

- [ ] **Step 1: Full test suites**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core -- --test-threads=4
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp -- --test-threads=4
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-cli -- --test-threads=4
```

Expected: green, modulo the two known pre-existing mcp failures unrelated to this branch (`tool_parity` get_compression_stats; `watcher_concurrency::test_graph_tools_with_group_watchers`). Any NEW failure is this PR's to fix before proceeding.

- [ ] **Step 2: Clippy**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-core -p infigraph-mcp -- -D warnings`
Expected: clean. Fix anything introduced by this branch only.

- [ ] **Step 3: Push and open the fork PR (stacked)**

```bash
git push -u fork feat/health-beacons
gh pr create --repo <fork> --base feat/index-operation-lock --head feat/health-beacons \
  --title "feat: health beacons on MCP tool responses (PR 6)" \
  --body "<summary: five spec conditions; SCIP-staleness deferred pending R3.3.4 generation counters; stacked on PR 3>"
```

Fork-only per standing directive (`no-upstream-prs-without-asking`): do NOT open an upstream PR. Use the same fork remote name the previous PRs used (check `git remote -v`; PRs #1-#3 live on the fork).

- [ ] **Step 4: Final whole-branch review**

Dispatch the final reviewer over the full branch diff (`2c10131..HEAD`) with the spec section and this plan as context; resolve findings before declaring merge-ready.

---

## Self-Review Notes

- **Spec coverage:** worker-restarted (T4, initialize-latch), watcher-inactive (T3 probe + T4 beacon), trigram fallback + HNSW (T2 + T4), slow lock wait (T1 + T4). SCIP-stale explicitly deferred — the spec authorizes shipping conditions incrementally as signals exist, and R3.3.4 generation counters do not exist yet; the PR description must say so.
- **"No new state stores" honored:** signals are lock files, sidecar files, process statics, and one drained in-memory buffer.
- **Footer placement:** after `compress_pipeline_safe`, inside `handle_tools_call` — dispatch-level tests (tool_parity etc.) are untouched because `dispatch_tool` itself is unchanged.
- **Restart-beacon caveat (accepted):** an HTTP-mode client that never sends `initialize` would see the restart line once on its first call; one-shot latching bounds the noise, and stdio (the supervisor-respawn transport where I-13 lives) is the only transport where the signal is load-bearing.
