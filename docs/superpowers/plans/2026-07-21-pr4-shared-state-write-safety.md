# PR4: Shared-State Write Safety (sessions + registry) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the two confirmed unlocked-write races in shared state — `.infigraph/sessions/` (JSON + narrative + `embeddings.bin`, hit by every `save_session`/`consolidate_memory`/`purge_sessions` call) and `~/.infigraph/registry.json` (hit by every `index`/`group` command) — using the existing `lockfile` module from PR1, plus make the `embeddings.bin` sidecar format atomic (temp+rename) per R3.3.1.

**Architecture:** Two new lock files, both built on the existing `crates/infigraph-core/src/lockfile.rs` primitive (`lockfile::acquire(path, role, timeout) -> Result<LockFile>`, an RAII guard that releases on drop): `.infigraph/sessions/sessions.lock` guards the read-merge-write critical section in `tool_save_session`/`tool_consolidate_memory`/`tool_purge_sessions`; `~/.infigraph/registry.lock` guards `Registry::save()`. Both locks reuse the exact calling convention already established by `index.lock` (PR3, `crates/infigraph-core/src/ops.rs::begin_index_op`) and `graph.lock` (PR1/PR2, `crates/infigraph-core/src/graph/store.rs`) — `lockfile::acquire` returns `anyhow::Result`, and every call site already returns `anyhow::Result`, so `?` propagates the `Busy` error with zero conversion code needed.

**Tech Stack:** Rust, `anyhow` for errors, `serde_json` for the registry/session JSON, `tempfile` (dev-only) for tests, `std::fs::rename` for atomic sidecar swaps (no new crate dependencies).

## Global Constraints

- Every `cargo` invocation in this plan's steps MUST be run with `CARGO_PROFILE_DEV_DEBUG=0` in the environment — this repo has a standing rule that mixing debug-info settings across builds has caused ENOSPC incidents from duplicate C++ build trees. Always: `CARGO_PROFILE_DEV_DEBUG=0 cargo test ...`.
- `lockfile::acquire(path: &Path, role: &str, timeout: Duration) -> Result<LockFile>` already wraps its `Busy` timeout error as `anyhow::Error` (see `crates/infigraph-core/src/lockfile.rs:211-230`) — every call site in this plan uses plain `?`, no `.map_err`/`From` impl needed.
- `role` is a plain `&str` (no enum exists) — use `"session-write"` for both sessions.lock call sites and `"registry-write"` for the registry.lock call site, so a `Busy` error or a future health-beacon integration can identify the contending operation by name.
- Lock timeout: use `Duration::from_secs(10)` for both new locks (named constants, not a magic literal at each call site) — these are small JSON/binary writes, not multi-minute operations like an index run, so a short bounded wait is correct; a caller stuck past 10s indicates a genuinely wedged holder, not normal contention.
- This plan does **not** attempt to eliminate registry.json "lost update" races where two processes each call `Registry::load()`, mutate independently, then both call `save()` — only `Registry::save()` itself is put under the lock (fixing torn/corrupted writes and serializing the writes themselves). A full fix requires restructuring `register_repo`/`group_add`/`group_remove`/`create_group_with_org`/`sync_group_contracts` and their ~11 call sites (spread across `infigraph-mcp` and `infigraph-cli`, including a 575-line `cmd_group` dispatch function that holds a loaded `Registry` across multi-minute indexing operations) into a lock-held load-mutate-save critical section — architecturally a separate, larger piece of work. This is a deliberate scope decision, flagged to the human before Task 4 starts (see Task 4's note).
- Sessions state (`.infigraph/sessions/*.json`, `embeddings.bin`, narrative `.md` files) is per-project; the registry (`~/.infigraph/registry.json`) is global to the user's home directory. Do not conflate the two lock files.

---

### Task 1: Atomic `embed::save_embeddings` (temp + rename)

**Files:**
- Modify: `crates/infigraph-core/src/embed/mod.rs:388-404` (`save_embeddings`)
- Test: `crates/infigraph-core/tests/embed_atomicity.rs` (new file)

**Interfaces:**
- Consumes: nothing new — `save_embeddings`'s signature (`pub fn save_embeddings(path: &Path, embeddings: &[(String, Vec<f32>)]) -> Result<()>`) is unchanged, only its body changes.
- Produces: the same signature, but now writes via temp-file-then-`rename(2)`, so a concurrent reader (via `load_embeddings`, which mmaps the file) never observes a truncated/partial file. Tasks 2 and 3 call this function unchanged and inherit the atomicity for free.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/tests/embed_atomicity.rs`:

```rust
use infigraph_core::embed::{load_embeddings, save_embeddings};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[test]
fn save_embeddings_leaves_no_temp_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("embeddings.bin");
    let data = vec![
        ("a".to_string(), vec![1.0, 2.0]),
        ("b".to_string(), vec![3.0, 4.0]),
    ];
    save_embeddings(&path, &data).unwrap();
    assert!(path.exists());
    assert!(!dir.path().join("embeddings.bin.tmp").exists());
    let loaded = load_embeddings(&path).unwrap();
    assert_eq!(loaded.len(), 2);
}

