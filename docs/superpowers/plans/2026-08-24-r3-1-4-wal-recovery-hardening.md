# R3.1.4 — WAL Auto-Recovery, Crash-Loop Breaker, Disk/Quarantine Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the still-open half of R3.1.4 (#115's WAL auto-recovery/degrade/crash-loop-breaker, and #100's live-graph growth + second-incident UX gaps), reusing the already-shipped quarantine and daemon-routed full-reindex machinery rather than building new recovery primitives.

**Architecture:** A dead-holder-WAL read quarantines immediately and drops a sentinel; the daemon's existing write-coordinator poll tick (never the read path itself) translates that sentinel into the existing `WriteRequest::FullReindex` request, gated by a crash-loop breaker built on the same append-log pattern `dirty.rs`/`audit.rs` already establish. Four smaller, independent fixes (disk-growth breaker, doctor observe-only, plain-index auto-promotion, daemon-log discoverability) round out the remaining scope.

**Tech Stack:** Rust workspace (`infigraph-core`, `infigraph-cli`, `infigraph-mcp`), Kuzu embedded graph DB, existing `anyhow`-based error handling (no new error taxonomy — R4.1 is separately tracked and out of scope here).

**Spec:** `docs/superpowers/specs/2026-08-24-r3-1-4-wal-recovery-hardening-design.md` — read it alongside this plan; it explains the *why* behind each task's *what*.

## Global Constraints