#[test]
fn concurrent_readers_never_observe_a_torn_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("embeddings.bin");
    // Seed the file so readers have something to load from the start.
    save_embeddings(&path, &[("seed".to_string(), vec![0.0; 8])]).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let write_path = path.clone();
    let writer = std::thread::spawn(move || {
        for i in 0..200 {
            let data: Vec<(String, Vec<f32>)> = (0..50)
                .map(|j| (format!("id-{i}-{j}"), vec![i as f32; 32]))
                .collect();
            save_embeddings(&write_path, &data).unwrap();
        }
    });

    let mut readers = Vec::new();
    for _ in 0..4 {
        let read_path = path.clone();
        let stop = Arc::clone(&stop);
        readers.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(loaded) = load_embeddings(&read_path) {
                    // A torn read would either fail to parse (load_embeddings
                    // returns Err, which the `if let Ok` above already
                    // filters out) or desync the declared count from the
                    // actual entries. The file is always either the 1-entry
                    // seed or one full 50-entry batch — never partial.
                    assert!(
                        loaded.len() == 1 || loaded.len() == 50,
                        "torn read: {} entries",
                        loaded.len()
                    );
                }
            }
        }));
    }

    writer.join().unwrap();
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test embed_atomicity -- --nocapture`

Expected: `save_embeddings_leaves_no_temp_file_behind` passes trivially today (no temp file is ever created by the current implementation, so the "no leftover temp file" assertion is vacuously true) — that's fine, it becomes a real regression guard once Step 3 introduces a temp file. `concurrent_readers_never_observe_a_torn_write` is the meaningful one: it may pass or intermittently fail on the current (non-atomic) implementation depending on thread interleaving and OS write-buffering behavior — it is not guaranteed to fail deterministically pre-fix (this is an inherent property of a race-condition test). Run it a few times in a loop if it passes once: `for i in 1 2 3 4 5; do CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test embed_atomicity concurrent_readers -- --nocapture || break; done`. The important bar is Step 4: it must pass reliably (every run) after the fix.

- [ ] **Step 3: Implement atomic save via temp + rename**

Replace the body of `save_embeddings` in `crates/infigraph-core/src/embed/mod.rs:388-404`:

```rust
/// Save symbol embeddings to a binary file. Format: [count:u32] then for each entry: [id_len:u32][id_bytes][dim:u32][f32 * dim]
pub fn save_embeddings(path: &Path, embeddings: &[(String, Vec<f32>)]) -> Result<()> {
    let tmp_path = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("embeddings.bin")
    ));
    {
        let file = std::fs::File::create(&tmp_path).context("create temp embeddings file")?;
        let mut w = BufWriter::new(file);
        w.write_all(&(embeddings.len() as u32).to_le_bytes())?;
        for (id, vec) in embeddings {
            let id_bytes = id.as_bytes();
            w.write_all(&(id_bytes.len() as u32).to_le_bytes())?;
            w.write_all(id_bytes)?;
            w.write_all(&(vec.len() as u32).to_le_bytes())?;
            for &v in vec {
                w.write_all(&v.to_le_bytes())?;
            }
        }
        w.flush().context("flush temp embeddings file")?;
    }
    std::fs::rename(&tmp_path, path).context("atomically replace embeddings file")?;
    invalidate_embeddings_cache();
    Ok(())
}
```

The only behavioral change: writes go to `<path>.tmp` in the same directory (same filesystem — required for `rename(2)` to be atomic), then swap onto `path` via `rename`. The inner `{ }` block ensures the `BufWriter`/`File` are dropped (flushing to the temp file) before the rename. `invalidate_embeddings_cache()` is called after the rename, unchanged from before.

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test embed_atomicity -- --nocapture`

Expected: both tests `PASS`. Re-run `concurrent_readers_never_observe_a_torn_write` 5 times in a loop to confirm it's not flaky:
`for i in 1 2 3 4 5; do CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test embed_atomicity concurrent_readers -- --nocapture || echo "FAILED run $i"; done`

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/embed/mod.rs crates/infigraph-core/tests/embed_atomicity.rs
git commit -m "fix: embed::save_embeddings writes atomically via temp+rename (R3.3.1)"
```

---

### Task 2: `sessions.lock` around `tool_save_session`'s critical section

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/session.rs:107-256` (`tool_save_session`)
- Test: `crates/infigraph-mcp/src/tools/session.rs` (extend the existing `#[cfg(test)] mod tests` block at the bottom of the file — follow the pattern of `test_multi_session_compact_omits_decisions_body`, which already uses `tempfile::tempdir()` + `SessionStore::open_dir` + calling a `tool_*` function directly by name in the same module)

**Interfaces:**
- Consumes: `lockfile::acquire(path: &Path, role: &str, timeout: Duration) -> Result<LockFile>` from `crates/infigraph-core/src/lockfile.rs` (Task 1 of PR1, already shipped); `embed::save_embeddings`/`embed::load_embeddings` (now atomic per Task 1 of this plan).
- Produces: a `SESSION_LOCK_TIMEOUT: Duration` constant in `session.rs`, reused by Task 3. The lock file path pattern `<sessions_dir>/sessions.lock` (i.e. `<project_root>/.infigraph/sessions/sessions.lock`) — Task 3 constructs this the same way.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block at the bottom of `crates/infigraph-mcp/src/tools/session.rs` (this module already has `use serde_json::json;`, `tempfile`, and calls `tool_*` functions directly — follow the existing style, e.g. `test_multi_session_compact_omits_decisions_body`):

```rust
    #[test]
    fn concurrent_save_session_preserves_all_embeddings() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        std::fs::create_dir_all(project.join(".infigraph")).unwrap();
        let project_str = project.to_str().unwrap().to_string();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for t in 0..2 {
            let project_str = project_str.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..15 {
                    let args = json!({
                        "path": project_str,
                        "name": format!("concurrent-{t}-{i}"),
                        "summary": format!("thread {t} iteration {i}"),
                    });
                    tool_save_session(&args).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let emb_path = project
            .join(".infigraph")
            .join("sessions")
            .join("embeddings.bin");
        let embeddings = embed::load_embeddings(&emb_path).unwrap();
        let ids: std::collections::HashSet<&str> =
            embeddings.iter().map(|(id, _)| id.as_str()).collect();

        for t in 0..2 {
            for i in 0..15 {
                let expected = format!("named_concurrent-{t}-{i}");
                assert!(
                    ids.contains(expected.as_str()),
                    "missing embedding for {expected} — lost update, {} of 30 present",
                    ids.len()
                );
            }
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib tools::session::tests::concurrent_save_session_preserves_all_embeddings -- --nocapture`

Expected: this is a race-condition test (30 interleaved read-modify-write cycles on the shared `embeddings.bin` across 2 threads with no barrier between iterations) — it is very likely to `FAIL` with a "missing embedding" panic on the first run given today's unguarded `load_embeddings`/`retain`/`push`/`save_embeddings` sequence in `tool_save_session`. If it happens to pass on a given run (thread interleaving is non-deterministic), rerun it 3-5 times: `for i in 1 2 3 4 5; do CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib tools::session::tests::concurrent_save_session_preserves_all_embeddings -- --nocapture || break; done` — expect at least one failure across the runs, confirming the race exists before Step 3's fix.

- [ ] **Step 3: Wire `sessions.lock` around the critical section**

Replace the full body of `tool_save_session` in `crates/infigraph-mcp/src/tools/session.rs:107-256`:

```rust
const SESSION_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

pub fn tool_save_session(args: &Value) -> Result<String> {
    let store = open_session_store(args)?;
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let summary = args
        .get("summary")
        .and_then(|s| s.as_str())
        .context("missing 'summary'")?;
    let pending_tasks = args
        .get("pending_tasks")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let decisions = args.get("decisions").and_then(|s| s.as_str()).unwrap_or("");
    let files_touched = args
        .get("files_touched")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let constraints = args
        .get("constraints")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let assumptions = args
        .get("assumptions")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let blockers = args.get("blockers").and_then(|s| s.as_str()).unwrap_or("");
    let narrative = args.get("narrative").and_then(|s| s.as_str()).unwrap_or("");
    let session_name = args.get("name").and_then(|s| s.as_str()).unwrap_or("");

    let now = session_epoch();
    let root = PathBuf::from(path);
    let sessions_dir = root.join(".infigraph").join("sessions");

    let (session_id, session_count) = {
        let _session_lock = lockfile::acquire(
            &sessions_dir.join("sessions.lock"),
            "session-write",
            SESSION_LOCK_TIMEOUT,
        )?;

        let session_id = if session_name.is_empty() {
            session_date_id()
        } else {
            format!("named_{}", session_name.to_lowercase().replace(' ', "_"))
        };

        let new_files: Vec<&str> = files_touched
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        let session = if let Some(existing) = store.load(&session_id)? {
            let merged_decisions = if decisions.is_empty() {
                existing.decisions.clone()
            } else if existing.decisions.is_empty() {
                decisions.to_string()
            } else {
                format!("{} | {}", existing.decisions, decisions)
            };

            let mut all_files: Vec<String> = existing
                .files_touched
                .split(", ")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            for f in &new_files {
                if !all_files.iter().any(|x| x == f) {
                    all_files.push(f.to_string());
                }
            }

            SessionData {
                id: session_id.clone(),
                name: session_name.to_string(),
                summary: summary.to_string(),
                pending_tasks: pending_tasks.to_string(),
                decisions: merged_decisions,
                files_touched: all_files.join(", "),
                constraints: constraints.to_string(),
                assumptions: assumptions.to_string(),
                blockers: blockers.to_string(),
                created_at: existing.created_at,
                updated_at: now,
                confidence: 0.9_f32.max(existing.confidence),
                last_accessed: now,
            }
        } else {
            SessionData {
                id: session_id.clone(),
                name: session_name.to_string(),
                summary: summary.to_string(),
                pending_tasks: pending_tasks.to_string(),
                decisions: decisions.to_string(),
                files_touched: new_files.join(", "),
                constraints: constraints.to_string(),
                assumptions: assumptions.to_string(),
                blockers: blockers.to_string(),
                created_at: now,
                updated_at: now,
                confidence: score_session_value(decisions, constraints, assumptions, blockers),
                last_accessed: now,
            }
        };

        store.save(&session)?;

        if !narrative.is_empty() {
            let md_path = sessions_dir.join(format!("{session_id}.md"));
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&md_path)?;
            let ts_secs = now % 86400;
            let hh = ts_secs / 3600;
            let mm = (ts_secs % 3600) / 60;
            writeln!(f, "\n## Save @ {hh:02}:{mm:02} UTC\n")?;
            writeln!(f, "{narrative}")?;
        }

        let emb_path = sessions_dir.join("embeddings.bin");
        let embed_text = format!(
            "{session_name} {summary} {pending_tasks} {decisions} {constraints} {assumptions} {narrative}"
        );
        let embedder = embed::code_embedder();
        let vec = embedder.embed(&embed_text)?;
        let mut emb_store = embed::load_embeddings(&emb_path).unwrap_or_default();
        emb_store.retain(|(id, _)| id != &session_id);
        emb_store.push((session_id.clone(), vec));
        embed::save_embeddings(&emb_path, &emb_store)?;

        (session_id, emb_store.len())
    };
    // `_session_lock` is dropped here (end of block), released before the
    // auto-consolidation call below — tool_consolidate_memory acquires the
    // same sessions.lock itself (Task 3), so holding it across that call
    // would self-deadlock.

    let auto_consolidated = if session_count > 50 {
        let consolidate_args = serde_json::json!({ "path": path, "threshold": 0.7 });
        tool_consolidate_memory(&consolidate_args).ok()
    } else {
        None
    };

    let mut result = if session_name.is_empty() {
        format!("Session saved: {session_id}")
    } else {
        format!("Session saved: {session_id} (name: {session_name})")
    };

    if let Some(consolidation_msg) = auto_consolidated {
        result.push_str(&format!(
            "\n\n**Auto-consolidation triggered ({session_count} sessions):**\n{consolidation_msg}"
        ));
    }

    Ok(result)
}
```

Add `use infigraph_core::lockfile;` and `use std::time::Duration;` to the top of `crates/infigraph-mcp/src/tools/session.rs` if not already present (check the existing `use` block first — `embed`, `PathBuf`, `Context`, `Result`, `Value` are already imported given the pre-existing function body).

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib tools::session:: -- --nocapture`

Expected: `concurrent_save_session_preserves_all_embeddings` passes, along with every other existing test in `tools::session` (e.g. `test_date_from_session_id`, `test_multi_session_compact_omits_decisions_body`). Re-run the concurrency test 5 times to confirm it's not flaky:
`for i in 1 2 3 4 5; do CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib tools::session::tests::concurrent_save_session_preserves_all_embeddings -- --nocapture || echo "FAILED run $i"; done`

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-mcp/src/tools/session.rs
git commit -m "feat: sessions.lock guards tool_save_session's read-merge-write critical section (R2.3.6)"
```

---

### Task 3: `sessions.lock` around `tool_consolidate_memory` and `tool_purge_sessions`

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/session.rs:718-943` (`tool_consolidate_memory`)
- Modify: `crates/infigraph-mcp/src/tools/session.rs:558-615` (`tool_purge_sessions`)
- Test: `crates/infigraph-mcp/src/tools/session.rs` (extend `#[cfg(test)] mod tests`, same as Task 2)