- Every `cargo` invocation MUST be run with `CARGO_PROFILE_DEV_DEBUG=0` in the environment (repo-wide rule; mixing debug-info settings has caused ENOSPC incidents from duplicate debug symbols).
- Run tests with `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND` — this shell leaks both into the environment; unset them explicitly or tests silently exercise the wrong backend.
- The read path must never trigger a write directly (spec Architecture decision 1) — the crash-loop breaker and the sentinel-to-`FullReindex` translation both live in the daemon coordinator, never in `GraphStore::open_read_only_or_degrade`.
- Reuse existing machinery — the `WriteRequest::FullReindex` build-fresh-then-swap path, `quarantine_graph`, `snapshot::list_restore_points`, and the `dirty.rs`/`audit.rs` append-log convention — never reimplement any of these.
- `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings` must pass on the whole workspace before any task is considered done (pre-existing drift elsewhere can block an unrelated change — check baseline first if either fails unexpectedly).
- Disk on this machine is constrained: batch `cargo test` per-crate rather than one large `--all` run (see this session's own environment notes).

---

### Task 1: `recovery.rs` — sentinel, attempts-log, and crash-loop primitives

**Files:**
- Create: `crates/infigraph-core/src/recovery.rs`
- Modify: `crates/infigraph-core/src/lib.rs` (add `pub mod recovery;` alongside the existing `pub mod dirty;`/`pub mod quarantine;` declarations)
- Test: inline `#[cfg(test)] mod tests` in `recovery.rs`, following `dirty.rs`'s and `logrotate.rs`'s existing in-file test convention

**Interfaces:**
- Consumes: `crate::snapshot::{list_restore_points, RestorePointKind}` (existing), `crate::daemon_protocol::write_atomic` (existing)
- Produces (used by Tasks 2–4):
  - `pub const CRASH_LOOP_THRESHOLD: usize`
  - `pub const CRASH_LOOP_WINDOW: std::time::Duration`
  - `pub fn recovery_needed_path(infigraph_dir: &Path) -> PathBuf`
  - `pub fn mark_recovery_needed(infigraph_dir: &Path, dead_pid: u32, quarantined_to: &Path) -> anyhow::Result<()>`
  - `pub fn pending_recovery(infigraph_dir: &Path) -> bool`
  - `pub fn clear_recovery_needed(infigraph_dir: &Path) -> anyhow::Result<()>`
  - `pub fn recent_recovery_attempts(infigraph_dir: &Path) -> anyhow::Result<Vec<u64>>`
  - `pub fn record_recovery_attempt(infigraph_dir: &Path) -> anyhow::Result<()>`
  - `pub fn crash_loop_marker_path(infigraph_dir: &Path) -> PathBuf`
  - `pub fn write_crash_loop_marker(infigraph_dir: &Path, attempts: &[u64]) -> anyhow::Result<()>`
  - `pub fn crash_loop_detected(infigraph_dir: &Path) -> Option<Vec<u64>>`
  - `pub fn find_most_recent_previous(infigraph_dir: &Path, graph_name: &str) -> Option<PathBuf>`
  - `pub fn drain_recovery_sentinel(infigraph_dir: &Path) -> anyhow::Result<()>` (the coordinator's one-line integration point — Task 3 wires this in)

- [ ] **Step 1: Write the failing tests**

```rust
// bottom of crates/infigraph-core/src/recovery.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_round_trips_and_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert!(!pending_recovery(dir));
        mark_recovery_needed(dir, 4242, &dir.join("graph.corrupt.1")).unwrap();
        assert!(pending_recovery(dir));
        clear_recovery_needed(dir).unwrap();
        assert!(!pending_recovery(dir));
    }

    #[test]
    fn attempts_outside_the_window_are_not_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir).unwrap();
        // A timestamp far outside the 1h window, written directly (bypassing
        // record_recovery_attempt, which always stamps "now").
        std::fs::write(dir.join("recovery-attempts.log"), "100\n").unwrap();
        assert!(recent_recovery_attempts(dir).unwrap().is_empty());
    }

    #[test]
    fn drain_recovery_sentinel_submits_a_full_reindex_request_under_the_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        mark_recovery_needed(dir, 1, &dir.join("graph.corrupt.1")).unwrap();

        drain_recovery_sentinel(dir).unwrap();

        assert!(!pending_recovery(dir), "sentinel must be cleared");
        let requested: Vec<_> = std::fs::read_dir(dir.join("requests"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            requested.iter().any(|n| n.ends_with(".request")),
            "expected a .request file, got {requested:?}"
        );
        assert_eq!(recent_recovery_attempts(dir).unwrap().len(), 1);
        assert!(crash_loop_detected(dir).is_none());
    }

    #[test]
    fn drain_recovery_sentinel_trips_the_breaker_at_the_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for _ in 0..CRASH_LOOP_THRESHOLD {
            record_recovery_attempt(dir).unwrap();
        }
        mark_recovery_needed(dir, 1, &dir.join("graph.corrupt.99")).unwrap();

        drain_recovery_sentinel(dir).unwrap();

        assert!(!pending_recovery(dir), "sentinel must still be cleared");
        assert!(
            crash_loop_detected(dir).is_some(),
            "must trip after CRASH_LOOP_THRESHOLD prior attempts in the window"
        );
        let requested = std::fs::read_dir(dir.join("requests")).map(|d| d.count()).unwrap_or(0);
        assert_eq!(requested, 0, "must NOT submit another FullReindex once tripped");
    }

    #[test]
    fn find_most_recent_previous_prefers_the_newest_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("graph.previous.100"), b"old").unwrap();
        std::fs::write(dir.join("graph.previous.200"), b"new").unwrap();
        let found = find_most_recent_previous(dir, "graph").unwrap();
        assert_eq!(found.file_name().unwrap(), "graph.previous.200");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 cargo test --lib recovery:: -- --test-threads=1`
Expected: FAIL to compile (`recovery` module doesn't exist yet).

- [ ] **Step 3: Implement `recovery.rs`**

```rust
//! Automatic recovery from a dead-holder WAL (R3.1.4a, #115) and its
//! crash-loop breaker (R3.1.4c). Two pieces of state persist under
//! `.infigraph/`, mirroring the short-lived-file-then-append-log shape
//! `dirty.rs`/`audit.rs` already use elsewhere in this crate: a
//! "recovery needed" sentinel the daemon coordinator polls for
//! (`drain_recovery_sentinel`, called from `watch::run_write_coordinator`),
//! and a rolling attempts log the coordinator consults before acting on it.
//!
//! The read path (`graph::store::open_read_only_or_degrade`) only ever
//! writes the sentinel -- it never submits a write request itself. Per
//! docs/superpowers/specs/2026-08-24-r3-1-4-wal-recovery-hardening-design.md's
//! Architecture section, only the daemon coordinator thread may initiate a
//! rebuild, preserving R2.1.3's single-writer invariant.

use crate::snapshot::{list_restore_points, RestorePointKind};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RECOVERY_NEEDED_FILE: &str = "recovery-needed";
const RECOVERY_ATTEMPTS_LOG: &str = "recovery-attempts.log";
const CRASH_LOOP_MARKER_FILE: &str = "recovery-crash-loop";

/// Matches the observed incident exactly (two rebuild generations within
/// ~1 hour) and mirrors `quarantine::QUARANTINE_RETENTION`'s existing N=2
/// convention.
pub const CRASH_LOOP_THRESHOLD: usize = 2;
pub const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(3600);

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn recovery_needed_path(infigraph_dir: &Path) -> PathBuf {
    infigraph_dir.join(RECOVERY_NEEDED_FILE)
}

/// Called from the read path the moment it quarantines a dead-holder-WAL
/// graph. Records why, both for the coordinator and for a human reading
/// the sentinel file directly during an incident.
pub fn mark_recovery_needed(infigraph_dir: &Path, dead_pid: u32, quarantined_to: &Path) -> Result<()> {
    std::fs::create_dir_all(infigraph_dir)
        .with_context(|| format!("create {}", infigraph_dir.display()))?;
    let payload = serde_json::json!({
        "dead_pid": dead_pid,
        "quarantined_to": quarantined_to.display().to_string(),
        "detected_at": now_epoch_secs(),
    });
    crate::daemon_protocol::write_atomic(
        &recovery_needed_path(infigraph_dir),
        &serde_json::to_string_pretty(&payload)?,
    )
}

pub fn pending_recovery(infigraph_dir: &Path) -> bool {
    recovery_needed_path(infigraph_dir).exists()
}

pub fn clear_recovery_needed(infigraph_dir: &Path) -> Result<()> {
    let path = recovery_needed_path(infigraph_dir);
    if path.exists() {
        std::fs::remove_file(&path).context("remove recovery-needed sentinel")?;
    }
    Ok(())
}

/// Timestamps of auto-triggered rebuilds still inside the crash-loop window.
pub fn recent_recovery_attempts(infigraph_dir: &Path) -> Result<Vec<u64>> {
    let path = infigraph_dir.join(RECOVERY_ATTEMPTS_LOG);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path).context("read recovery-attempts.log")?;
    let cutoff = now_epoch_secs().saturating_sub(CRASH_LOOP_WINDOW.as_secs());
    Ok(content
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .filter(|&ts| ts >= cutoff)
        .collect())
}

pub fn record_recovery_attempt(infigraph_dir: &Path) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(infigraph_dir)
        .with_context(|| format!("create {}", infigraph_dir.display()))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(infigraph_dir.join(RECOVERY_ATTEMPTS_LOG))
        .context("open recovery-attempts.log for append")?;
    writeln!(f, "{}", now_epoch_secs()).context("append recovery-attempts.log")?;
    f.sync_all().ok();
    Ok(())
}

pub fn crash_loop_marker_path(infigraph_dir: &Path) -> PathBuf {
    infigraph_dir.join(CRASH_LOOP_MARKER_FILE)
}

pub fn write_crash_loop_marker(infigraph_dir: &Path, attempts: &[u64]) -> Result<()> {
    let payload = serde_json::json!({ "attempts": attempts, "detected_at": now_epoch_secs() });
    crate::daemon_protocol::write_atomic(
        &crash_loop_marker_path(infigraph_dir),
        &serde_json::to_string_pretty(&payload)?,
    )
}

/// `Some(prior attempt timestamps)` once the breaker has tripped. A human
/// clears this by deleting the marker file after fixing the underlying
/// cause -- matching the existing "delete the lock if you don't expect a
/// watcher here" convention `doctor`'s remediation hints already use.
pub fn crash_loop_detected(infigraph_dir: &Path) -> Option<Vec<u64>> {
    let content = std::fs::read_to_string(crash_loop_marker_path(infigraph_dir)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("attempts")?
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
}

/// Most recent cleanly-retired healthy graph for `graph_name`, if any --
/// the degrade fallback (R3.1.4b).
pub fn find_most_recent_previous(infigraph_dir: &Path, graph_name: &str) -> Option<PathBuf> {
    list_restore_points(infigraph_dir)
        .into_iter()
        .find(|p| {
            matches!(p.kind, RestorePointKind::Previous)
                && p.path
                    .file_name()
                    .map(|n| n.to_string_lossy().starts_with(graph_name))
                    .unwrap_or(false)
        })
        .map(|p| p.path)
}

/// The daemon coordinator's one-line integration point (called from
/// `watch::run_write_coordinator`'s `serve_requests` tick). No-op when no
/// sentinel is pending. Otherwise: under the crash-loop threshold, submits
/// a synthetic `WriteRequest::FullReindex` request file (the SAME request
/// type and code path `infigraph index --full` uses) for this same tick's
/// existing request-directory scan to pick up; at or over the threshold,
/// writes the crash-loop marker instead and submits nothing.
pub fn drain_recovery_sentinel(infigraph_dir: &Path) -> Result<()> {
    if !pending_recovery(infigraph_dir) {
        return Ok(());
    }

    let attempts = recent_recovery_attempts(infigraph_dir)?;
    if attempts.len() >= CRASH_LOOP_THRESHOLD {
        write_crash_loop_marker(infigraph_dir, &attempts)?;
        crate::audit::audit_log(
            "recovery",
            "crash-loop-detected",
            &format!("{} auto-rebuilds within the last hour", attempts.len()),
            &infigraph_dir.display().to_string(),
        );
        return clear_recovery_needed(infigraph_dir);
    }

    let requests_dir = infigraph_dir.join("requests");
    let request_path = requests_dir.join("auto-recovery.request");
    let serialized = serde_json::to_string(&crate::daemon_protocol::WriteRequest::FullReindex)
        .expect("WriteRequest::FullReindex always serializes");
    crate::daemon_protocol::write_atomic(&request_path, &serialized)?;
    record_recovery_attempt(infigraph_dir)?;
    crate::audit::audit_log(
        "recovery",
        "auto-triggered-full-reindex",
        "dead-holder WAL quarantined; auto-rebuild triggered",
        &infigraph_dir.display().to_string(),
    );
    clear_recovery_needed(infigraph_dir)
}
```

Add `pub mod recovery;` to `crates/infigraph-core/src/lib.rs` next to the existing `pub mod dirty;`/`pub mod quarantine;` lines.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 cargo test --lib recovery:: -- --test-threads=1`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/recovery.rs crates/infigraph-core/src/lib.rs
git commit -m "feat(recovery): sentinel, attempts-log, and crash-loop primitives (R3.1.4)"
```

---

### Task 2: `GraphStore::open_read_only_or_degrade` — quarantine-on-read and pre-crash-snapshot fallback

**Files:**
- Modify: `crates/infigraph-core/src/graph/store.rs`
- Test: `crates/infigraph-core/tests/write_lock_edge_cases.rs` (extend — this is where the existing `test_read_only_open_surfaces_wal_replay_failure` regression test for `GraphCorruption` already lives)

**Interfaces:**
- Consumes: `crate::recovery::{mark_recovery_needed, pending_recovery, find_most_recent_previous, crash_loop_detected}` (Task 1), `crate::quarantine::quarantine_graph` (existing), `GraphStore::open_read_only` (existing, unchanged)
- Produces (used by Task 4): `pub enum DegradeReason { PreCrashSnapshot { snapshot_path: PathBuf, dead_pid: u32 } }`, `pub fn GraphStore::open_read_only_or_degrade(path: &Path) -> anyhow::Result<(GraphStore, Option<DegradeReason>)>`

- [ ] **Step 1: Write the failing tests**

```rust
// append to crates/infigraph-core/tests/write_lock_edge_cases.rs

#[test]
fn open_read_only_or_degrade_falls_back_to_the_previous_pool() {
    let tmp = tempfile::tempdir().unwrap();
    let infigraph_dir = tmp.path().join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();
    let graph_path = infigraph_dir.join("graph");

    // Seed a real, openable "previous" graph a full reindex would have
    // retired -- open+init it via the normal write path, then rename it
    // aside exactly as `quarantine::retire_previous_graph` would.
    infigraph_core::graph::GraphStore::open(&graph_path).unwrap();
    let previous_path = infigraph_dir.join("graph.previous.111");
    std::fs::rename(&graph_path, &previous_path).unwrap();

    // Recreate the live path as a dead-holder-WAL scenario: an empty base
    // file, a WAL sibling, and a lock file naming a pid that isn't running.
    std::fs::write(&graph_path, vec![0u8; 4096]).unwrap();
    std::fs::write(infigraph_dir.join("graph.wal"), b"stub wal").unwrap();
    let lock_path = infigraph_dir.join("graph.lock");
    infigraph_core::lockfile::write_holder_for_test(&lock_path, 999_999_999);

    let (store, reason) =
        infigraph_core::graph::GraphStore::open_read_only_or_degrade(&graph_path).unwrap();
    drop(store);

    match reason {
        Some(infigraph_core::graph::DegradeReason::PreCrashSnapshot { snapshot_path, .. }) => {
            assert_eq!(snapshot_path, previous_path);
        }
        other => panic!("expected PreCrashSnapshot degrade, got {other:?}"),
    }
    assert!(
        infigraph_core::recovery::pending_recovery(&infigraph_dir),
        "sentinel must be left for the daemon coordinator to pick up"
    );
    assert!(!graph_path.exists(), "the dead-holder graph must have been quarantined");
}

#[test]
fn open_read_only_or_degrade_refuses_with_rebuild_in_progress_wording_when_no_fallback_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let infigraph_dir = tmp.path().join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();
    let graph_path = infigraph_dir.join("graph");

    std::fs::write(&graph_path, vec![0u8; 4096]).unwrap();
    std::fs::write(infigraph_dir.join("graph.wal"), b"stub wal").unwrap();
    let lock_path = infigraph_dir.join("graph.lock");
    infigraph_core::lockfile::write_holder_for_test(&lock_path, 999_999_999);

    let err = infigraph_core::graph::GraphStore::open_read_only_or_degrade(&graph_path)
        .expect_err("no .previous. pool entry exists -- must refuse");
    assert!(
        err.to_string().contains("auto-rebuild"),
        "must say a rebuild is already in progress, not tell the human to run --full: {err}"
    );
}

#[test]
fn open_read_only_or_degrade_refuses_distinctly_once_the_crash_loop_breaker_has_tripped() {
    let tmp = tempfile::tempdir().unwrap();
    let infigraph_dir = tmp.path().join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();
    infigraph_core::recovery::write_crash_loop_marker(&infigraph_dir, &[1, 2]).unwrap();

    let graph_path = infigraph_dir.join("graph"); // doesn't need to exist for this path
    let err = infigraph_core::graph::GraphStore::open_read_only_or_degrade(&graph_path)
        .expect_err("crash-loop marker must short-circuit to a refusal");
    assert!(
        err.to_string().contains("crash-loop"),
        "must be the distinct crash-loop wording, not the generic quarantine message: {err}"
    );
}
```

Note: `infigraph_core::lockfile::write_holder_for_test` is a test-only helper that must already exist (it's the standard way this codebase's existing tests, e.g. `open_refuses_a_graph_with_an_unreplayed_wal_from_a_dead_process`, seed a dead-holder lock — confirm its exact name/signature by reading that existing test in `store.rs` before writing this step for real; if it writes the lock file directly instead of via a helper, mirror that file's exact approach rather than inventing a new helper).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 cargo test --test write_lock_edge_cases open_read_only_or_degrade -- --test-threads=1`
Expected: FAIL to compile (`open_read_only_or_degrade`/`DegradeReason` don't exist yet).

- [ ] **Step 3: Implement `open_read_only_or_degrade`**

`GraphStore` and `GraphCorruption` are both re-exported from `crates/infigraph-core/src/graph/mod.rs` (confirm the exact existing `pub use store::{...}` line there) so external crates reference them as `infigraph_core::graph::GraphStore` rather than reaching into the `store` submodule directly — add `DegradeReason` to that same re-export list so `crates/infigraph-mcp` (Task 4) can reference it as `infigraph_core::graph::DegradeReason`.

Add to `crates/infigraph-core/src/graph/store.rs`, beside the existing `open_read_only`:

```rust
/// Reason a read was served from something other than the live graph.
#[derive(Debug, Clone)]
pub enum DegradeReason {
    /// The live graph had a dead-holder WAL and was quarantined; this read
    /// was served from the most recent cleanly-retired graph instead, while
    /// a rebuild has been signaled to the daemon coordinator in the
    /// background. Callers should surface a staleness banner naming
    /// `snapshot_path`'s age.
    PreCrashSnapshot { snapshot_path: PathBuf, dead_pid: u32 },
}

/// Like [`open_read_only`], but on a dead-holder WAL, quarantines
/// immediately and degrades to the most recent `graph.previous.<ts>` entry
/// (read-only) instead of failing outright -- see R3.1.4b. Never triggers a
/// write itself; it only quarantines (a data-safety move, not a write to
/// the live graph) and leaves a sentinel for the daemon coordinator
/// (`recovery::drain_recovery_sentinel`) to act on asynchronously.
///
/// Internal/test call sites that want the strict, non-degrading behavior
/// keep calling `open_read_only` directly -- it is unchanged.
pub fn open_read_only_or_degrade(path: &Path) -> Result<(Self, Option<DegradeReason>)> {
    let infigraph_dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("graph path {} has no parent directory", path.display()))?;
    let graph_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("graph path {} has no file name", path.display()))?
        .to_string_lossy()
        .into_owned();

    // A tripped crash-loop breaker takes precedence over everything else --
    // a distinct, unambiguous refusal rather than another quarantine
    // attempt or degrade lookup.
    if let Some(attempts) = crate::recovery::crash_loop_detected(infigraph_dir) {
        anyhow::bail!(
            "crash-loop detected: {} auto-rebuild attempts within the last hour -- refusing \
             further automatic rebuilds. Investigate the underlying cause, then delete {} to \
             reset and retry manually with `infigraph index --full`.",
            attempts.len(),
            crate::recovery::crash_loop_marker_path(infigraph_dir).display(),
        );
    }

    if path.exists() {
        let lock_path = db_lock_path(path);
        if let Some(pid) = unclean_shutdown_wal_holder(path, &lock_path) {
            crate::quarantine::quarantine_graph(infigraph_dir, &graph_name)?;
            crate::recovery::mark_recovery_needed(infigraph_dir, pid, path)?;
            return Self::degrade_or_refuse(infigraph_dir, &graph_name, pid);
        }
        return Self::open_read_only(path).map(|s| (s, None));
    }

    // Missing path: either genuinely never indexed, or quarantined by an
    // earlier call (this reader or another one) and not yet rebuilt.
    if crate::recovery::pending_recovery(infigraph_dir) {
        // dead_pid isn't recoverable here (the sentinel only round-trips it
        // internally) -- 0 is an acceptable placeholder in the returned
        // reason since callers only use it for banner wording, not logic.
        return Self::degrade_or_refuse(infigraph_dir, &graph_name, 0);
    }

    anyhow::bail!(
        "no graph exists yet at {} -- run `infigraph index` first",
        path.display()
    );
}

fn degrade_or_refuse(
    infigraph_dir: &Path,
    graph_name: &str,
    dead_pid: u32,
) -> Result<(Self, Option<DegradeReason>)> {
    if let Some(previous) = crate::recovery::find_most_recent_previous(infigraph_dir, graph_name) {
        let store = Self::open_read_only(&previous)?;
        return Ok((
            store,
            Some(DegradeReason::PreCrashSnapshot {
                snapshot_path: previous,
                dead_pid,
            }),
        ));
    }
    anyhow::bail!(
        "graph for {graph_name} is being automatically rebuilt after a detected crash -- \
         retry shortly"
    );
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 cargo test --test write_lock_edge_cases -- --test-threads=1`
Expected: PASS, including the 3 new tests and every pre-existing test in that file (regression check — `open_read_only` itself must be byte-for-byte unchanged in behavior).

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/graph/store.rs crates/infigraph-core/tests/write_lock_edge_cases.rs
git commit -m "feat(recovery): open_read_only_or_degrade -- quarantine-on-read + pre-crash-snapshot fallback (R3.1.4a/b)"
```

---

### Task 3: Wire the daemon coordinator's poll tick to `drain_recovery_sentinel`

**Files:**
- Modify: `crates/infigraph-core/src/watch/mod.rs` (inside `run_write_coordinator`'s `if serve_requests { ... }` block)
- Test: `crates/infigraph-core/tests/daemon_kuzu_e2e.rs` (extend — this file already hosts the real-subprocess daemon integration tests, e.g. `real_cli_index_against_a_real_daemon_completes_and_writes`)

**Interfaces:**
- Consumes: `crate::recovery::drain_recovery_sentinel` (Task 1) — already fully self-contained, so this task is purely the one-line integration plus an end-to-end regression test proving the whole async path actually works through a real daemon process.

- [ ] **Step 1: Hoist the existing `requests_dir` binding and add the new call**

In `crates/infigraph-core/src/watch/mod.rs`, inside `run_write_coordinator`, find:

```rust
        if serve_requests {
            let requests_dir = root.join(".infigraph").join("requests");
            if let Ok(entries) = std::fs::read_dir(&requests_dir) {
```

Replace with:

```rust
        if serve_requests {
            let infigraph_dir = root.join(".infigraph");

            // R3.1.4a/c: translate a pending dead-holder-WAL sentinel into a
            // synthetic FullReindex request (or a crash-loop refusal) before
            // scanning for requests below, so this same tick's scan picks
            // the synthetic request up immediately rather than waiting a
            // full COORDINATOR_TICK.
            if let Err(e) = crate::recovery::drain_recovery_sentinel(&infigraph_dir) {
                eprintln!("[watch] recovery-sentinel handling failed: {e}");
            }

            let requests_dir = infigraph_dir.join("requests");
            if let Ok(entries) = std::fs::read_dir(&requests_dir) {
```

(No other lines in this block change — the existing scan loop, `route_or_serve_request` call, and closing braces stay exactly as they are.)

- [ ] **Step 2: Write the failing end-to-end test**

```rust
// append to crates/infigraph-core/tests/daemon_kuzu_e2e.rs

#[test]
fn a_dead_holder_wal_sentinel_triggers_an_automatic_full_reindex_via_the_real_daemon() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    // Bootstrap a real index so `.infigraph/` and a real daemon-servable
    // graph both exist, matching how a real crash would be discovered.
    run_cli_index(project.path(), &[]);

    let infigraph_dir = project.path().join(".infigraph");
    // Simulate what open_read_only_or_degrade does on detecting a
    // dead-holder WAL, without a real crash: drop the sentinel directly.
    infigraph_core::recovery::mark_recovery_needed(
        &infigraph_dir,
        999_999_999,
        &infigraph_dir.join("graph.corrupt.stub"),
    )
    .unwrap();

    let mut child = Command::new(cli_binary())
        .arg("daemon")
        .current_dir(project.path())
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut cleared = false;
    while std::time::Instant::now() < deadline {
        if !infigraph_core::recovery::pending_recovery(&infigraph_dir) {
            cleared = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();

    assert!(cleared, "daemon must clear the recovery-needed sentinel within 30s");
    assert_eq!(
        infigraph_core::recovery::recent_recovery_attempts(&infigraph_dir).unwrap().len(),
        1,
        "exactly one auto-triggered rebuild must be recorded"
    );
}

#[test]
fn a_third_recovery_trigger_inside_the_window_trips_the_crash_loop_breaker_instead_of_rebuilding() {
    let tmp = tempfile::tempdir().unwrap();
    let infigraph_dir = tmp.path().join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();
    for _ in 0..infigraph_core::recovery::CRASH_LOOP_THRESHOLD {
        infigraph_core::recovery::record_recovery_attempt(&infigraph_dir).unwrap();
    }
    infigraph_core::recovery::mark_recovery_needed(
        &infigraph_dir,
        1,
        &infigraph_dir.join("graph.corrupt.stub"),
    )
    .unwrap();

    infigraph_core::recovery::drain_recovery_sentinel(&infigraph_dir).unwrap();

    assert!(
        infigraph_core::recovery::crash_loop_detected(&infigraph_dir).is_some(),
        "breaker must trip at the threshold"
    );
    let requests: usize = std::fs::read_dir(infigraph_dir.join("requests"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(requests, 0, "must not submit another FullReindex once tripped");
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test --test daemon_kuzu_e2e crash_loop -- --test-threads=1`
Expected: the crash-loop test passes already (it only depends on Task 1, already implemented) — this confirms Task 1's isolation. The `a_dead_holder_wal_sentinel_triggers...` test FAILS (times out / `cleared` is false) before Step 1's wiring lands.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test --test daemon_kuzu_e2e crash_loop -- --test-threads=1`
Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test --test daemon_kuzu_e2e a_dead_holder_wal_sentinel -- --test-threads=1`
Expected: both PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/watch/mod.rs crates/infigraph-core/tests/daemon_kuzu_e2e.rs
git commit -m "feat(recovery): daemon coordinator drains the recovery sentinel each tick (R3.1.4a/c)"
```

---

### Task 4: Wire MCP `search`/`get_code_snippet` to the degrade path + staleness banner

**Confirmed via `trace_callees` + direct reads (not assumed):** every MCP read tool — `search`'s `get_search_data_local` and `get_code_snippet`'s `tool_get_code_snippet` included — funnels through exactly one shared chokepoint: `helpers::open_prism_read_only` → `Infigraph::init_read_only` → `graph::KuzuBackend::open_read_only` → `GraphStore::open_read_only`. Rather than touching either tool's body directly, this task adds a parallel `_or_degrade` sibling at each layer of that chain (new functions, existing ones untouched) so only the two call sites that need the degrade signal switch to it — the other 47 callers of `open_prism_read_only` are unaffected. `search`'s result additionally passes through an mtime-keyed cache (`get_or_build_search_ctx`/`CachedSearchData`) that this task deliberately does not thread the degrade signal through (see Step 3) — a second, cheap metadata-only open handles that tool's banner independently of its cache.

**Files:**
- Modify: `crates/infigraph-core/src/graph/kuzu_backend.rs` (new `KuzuBackend::open_read_only_or_degrade`)
- Modify: `crates/infigraph-core/src/lib.rs` (new `Infigraph::init_read_only_or_degrade`)
- Modify: `crates/infigraph-mcp/src/tools/helpers.rs` (new `open_prism_read_only_or_degrade`)
- Modify: `crates/infigraph-mcp/src/tools/graph.rs` (`tool_get_code_snippet`, L66-106 — switches to the new helper)
- Modify: `crates/infigraph-mcp/src/tools/search.rs` (`tool_search` — adds an independent degrade check alongside the existing `staleness_banner(root)` call)
- Test: `crates/infigraph-mcp/tests/watcher_concurrency.rs` (extend — sibling to the existing `test_search_auto_starts_watcher_when_none_running`/`test_no_stale_warning_with_*` tests)

**Interfaces:**
- Consumes: `GraphStore::open_read_only_or_degrade`, `DegradeReason` (Task 2)
- Produces: `KuzuBackend::open_read_only_or_degrade(path: &Path) -> Result<(Self, Option<DegradeReason>)>`, `Infigraph::init_read_only_or_degrade(&mut self) -> Result<Option<DegradeReason>>`, `helpers::open_prism_read_only_or_degrade(args: &Value) -> Result<(Infigraph, Option<DegradeReason>)>`, `fn degrade_banner(reason: &DegradeReason) -> String`

- [ ] **Step 1: Write the failing tests**

```rust
// append to crates/infigraph-mcp/tests/watcher_concurrency.rs

fn seed_dead_holder_wal_with_a_previous_pool_entry(project: &std::path::Path) {
    let infigraph_dir = project.join(".infigraph");
    let graph_path = infigraph_dir.join("graph");

    // A real, openable "previous" graph a full reindex would have retired.
    infigraph_core::graph::GraphStore::open(&graph_path).unwrap();
    std::fs::rename(&graph_path, infigraph_dir.join("graph.previous.111")).unwrap();

    // The live path re-created as a dead-holder-WAL scenario.
    std::fs::write(&graph_path, vec![0u8; 4096]).unwrap();
    std::fs::write(infigraph_dir.join("graph.wal"), b"stub wal").unwrap();
    infigraph_core::lockfile::write_holder_for_test(&infigraph_dir.join("graph.lock"), 999_999_999);
}

#[test]
fn get_code_snippet_degrades_to_the_previous_snapshot_and_banners_it() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();
    run_bootstrap_index(project.path()); // this file's existing bootstrap helper

    seed_dead_holder_wal_with_a_previous_pool_entry(project.path());

    let args = serde_json::json!({
        "path": project.path().to_string_lossy(),
        "symbol_id": "a.py::a",
    });
    let out = infigraph_mcp::tools::graph::tool_get_code_snippet(&args).unwrap();
    assert!(
        out.contains("pre-crash snapshot"),
        "expected a degrade banner, got: {out}"
    );
}

#[test]
fn search_degrades_to_the_previous_snapshot_and_banners_it() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();
    run_bootstrap_index(project.path());

    seed_dead_holder_wal_with_a_previous_pool_entry(project.path());

    let args = serde_json::json!({ "path": project.path().to_string_lossy(), "query": "a" });
    let out = infigraph_mcp::tools::search::tool_search(&args).unwrap();
    assert!(
        out.contains("pre-crash snapshot"),
        "expected a degrade banner, got: {out}"
    );
}
```

(`run_bootstrap_index` and `infigraph_core::lockfile::write_holder_for_test` must already exist as this file's/that module's established test helpers — confirm their exact names/signatures by reading `watcher_concurrency.rs`'s existing tests and `store.rs`'s existing dead-holder tests before writing this step for real, mirroring Task 2's identical note.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd crates/infigraph-mcp && CARGO_PROFILE_DEV_DEBUG=0 env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test --test watcher_concurrency degrades_to_the_previous_snapshot -- --test-threads=1`
Expected: FAIL to compile (the `_or_degrade` chain doesn't exist yet).

- [ ] **Step 3: Implement the chain**

`crates/infigraph-core/src/graph/kuzu_backend.rs`, beside the existing `open_read_only`:

```rust
    pub fn open_read_only_or_degrade(path: &Path) -> Result<(Self, Option<DegradeReason>)> {
        let (store, reason) = GraphStore::open_read_only_or_degrade(path)?;
        Ok((Self { store }, reason))
    }
```

`crates/infigraph-core/src/lib.rs`, beside the existing `init_read_only`:

```rust
    /// Like [`init_read_only`], but degrades to the most recent pre-crash
    /// snapshot on a dead-holder WAL instead of failing outright -- see
    /// `graph::store::open_read_only_or_degrade`. Neo4j has no local-graph
    /// crash-recovery concept, so it always returns `Ok(None)` there.
    pub fn init_read_only_or_degrade(&mut self) -> Result<Option<graph::DegradeReason>> {
        let backend_env = std::env::var("INFIGRAPH_BACKEND").unwrap_or_else(|_| "kuzu".into());
        match backend_env.as_str() {
            #[cfg(feature = "neo4j")]
            "neo4j" => {
                let neo = graph::Neo4jBackend::connect_from_env()?;
                self.backend_kind = BackendKind::Neo4j(neo);
                Ok(None)
            }
            #[cfg(not(feature = "neo4j"))]
            "neo4j" => {
                anyhow::bail!("neo4j backend requested but binary compiled without `neo4j` feature")
            }
            _ => {
                let (kb, reason) = open_kuzu_with_retry(
                    || graph::KuzuBackend::open_read_only_or_degrade(&self.db_path),
                    std::time::Duration::from_secs(3),
                )?;
                self.backend_kind = BackendKind::Kuzu(kb);
                Ok(reason)
            }
        }
    }
```

`crates/infigraph-mcp/src/tools/helpers.rs`, beside the existing `open_prism_read_only`:

```rust
pub fn open_prism_read_only_or_degrade(args: &Value) -> Result<(Infigraph, Option<infigraph_core::graph::DegradeReason>)> {
    let raw_path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path' argument")?;
    let path = resolve_project_path(raw_path);
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(&PathBuf::from(&path), registry)?;
    let reason = prism.init_read_only_or_degrade()?;
    apply_repo_filter(&mut prism, &path);
    Ok((prism, reason))
}

pub fn degrade_banner(reason: &infigraph_core::graph::DegradeReason) -> String {
    match reason {
        infigraph_core::graph::DegradeReason::PreCrashSnapshot { snapshot_path, .. } => format!(
            "⚠ serving results from a pre-crash snapshot ({}) -- a WAL corruption was just \
             detected and an automatic rebuild has been triggered in the background; results \
             may lag recent changes until it completes\n\n",
            snapshot_path.display()
        ),
    }
}
```

`crates/infigraph-mcp/src/tools/graph.rs::tool_get_code_snippet` — replace its first line:

```rust
    let prism = open_prism_read_only(args)?;
```

with:

```rust
    let (prism, degrade_reason) = open_prism_read_only_or_degrade(args)?;
```

and at the function's existing final `Ok(out)`, replace with:

```rust
    if let Some(ref reason) = degrade_reason {
        out = format!("{}{out}", degrade_banner(reason));
    }
    Ok(out)
```

(changing `let mut out = ...` above it from `let out = ...` if not already mutable).

`crates/infigraph-mcp/src/tools/search.rs::tool_search` — this tool's actual result composition goes through `get_or_build_search_ctx`'s mtime-keyed cache, which this task deliberately does not thread the degrade signal through (touching the cache's shape risks correctness bugs unrelated to this feature). Instead, add one cheap, independent degrade check alongside the existing `staleness_banner(root)` call already in `tool_search`'s body:

```rust
    let degrade = {
        let args_for_check = serde_json::json!({ "path": raw_path }); // reuse tool_search's own resolved path
        super::helpers::open_prism_read_only_or_degrade(&args_for_check)
            .ok()
            .and_then(|(_, reason)| reason)
    };
```

placed next to the existing `staleness_banner(&root)` call, and prepended to the response text ahead of it (degrade banner first — serving historical data is the more severe condition — then the existing dirty-file staleness banner), using the same `degrade_banner` function defined above.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd crates/infigraph-mcp && CARGO_PROFILE_DEV_DEBUG=0 env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test --test watcher_concurrency -- --test-threads=1`
Expected: PASS, including every pre-existing test in the file.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/graph/kuzu_backend.rs crates/infigraph-core/src/lib.rs crates/infigraph-mcp/src/tools/helpers.rs crates/infigraph-mcp/src/tools/graph.rs crates/infigraph-mcp/src/tools/search.rs crates/infigraph-mcp/tests/watcher_concurrency.rs
git commit -m "feat(recovery): search/get_code_snippet degrade to a pre-crash snapshot with a banner (R3.1.4b)"
```

---

### Task 5: Disk-growth circuit breaker (#100 item 2 — symptom guard, not root cause)

**Files:**
- Modify: `crates/infigraph-core/src/graph/store_util.rs` (sibling to the existing `check_disk_headroom`)
- Modify: `crates/infigraph-core/src/graph/store_write.rs` (`GraphStore::upsert_file`, the fully worked call site)
- Modify: `crates/infigraph-core/src/graph/kuzu_backend.rs` (`upsert_files_bulk` — same pattern, second call site)
- Modify: `crates/infigraph-cli/src/index.rs` (`import_scip_index`'s call site — same pattern, third call site; grep for its existing `check_disk_headroom` call to find the exact spot)
- Test: inline `#[cfg(test)] mod tests` in `store_util.rs`, following the existing `disk_headroom_passes_for_tiny_projected_write_on_real_dir` test's style

**Interfaces:**
- Produces: `pub(crate) fn check_graph_growth_ratio(infigraph_dir: &Path, graph_path: &Path) -> Result<(), String>`, `pub(crate) fn stamp_healthy_graph_size(infigraph_dir: &Path, graph_path: &Path)`

- [ ] **Step 1: Write the failing tests**

```rust
// add to crates/infigraph-core/src/graph/store_util.rs's existing #[cfg(test)] mod tests

#[test]
fn growth_check_establishes_a_baseline_on_first_call_rather_than_refusing() {
    let tmp = tempfile::tempdir().unwrap();
    let graph_path = tmp.path().join("graph");
    std::fs::write(&graph_path, vec![0u8; 1024]).unwrap();

    assert!(check_graph_growth_ratio(tmp.path(), &graph_path).is_ok());
    assert!(tmp.path().join("graph.health.json").exists());
}

#[test]
fn growth_check_refuses_once_current_size_exceeds_the_ratio() {
    let tmp = tempfile::tempdir().unwrap();
    let graph_path = tmp.path().join("graph");
    std::fs::write(&graph_path, vec![0u8; 1_000_000]).unwrap();
    stamp_healthy_graph_size(tmp.path(), &graph_path); // baseline: 1MB

    std::fs::write(&graph_path, vec![0u8; 20_000_000]).unwrap(); // 20x -- over the 10x default
    let err = check_graph_growth_ratio(tmp.path(), &graph_path)
        .expect_err("20x growth over a 1MB baseline must be refused at the 10x default");
    assert!(err.contains("healthy size"), "unexpected message: {err}");
}

#[test]
fn growth_check_passes_for_ordinary_growth_under_the_ratio() {
    let tmp = tempfile::tempdir().unwrap();
    let graph_path = tmp.path().join("graph");
    std::fs::write(&graph_path, vec![0u8; 1_000_000]).unwrap();
    stamp_healthy_graph_size(tmp.path(), &graph_path);

    std::fs::write(&graph_path, vec![0u8; 3_000_000]).unwrap(); // 3x -- under the 10x default
    assert!(check_graph_growth_ratio(tmp.path(), &graph_path).is_ok());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 cargo test --lib store_util::tests::growth -- --test-threads=1`
Expected: FAIL to compile (functions don't exist yet).

- [ ] **Step 3: Implement the growth check**

Add to `crates/infigraph-core/src/graph/store_util.rs`, near `check_disk_headroom`:

```rust
const GRAPH_GROWTH_MAX_RATIO_ENV: &str = "INFIGRAPH_GRAPH_GROWTH_MAX_RATIO";
/// Observed pathological incidents were 40-70x a healthy graph's size;
/// 10x gives wide headroom for legitimate growth (large refactors, new
/// language support landing) while still catching the actual pattern well
/// before it reaches disk-filling scale. Env-overridable following the
/// exact precedent `quarantine::quarantine_max_bytes`'s
/// `INFIGRAPH_QUARANTINE_MAX_BYTES` already sets.
const DEFAULT_GRAPH_GROWTH_MAX_RATIO: u64 = 10;

fn graph_growth_max_ratio() -> u64 {
    std::env::var(GRAPH_GROWTH_MAX_RATIO_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_GRAPH_GROWTH_MAX_RATIO)
}

fn graph_health_path(infigraph_dir: &Path) -> std::path::PathBuf {
    infigraph_dir.join("graph.health.json")
}

fn read_healthy_size(infigraph_dir: &Path) -> Option<u64> {
    let content = std::fs::read_to_string(graph_health_path(infigraph_dir)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("healthy_size_bytes")?.as_u64()
}

/// Refreshes the recorded "last known healthy size" baseline. Call this
/// after any write that completes successfully (the same call sites that
/// call `check_graph_growth_ratio` before the write), so the baseline
/// tracks legitimate growth over time rather than freezing at whatever
/// size the graph happened to be the first time this feature ran.
pub(crate) fn stamp_healthy_graph_size(infigraph_dir: &Path, graph_path: &Path) {
    let Ok(meta) = std::fs::metadata(graph_path) else {
        return; // nothing written yet -- nothing to stamp
    };
    let payload = serde_json::json!({ "healthy_size_bytes": meta.len() });
    let _ = crate::daemon_protocol::write_atomic(
        &graph_health_path(infigraph_dir),
        &serde_json::to_string_pretty(&payload).unwrap_or_default(),
    );
}

/// Circuit breaker against the runaway-WAL-growth pattern from #100 (a live
/// graph observed growing 40-70x its healthy size before crashing). This is
/// NOT a fix for the underlying cause -- see the design spec's Non-goals --
/// only a refusal before a write can push the graph further into that
/// pattern. First call for a given `infigraph_dir` establishes the baseline
/// rather than refusing (there's nothing to compare against yet).
pub(crate) fn check_graph_growth_ratio(infigraph_dir: &Path, graph_path: &Path) -> Result<(), String> {
    let Some(healthy) = read_healthy_size(infigraph_dir) else {
        stamp_healthy_graph_size(infigraph_dir, graph_path);
        return Ok(());
    };
    let Ok(meta) = std::fs::metadata(graph_path) else {
        return Ok(()); // fresh/missing graph -- nothing to compare
    };
    let current = meta.len();
    let max_allowed = healthy.saturating_mul(graph_growth_max_ratio());
    if current > max_allowed {
        return Err(format!(
            "graph at {} is {} MB, {}x its recorded healthy size ({} MB) -- refusing further \
             growth (cap: {}x, override with {GRAPH_GROWTH_MAX_RATIO_ENV}); this guards against \
             the runaway-WAL-growth pattern from github.com/pradeepmouli/infigraph#100 -- if this \
             growth is legitimate, delete {} to reset the baseline",
            graph_path.display(),
            current / (1024 * 1024),
            current / healthy.max(1),
            healthy / (1024 * 1024),
            graph_growth_max_ratio(),
            graph_health_path(infigraph_dir).display(),
        ));
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 cargo test --lib store_util::tests::growth -- --test-threads=1`
Expected: PASS (3 tests).

- [ ] **Step 5: Wire the check into the three `check_disk_headroom` call sites**

In `crates/infigraph-core/src/graph/store_write.rs::upsert_file`, immediately after the existing `check_disk_headroom` block and before `let conn = self.connection()?;`, add:

```rust
        if let Some(dir) = self.db_dir() {
            if let Err(msg) = super::store_util::check_graph_growth_ratio(dir, &dir.join("graph")) {
                anyhow::bail!("refusing to index -- {msg}");
            }
        }
```

After the write succeeds (end of the function, right before `Ok(())`), add:

```rust
        if let Some(dir) = self.db_dir() {
            super::store_util::stamp_healthy_graph_size(dir, &dir.join("graph"));
        }
```

Apply the identical two-block pattern (preflight check right after the existing `check_disk_headroom` call, post-write stamp right before the function's final `Ok(...)`) at:
- `crates/infigraph-core/src/graph/kuzu_backend.rs::upsert_files_bulk`
- `crates/infigraph-cli/src/index.rs`'s `import_scip_index` call site (grep this file for its existing `check_disk_headroom` call — same file, same function-adjacent pattern)

Each of these two sites already has `db_dir`-equivalent access (a `Path` to the `.infigraph` directory) in scope, since they already call `check_disk_headroom(dir, ...)` — reuse that exact same `dir` binding.

- [ ] **Step 6: Run the full crate test suite for a regression check**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test --lib -- --test-threads=1`
Expected: PASS, no regressions from the new preflight/stamp calls at the three write call sites.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-core/src/graph/store_util.rs crates/infigraph-core/src/graph/store_write.rs crates/infigraph-core/src/graph/kuzu_backend.rs crates/infigraph-cli/src/index.rs
git commit -m "feat(recovery): disk-growth circuit breaker against runaway graph growth (#100 item 2)"
```

---

### Task 6: `infigraph doctor` becomes observe-only

**Files:**
- Modify: `crates/infigraph-core/src/doctor.rs` (whichever check function is found to have the side effect)
- Test: `crates/infigraph-core/tests/doctor.rs` (extend)

**Interfaces:**
- No new public interfaces — this task removes a side effect, it doesn't add one.

- [ ] **Step 1: Write the failing test proving the side effect exists**

```rust
// append to crates/infigraph-core/tests/doctor.rs

#[test]
fn doctor_never_creates_a_watch_lock_as_a_side_effect() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    std::fs::write(project.join("a.py"), "def a():\n    pass\n").unwrap();
    // Bootstrap a real index the same way this file's other tests do, so
    // doctor has something real to check (sidecars, SCIP staleness, etc.)
    // without a watcher ever having run.
    let registry = infigraph_core::lang::LanguageRegistry::with_all_builtin();
    let mut prism = infigraph_core::Infigraph::open(project, registry).unwrap();
    prism.init().unwrap();
    prism.index().unwrap();
    drop(prism);

    assert!(!project.join(".infigraph").join("watch.lock").exists());

    let ctx = infigraph_core::doctor::DoctorContext::for_project(project);
    let _ = infigraph_core::doctor::run_doctor(&ctx);

    assert!(
        !project.join(".infigraph").join("watch.lock").exists(),
        "running doctor must never spawn a watcher as a side effect"
    );
}
```

(Adjust `DoctorContext::for_project`/`run_doctor`'s exact names/signatures to match what's actually in `doctor.rs` — read the file's existing test file, `crates/infigraph-core/tests/doctor.rs`, for the real construction pattern before writing this step for real; the shape above is illustrative of the assertion, not a guess at unseen APIs.)

- [ ] **Step 2: Run the test to confirm it currently fails**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 cargo test --test doctor doctor_never_creates_a_watch_lock -- --test-threads=1`
Expected: FAIL — `watch.lock` exists after running doctor.

- [ ] **Step 3: Find and fix the offending call site**

Grep `crates/infigraph-core/src/doctor.rs` for every place a check function opens a project: any call to `Infigraph::open`, `Infigraph::init`, or a backend constructor that is NOT already `GraphStore::open_read_only`/`KuzuBackend::open_read_only` (the pattern `check_one_project_scip_staleness` already gets right — confirmed clean in this plan's own research). Any check found calling the non-read-only path carries `ensure_daemon_for_writes`'s auto-spawn side effect (confirmed via `plain_index_on_a_never_indexed_project_fails_fast_under_daemon_backend`'s own doc comment in `daemon_kuzu_e2e.rs`, which documents this exact side effect on the write-open path).

Convert whatever is found to open read-only instead, mirroring `check_one_project_scip_staleness`'s existing pattern:

```rust
let store = crate::graph::GraphStore::open_read_only(&graph_path).ok()?;
```

If the check function was relying on state only the write-path open computes (unlikely, but verify), that state must be recomputed from the read-only connection instead — never re-add the write-path open to preserve it.

- [ ] **Step 4: Run the test to confirm it passes**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 cargo test --test doctor -- --test-threads=1`
Expected: PASS, including every pre-existing test in the file (this is a behavior-removal change — the full file's regression coverage matters more than usual here).

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/doctor.rs crates/infigraph-core/tests/doctor.rs
git commit -m "fix(doctor): doctor never spawns a watcher as a side effect of diagnosing (#100 second incident)"
```

---

### Task 7: Plain `index` auto-promotes to full rebuild when the graph is missing post-corruption

**Files:**
- Modify: `crates/infigraph-cli/src/index.rs` (`cmd_index`)
- Test: `crates/infigraph-core/tests/daemon_kuzu_e2e.rs` (extend, sibling to the existing `plain_index_on_a_never_indexed_project_fails_fast_under_daemon_backend`)

**Interfaces:**
- No new public interfaces — this is a control-flow change inside `cmd_index`.

- [ ] **Step 1: Write the failing test**

```rust
// append to crates/infigraph-core/tests/daemon_kuzu_e2e.rs, near
// plain_index_on_a_never_indexed_project_fails_fast_under_daemon_backend

#[test]
fn plain_index_auto_promotes_to_a_full_rebuild_when_the_graph_is_missing_but_infigraph_dir_exists() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    // A previously-indexed project (`.infigraph/` exists) whose graph file
    // was deleted -- the exact post-corruption-manual-cleanup scenario from
    // the #100 second-incident comment, distinct from the never-indexed
    // scenario the existing sibling test covers.
    run_cli_index(project.path(), &[]);
    std::fs::remove_file(project.path().join(".infigraph").join("graph")).unwrap();

    let output = run_cli_index(project.path(), &[]); // plain `index`, no --full
    assert!(
        output.status.success(),
        "plain index must auto-promote to a full rebuild, not fail: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(project.path().join(".infigraph").join("graph").exists());

    // No stray locks left behind by the promoted path.
    for stray in ["graph.rebuilding.lock", "index.lock"] {
        assert!(
            !project.path().join(".infigraph").join(stray).exists(),
            "{stray} must not be left behind"
        );
    }
}
```

(Reuse this file's existing `run_cli_index` helper, already used by `real_cli_index_against_a_real_daemon_completes_and_writes` — confirm its exact signature by reading it before writing this step for real, and pass whatever flag array shape it expects.)

- [ ] **Step 2: Run the test to confirm it currently fails**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test --test daemon_kuzu_e2e plain_index_auto_promotes -- --test-threads=1`
Expected: FAIL (today's confusing Kuzu read-only-mode error).

- [ ] **Step 3: Implement the auto-promotion**

In `crates/infigraph-cli/src/index.rs::cmd_index`, right after the `op_guard` acquisition block and before `if full {`, add:

```rust
    // A previously-indexed project (`.infigraph/` exists) whose graph file
    // is missing has nothing incremental to protect -- treat it exactly
    // like `--full` rather than letting it fall through to an incremental
    // open that fails with Kuzu's own confusing "Cannot create an empty
    // database under READ ONLY mode" error (#100 second-incident comment).
    let tg_dir = root.join(".infigraph");
    let full = full || (tg_dir.exists() && !tg_dir.join("graph").exists());
```

This binds a new (shadowing) `full` before the existing `if full { ... }` block runs, so every branch inside it (remote, daemon-backend, local) already handles the promoted case correctly with no further changes needed.

- [ ] **Step 4: Run the test to confirm it passes**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test --test daemon_kuzu_e2e plain_index -- --test-threads=1`
Expected: PASS, both the new test and the existing `plain_index_on_a_never_indexed_project_fails_fast_under_daemon_backend` (which must remain unaffected — it never creates `.infigraph/` at all, so the new condition is false for it).

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-cli/src/index.rs crates/infigraph-core/tests/daemon_kuzu_e2e.rs
git commit -m "fix(cli): plain index auto-promotes to full rebuild when the graph is missing post-corruption (#100 second incident)"
```

---

### Task 8: Per-instance daemon crash-log discoverability

**Files:**
- Modify: `crates/infigraph-core/src/watch/daemon.rs` (`build_daemon_command`)
- Modify: `crates/infigraph-core/src/doctor.rs` (`check_one_watcher` — surface the log path in its output)
- Test: `crates/infigraph-core/tests/watch_daemon.rs` (extend, sibling to the existing `build_daemon_command_appends_to_an_existing_log_instead_of_truncating`)
- Test: `crates/infigraph-core/tests/doctor.rs` (extend)

**Interfaces:**
- No new public interfaces.

**Context, confirmed via code reading:** `build_daemon_command` already redirects every detached-daemon spawn's stderr to a per-project `watch.log`, appended (never truncated) across daemon generations, size-capped via the existing `logrotate` module. Traced `ensure_daemon_watcher` (MCP's opportunistic auto-start path) → `ensure_daemon_running` → `ensure_daemon_running_required` → `spawn_daemon` → `build_daemon_command`: every spawn path, explicit or opportunistic, already goes through this same function. So stderr capture is NOT actually missing (unlike the spec's initial framing, written before this trace ran) — the real gap is that #115's investigation checked `~/.infigraph/logs/` (the global directory) and found only `audit.log`, because a per-project `watch.log` isn't discoverable from there without already knowing to look. This task closes that discoverability gap rather than adding new capture plumbing.

- [ ] **Step 1: Write the failing tests**

```rust
// append to crates/infigraph-core/tests/watch_daemon.rs

#[test]
fn build_daemon_command_writes_a_pid_and_timestamp_banner_at_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let tg_dir = tmp.path().join(".infigraph");
    std::fs::create_dir_all(&tg_dir).unwrap();
    let watch_binary = std::env::current_exe().unwrap(); // any real executable works for building the Command

    let _cmd = infigraph_core::watch::daemon::build_daemon_command(tmp.path(), &tg_dir, &watch_binary);

    let log = std::fs::read_to_string(tg_dir.join("watch.log")).unwrap();
    assert!(
        log.contains("[daemon-start]") && log.contains("pid="),
        "expected a start banner naming the pid, got: {log:?}"
    );
}
```

```rust
// append to crates/infigraph-core/tests/doctor.rs

#[test]
fn doctor_surfaces_the_watch_log_path_for_a_live_watcher() {
    // Using this file's existing watcher-liveness test setup (a real
    // watch.lock with a live holder, mirroring
    // check_watchers_warns_when_alive_watcher_has_no_live_mcp_instance's
    // existing fixture pattern), assert the PASS/WARN message for a
    // healthy watcher includes the watch.log path so a human doesn't need
    // to already know the per-project convention to find it.
    // (Fill in using this file's existing fixture helper for a live
    // watch.lock holder before implementing Step 3.)
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 cargo test --test watch_daemon build_daemon_command_writes_a_pid -- --test-threads=1`
Expected: FAIL — no banner written today.

- [ ] **Step 3: Implement the start banner**

In `crates/infigraph-core/src/watch/daemon.rs::build_daemon_command`, right after the existing `rotate_if_over(&log_path, 10 * 1024 * 1024);` call and before opening `stderr_target`, add:

```rust
    // R3.1.4g/#115: a crash's cause is only diagnosable if a human (or
    // doctor) can find which generation's output is whose inside the
    // shared, appended-to watch.log. A banner line at spawn gives every
    // generation a clear, greppable boundary.
    let banner = format!(
        "[daemon-start] pid={} started_at={}\n",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        use std::io::Write;
        let _ = f.write_all(banner.as_bytes());
    }
```

Note: this writes the banner from the *parent* process (the one calling `build_daemon_command`), before the child ever runs — the pid logged is the not-yet-spawned child's pid, which isn't known yet at this point. Fix: move this banner write to happen from inside the spawned child instead (the `infigraph daemon` command's own entry point, e.g. wherever `cmd_daemon`/the daemon's `main` first runs, using `std::process::id()` there, which IS the child's own pid at that point). Locate that entry point (grep `crates/infigraph-cli/src/main.rs` for the `daemon` subcommand's handler) and add the banner write there instead, using the same `tg_dir.join("watch.log")` path convention — remove the snippet above from `build_daemon_command` and place the equivalent write at the actual daemon-process entry point.

- [ ] **Step 4: Implement `doctor`'s log-path surfacing**

In `crates/infigraph-core/src/doctor.rs::check_one_watcher`, in the final `CheckResult::pass` branch (the "watcher alive with fresh heartbeat" case) and in the "alive but no live MCP instance" warn branch, append the log path to the message:

```rust
    CheckResult::pass(
        WATCHER_CATEGORY,
        label,
        format!(
            "watcher (PID {}) alive with fresh heartbeat -- log: {}",
            holder.pid,
            project_path.join(".infigraph").join("watch.log").display()
        ),
    )
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 cargo test --test watch_daemon -- --test-threads=1`
Run: `cd crates/infigraph-core && CARGO_PROFILE_DEV_DEBUG=0 cargo test --test doctor -- --test-threads=1`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/watch/daemon.rs crates/infigraph-core/src/doctor.rs crates/infigraph-cli/src/main.rs crates/infigraph-core/tests/watch_daemon.rs crates/infigraph-core/tests/doctor.rs
git commit -m "feat(recovery): daemon-instance start banner + doctor surfaces the watch.log path (#115 item 4)"
```

---

### Task 9: Update `docs/DESIGN-hardening.md`'s Implementation Status

**Files:**
- Modify: `docs/DESIGN-hardening.md`

**Interfaces:** none — documentation only.

- [ ] **Step 1: Move R3.1.4 from "Not started" to "Shipped"**

In the "Not started" section, remove the R3.1.4 line. In the "Shipped" section, add an entry following the exact style of the R3.1.3 entry immediately above where R3.1.4 was filed (§3.1), naming: the sentinel/attempts-log mechanism (`recovery.rs`), `open_read_only_or_degrade`, the coordinator wiring, the disk-growth breaker, and the three second-incident UX fixes (doctor observe-only, plain-index auto-promotion, daemon-log discoverability) — each with its file/module reference, mirroring how the R3.1.3 entry cites `unclean_shutdown_wal_holder` by name and file.

- [ ] **Step 2: Update §3.1's R3.1.4 body text**

Replace the "Not yet scoped into tasks" closing line of R3.1.4's own description (§3.1) with a pointer to the shipped implementation, matching how R3.1.3's own body text ends with "**Fixed** ([#92](...))" and names the exact mechanism.

- [ ] **Step 3: Commit**

```bash
git add docs/DESIGN-hardening.md
git commit -m "docs(hardening): mark R3.1.4 shipped -- WAL auto-recovery, crash-loop breaker, disk/quarantine hardening"
```

---

## Self-Review Notes

**Spec coverage:** Component A → Tasks 2 (read-side) + 3 (daemon-side). Component B → Tasks 2 (degrade lookup) + 4 (MCP wiring). Component C → Tasks 1 (primitives) + 3 (integration + test). Component D → Task 5. Component E → Task 6. Component F → Task 7. Component G → Task 8 (scope corrected during planning — stderr capture already existed; the real gap was discoverability, confirmed by tracing the actual spawn call chain rather than assuming the spec's initial framing). Documentation wrap-up → Task 9. Every component has at least one task; no gaps found.

**Deviations from the spec, and why:** (1) Component D's baseline-stamping mechanism moved from "stamp at full-reindex completion" (spec) to "stamp after every successful write at the same three call sites `check_disk_headroom` already guards" (plan) — functionally equivalent (both keep the baseline current), but self-contained within `store_util.rs` rather than requiring a new integration point inside `finish_full_reindex`'s internals, which this planning pass did not need to trace. (2) Component G's scope shrank after Task 8's research step traced the actual spawn call chain and found stderr capture already works end-to-end for every spawn path — the plan fixes the real gap (discoverability from the global log directory) instead of rebuilding something that already exists. (3) Component B's consumer wiring (Task 4) turned out simpler than the spec anticipated: `trace_callees` confirmed both `search` and `get_code_snippet` funnel through exactly one shared chokepoint (`helpers::open_prism_read_only` → `Infigraph::init_read_only` → `KuzuBackend::open_read_only` → `GraphStore::open_read_only`), so Task 4 adds one `_or_degrade` sibling per layer rather than touching either tool's body ad hoc — fully resolved during planning, no discovery step needed at execution time.

**Known incomplete steps, flagged rather than hidden:** Task 6 Step 1 and Task 8's doctor test each depend on a fact this planning pass could not resolve without running code (the exact doctor check with the side effect, and this file's existing fixture helpers, respectively) — each is written as an explicit discovery step with a concrete deliverable (find and write down the exact location/helper) before the step that needs it, not silently assumed. Task 4's original discovery step was fully resolved during this planning pass (see deviation 3 above) and no longer has one.