**Interfaces:**
- Consumes: `lockfile::acquire` and `SESSION_LOCK_TIMEOUT` from Task 2 (same file, same constant — do not redefine it).
- Produces: nothing new for later tasks — this is the last sessions.lock call site.

- [ ] **Step 1: Write the failing test**

Add to the same `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn concurrent_save_and_purge_do_not_corrupt_embeddings() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        std::fs::create_dir_all(project.join(".infigraph")).unwrap();
        let project_str = project.to_str().unwrap().to_string();

        // Seed a handful of sessions so purge has something to look at.
        for i in 0..3 {
            let args = json!({
                "path": project_str,
                "name": format!("seed-{i}"),
                "summary": format!("seed session {i}"),
            });
            tool_save_session(&args).unwrap();
        }

        let save_path = project_str.clone();
        let saver = std::thread::spawn(move || {
            for i in 0..10 {
                let args = json!({
                    "path": save_path,
                    "name": format!("racer-{i}"),
                    "summary": format!("racer session {i}"),
                });
                tool_save_session(&args).unwrap();
            }
        });

        let purge_path = project_str.clone();
        let purger = std::thread::spawn(move || {
            for _ in 0..10 {
                let args = json!({ "path": purge_path, "older_than_days": 9999 });
                tool_purge_sessions(&args).ok();
            }
        });

        saver.join().unwrap();
        purger.join().unwrap();

        // The embeddings file must remain parseable after the race — a torn
        // or corrupted write would make load_embeddings return Err.
        let emb_path = project
            .join(".infigraph")
            .join("sessions")
            .join("embeddings.bin");
        let loaded = embed::load_embeddings(&emb_path);
        assert!(loaded.is_ok(), "embeddings.bin corrupted: {loaded:?}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib tools::session::tests::concurrent_save_and_purge_do_not_corrupt_embeddings -- --nocapture`

Expected: may `PASS` or `FAIL` depending on interleaving (same non-determinism caveat as Task 2's test) — `tool_save_session` is already lock-protected from Task 2, but `tool_purge_sessions` is not yet, so its unguarded `load_embeddings`/`retain`/`save_embeddings` sequence can race against a concurrent `tool_save_session` call and corrupt `embeddings.bin`. Rerun 3-5 times if it passes once, same as Task 2 Step 2.

- [ ] **Step 3: Wire `sessions.lock` into both functions**

Replace the full body of `tool_purge_sessions` in `crates/infigraph-mcp/src/tools/session.rs:558-615`:

```rust
pub fn tool_purge_sessions(args: &Value) -> Result<String> {
    let store = open_session_store(args)?;
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let older_than_days = args
        .get("older_than_days")
        .and_then(|v| v.as_u64())
        .unwrap_or(30);

    let now = session_epoch();
    let cutoff = now - (older_than_days as i64 * 86400);

    let all = store.list_all()?;
    let to_purge: Vec<&SessionData> = all.iter().filter(|s| s.created_at < cutoff).collect();

    if to_purge.is_empty() {
        return Ok(format!(
            "No sessions older than {older_than_days} days found."
        ));
    }

    let purged_ids: Vec<String> = to_purge.iter().map(|s| s.id.clone()).collect();
    let root = PathBuf::from(path);
    let sessions_dir = root.join(".infigraph").join("sessions");
    let _session_lock = lockfile::acquire(
        &sessions_dir.join("sessions.lock"),
        "session-write",
        SESSION_LOCK_TIMEOUT,
    )?;

    for id in &purged_ids {
        store.delete(id)?;
    }

    let emb_path = sessions_dir.join("embeddings.bin");
    if emb_path.exists() {
        let mut emb_store = embed::load_embeddings(&emb_path).unwrap_or_default();
        let before = emb_store.len();
        emb_store.retain(|(id, _)| !purged_ids.contains(id));
        if emb_store.len() < before {
            embed::save_embeddings(&emb_path, &emb_store)?;
        }
    }

    let mut out = format!(
        "Purged {} session(s) older than {} days:\n",
        to_purge.len(),
        older_than_days
    );
    for s in &to_purge {
        let preview = if s.summary.len() > 60 {
            &s.summary[..60]
        } else {
            &s.summary
        };
        out.push_str(&format!("- {}: {preview}\n", s.id));
    }
    Ok(out)
}
```

Replace the full body of `tool_consolidate_memory` in `crates/infigraph-mcp/src/tools/session.rs:718-943`:

```rust
pub fn tool_consolidate_memory(args: &Value) -> Result<String> {
    let store = open_session_store(args)?;
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let similarity_threshold = args
        .get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.7) as f32;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let root = PathBuf::from(path);
    let sessions_dir = root.join(".infigraph").join("sessions");

    let (consolidated_count, mut out) = {
        let _session_lock = lockfile::acquire(
            &sessions_dir.join("sessions.lock"),
            "session-write",
            SESSION_LOCK_TIMEOUT,
        )?;

        // Purge expired sessions (confidence < 0.1)
        let purged = store.purge_expired(now)?;

        let emb_path = sessions_dir.join("embeddings.bin");

        if !emb_path.exists() {
            let msg = if purged.is_empty() {
                "No session embeddings found. Nothing to consolidate.".to_string()
            } else {
                format!(
                    "Purged {} expired sessions. No embeddings to consolidate.",
                    purged.len()
                )
            };
            return Ok(msg);
        }

        let emb_store = embed::load_embeddings(&emb_path)?;
        if emb_store.len() < 2 {
            return Ok("Fewer than 2 sessions — nothing to consolidate.".to_string());
        }

        // Load all active sessions with embeddings
        let mut sessions_with_emb: Vec<(SessionData, Vec<f32>)> = Vec::new();
        for (id, emb) in &emb_store {
            if let Some(session) = store.load(id)? {
                if !session.is_archived(now) && !id.starts_with("consolidated_") {
                    sessions_with_emb.push((session, emb.clone()));
                }
            }
        }

        if sessions_with_emb.len() < 2 {
            return Ok("Fewer than 2 active sessions — nothing to consolidate.".to_string());
        }

        // Union-find clustering by similarity
        let n = sessions_with_emb.len();
        let mut parent: Vec<usize> = (0..n).collect();

        fn find(parent: &mut [usize], i: usize) -> usize {
            if parent[i] != i {
                parent[i] = find(parent, parent[i]);
            }
            parent[i]
        }

        for i in 0..n {
            for j in (i + 1)..n {
                let sim =
                    embed::cosine_similarity(&sessions_with_emb[i].1, &sessions_with_emb[j].1);
                if sim >= similarity_threshold {
                    let pi = find(&mut parent, i);
                    let pj = find(&mut parent, j);
                    if pi != pj {
                        parent[pi] = pj;
                    }
                }
            }
        }

        // Build clusters
        let mut clusters: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        for i in 0..n {
            let root_idx = find(&mut parent, i);
            clusters.entry(root_idx).or_default().push(i);
        }

        let mut consolidated_count = 0;
        let mut out = String::from("## Memory Consolidation\n\n");

        for members in clusters.values() {
            if members.len() < 2 {
                continue;
            }

            let mut merged_summary = String::new();
            let mut merged_decisions = String::new();
            let mut merged_constraints = String::new();
            let mut merged_assumptions = String::new();
            let mut merged_blockers = String::new();
            let mut merged_files: Vec<String> = Vec::new();
            let mut source_ids: Vec<String> = Vec::new();
            let mut earliest_created = i64::MAX;
            let mut latest_updated = 0i64;

            for &idx in members {
                let session = &sessions_with_emb[idx].0;
                source_ids.push(session.id.clone());

                if !session.summary.is_empty() {
                    if !merged_summary.is_empty() {
                        merged_summary.push_str(" | ");
                    }
                    merged_summary.push_str(&session.summary);
                }
                if !session.decisions.is_empty() {
                    if !merged_decisions.is_empty() {
                        merged_decisions.push_str(" | ");
                    }
                    merged_decisions.push_str(&session.decisions);
                }
                if !session.constraints.is_empty() {
                    if !merged_constraints.is_empty() {
                        merged_constraints.push_str(" | ");
                    }
                    merged_constraints.push_str(&session.constraints);
                }
                if !session.assumptions.is_empty() {
                    if !merged_assumptions.is_empty() {
                        merged_assumptions.push_str(" | ");
                    }
                    merged_assumptions.push_str(&session.assumptions);
                }
                if !session.blockers.is_empty() {
                    if !merged_blockers.is_empty() {
                        merged_blockers.push_str(" | ");
                    }
                    merged_blockers.push_str(&session.blockers);
                }
                for f in session.files_touched.split(',').map(|s| s.trim()) {
                    if !f.is_empty() && !merged_files.contains(&f.to_string()) {
                        merged_files.push(f.to_string());
                    }
                }
                earliest_created = earliest_created.min(session.created_at);
                latest_updated = latest_updated.max(session.updated_at);
            }

            let consolidated_id = format!("consolidated_{}", earliest_created);

            let consolidated = SessionData {
                id: consolidated_id.clone(),
                name: format!("Consolidated ({} sessions)", members.len()),
                summary: merged_summary,
                pending_tasks: String::new(),
                decisions: merged_decisions,
                files_touched: merged_files.join(", "),
                constraints: merged_constraints,
                assumptions: merged_assumptions,
                blockers: merged_blockers,
                created_at: earliest_created,
                updated_at: now,
                confidence: 0.9,
                last_accessed: now,
            };

            store.save(&consolidated)?;

            let embed_text = format!(
                "{} {} {} {}",
                consolidated.summary,
                consolidated.decisions,
                consolidated.constraints,
                consolidated.assumptions
            );
            let embedder = embed::code_embedder();
            if let Ok(emb_vec) = embedder.embed(&embed_text) {
                let mut all_embs = embed::load_embeddings(&emb_path)?;
                all_embs.push((consolidated_id.clone(), emb_vec));
                embed::save_embeddings(&emb_path, &all_embs)?;
            }

            for &idx in members {
                let mut session = sessions_with_emb[idx].0.clone();
                session.confidence = (session.compute_confidence(now) * 0.5).max(0.3);
                store.save(&session)?;
            }

            out.push_str(&format!(
                "### {} — merged {} sessions\n",
                consolidated_id,
                members.len()
            ));
            out.push_str(&format!("Sources: {}\n\n", source_ids.join(", ")));
            consolidated_count += 1;
        }

        (consolidated_count, out)
    };
    // `_session_lock` is dropped here — the symbol-cluster lookup below
    // touches unrelated graph state, not sessions.lock resources, so it
    // deliberately runs outside the lock.

    if consolidated_count == 0 {
        out.push_str(
            "No clusters found above similarity threshold. Sessions are already distinct.\n",
        );
    } else {
        out.push_str(&format!(
            "\n**Created {} consolidated session(s).** Source sessions preserved with reduced confidence.\n",
            consolidated_count
        ));
    }

    // Build symbol clusters from co-occurrence data
    match super::memory_context::build_symbol_clusters(path, 3) {
        Ok(clusters) if !clusters.is_empty() => {
            out.push_str(&format!(
                "\n**Symbol clusters:** {} cluster(s) built from co-retrieval patterns.\n",
                clusters.len()
            ));
        }
        Ok(_) => {}
        Err(_) => {}
    }

    Ok(out)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib tools::session:: -- --nocapture`

Expected: all tests in `tools::session` pass, including `concurrent_save_and_purge_do_not_corrupt_embeddings` (Task 3) and `concurrent_save_session_preserves_all_embeddings` (Task 2, must still pass — confirms Task 3's changes didn't reintroduce the Task 2 deadlock risk). Re-run the new test 5 times to confirm it's not flaky, same as prior tasks.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-mcp/src/tools/session.rs
git commit -m "feat: sessions.lock guards tool_consolidate_memory and tool_purge_sessions (R2.3.6)"
```

---

### Task 4: `registry.lock` + atomic write for `Registry::save`

**Note for the human reviewing this plan before dispatch:** this task deliberately does **not** eliminate registry.json lost-update races (see Global Constraints) — it only makes `Registry::save()` itself lock-protected and atomic. The full fix (locking every load-mutate-save call site, including a 575-line `cmd_group` CLI dispatch function) is out of scope here. Flag if you want the fuller fix scoped as a follow-up task instead of accepting this narrower one.

**Files:**
- Modify: `crates/infigraph-core/src/multi/mod.rs:98-114` (`Registry::save`), `:740-746` (`registry_path`, add a sibling `registry_lock_path`)
- Test: `crates/infigraph-core/tests/registry_lock.rs` (new file)

**Interfaces:**
- Consumes: `lockfile::acquire` (same as Tasks 2/3, imported within `infigraph-core` as `crate::lockfile` rather than `infigraph_core::lockfile` since this file is inside the `infigraph-core` crate itself).
- Produces: nothing consumed by later tasks — this is the last task in the plan.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/tests/registry_lock.rs`:

```rust
use infigraph_core::multi::{Registry, RepoEntry};
use std::path::PathBuf;
use std::sync::Mutex;

// Registry::load/save read HOME at call time; HOME is process-global, so
// tests that override it must be serialized against each other (same
// pattern as SLOW_LOCK_ENV in crates/infigraph-core/tests/lockfile.rs).
static HOME_ENV: Mutex<()> = Mutex::new(());

#[test]
fn concurrent_saves_never_produce_unparseable_registry_json() {
    let _guard = HOME_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", dir.path());

    let mut handles = Vec::new();
    for t in 0..4 {
        handles.push(std::thread::spawn(move || {
            for i in 0..15 {
                let mut registry = Registry::load().unwrap_or_default();
                registry.repos.insert(
                    format!("repo-{t}-{i}"),
                    RepoEntry {
                        name: format!("repo-{t}-{i}"),
                        path: PathBuf::from(format!("/tmp/repo-{t}-{i}")),
                        languages: vec!["rust".to_string()],
                        symbol_count: 42,
                        module_count: 3,
                        last_indexed_commit: None,
                    },
                );
                registry.save().unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // Every save() call went through an atomic temp+rename swap, so the
    // final file must always parse — a torn write would fail here.
    let loaded = Registry::load();
    assert!(loaded.is_ok(), "registry.json corrupted after concurrent saves: {loaded:?}");

    std::env::remove_var("HOME");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test registry_lock -- --nocapture`

Expected: may `PASS` or `FAIL` — `std::fs::write` in the current `Registry::save()` is not guaranteed atomic under concurrent writers to the same path (today's implementation truncates-then-writes with no lock), so a `serde_json::from_str` parse failure on `Registry::load()` is possible but not deterministic on every run. Rerun 3-5 times if it passes once, same caveat as prior tasks: `for i in 1 2 3 4 5; do CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test registry_lock -- --nocapture || break; done`.

- [ ] **Step 3: Add `registry_lock_path` and wire the lock + atomic write into `save`**

Add a sibling function next to `registry_path` in `crates/infigraph-core/src/multi/mod.rs` (near line 740-746):

```rust
fn registry_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs_next::home_dir)
        .context("cannot determine home directory")?;
    Ok(home.join(".infigraph").join("registry.json"))
}

fn registry_lock_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs_next::home_dir)
        .context("cannot determine home directory")?;
    Ok(home.join(".infigraph").join("registry.lock"))
}

const REGISTRY_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
```

Replace the body of `Registry::save` at `crates/infigraph-core/src/multi/mod.rs:98-114`:

```rust
    pub fn save(&self) -> Result<()> {
        #[cfg(feature = "postgres")]
        {
            if is_remote_mode() {
                let pg = PostgresMetaStore::connect_from_env_cached()?;
                pg.init_schema()?;
                return pg.save_registry(self);
            }
        }
        let path = registry_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock_path = registry_lock_path()?;
        let _lock = lockfile::acquire(&lock_path, "registry-write", REGISTRY_LOCK_TIMEOUT)?;
        let tmp_path = path.with_file_name("registry.json.tmp");
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp_path, &data)?;
        std::fs::rename(&tmp_path, &path)?;
        Ok(())
    }
```

Add `use crate::lockfile;` and `use std::time::Duration;` to the top of `crates/infigraph-core/src/multi/mod.rs` if not already present (check the existing `use` block first).

Note: the Postgres remote-mode branch returns early via `return pg.save_registry(self);` before ever reaching the local-file path — no lock is needed there, Postgres has its own transactional guarantees, and this task only touches the local-file (default, non-`postgres`-feature) path.

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test registry_lock -- --nocapture`

Expected: `PASS`. Re-run 5 times to confirm it's not flaky:
`for i in 1 2 3 4 5; do CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test registry_lock -- --nocapture || echo "FAILED run $i"; done`

Also run the existing registry-adjacent tests to confirm no regression: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --lib multi:: -- --nocapture`

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/multi/mod.rs crates/infigraph-core/tests/registry_lock.rs
git commit -m "fix: Registry::save acquires registry.lock and writes atomically via temp+rename (R2.3.7/R3.3.1)"
```

---

### Task 5: Full-suite verification

**Files:** none new

- [ ] **Step 1: Run the full `infigraph-core` suite**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --no-fail-fast -- --test-threads=4`

Expected: all green, no new failures relative to the pre-PR4 baseline (the branch's own prior known-good state per `.superpowers/sdd/progress.md`).

- [ ] **Step 2: Run the full `infigraph-mcp` suite**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --no-fail-fast -- --test-threads=4`

Expected: all green except the two already-catalogued pre-existing failures (`tool_parity::advertised_tools_match_mcp_tool_names`, `watcher_concurrency::test_graph_tools_with_group_watchers`) — do not chase those, they predate this branch.

- [ ] **Step 3: Run clippy on the touched crates**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-core -p infigraph-mcp -- -D warnings`

Expected: clean, except the already-catalogued pre-existing warning (`useless_borrows_in_formatting` in `crates/infigraph-core/src/vuln/mod.rs:424`, unrelated to this branch).

- [ ] **Step 4: Commit if Steps 1-3 required any fixes, otherwise stop here**

If Steps 1-3 were clean, there is nothing to commit for this task — proceed directly to the build-and-install step outside this plan.
