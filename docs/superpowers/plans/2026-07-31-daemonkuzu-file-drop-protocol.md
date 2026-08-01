# DaemonKuzu File-Drop Protocol Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and prove the file-drop request/result protocol that lets a process ask a long-lived daemon (the watcher, extended in a later plan) to perform a write on its behalf, without opening its own `Database` handle — tested end-to-end via direct function calls, not yet wired into the real running watcher loop or any production call site.

**Architecture:** A caller writes a small JSON request file (atomically) into a staging directory; a server-side function reads it, executes the requested operation against an already-open `Infigraph`, writes a JSON result file back (atomically), and removes the request file. The caller polls for the result file with a bounded timeout. This plan builds and tests both sides as plain functions — no `GraphBackend` trait implementation, no watcher-loop wiring, no daemon bootstrapping. Those are follow-up plans once this foundational piece is proven.

**Tech Stack:** Rust, `serde`/`serde_json` (already a dependency), `tempfile` for tests, no new external dependencies.

## Global Constraints

- No new dependencies (confirmed during design: real socket IPC was considered and rejected specifically to avoid one — see `docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md`'s Alternatives section).
- Requests carry references, not payloads: file paths, not pre-computed data (e.g. never a serialized `FileExtraction`). The server does its own parsing/extraction using its own local filesystem access.
- Request and result files must be written atomically (temp file + `rename`, same directory) — a reader must never observe a partially-written file.
- Staging directory convention: `.infigraph/requests/`.

---

### Task 1: Request/result JSON types

**Files:**
- Create: `crates/infigraph-core/src/daemon_protocol.rs`
- Modify: `crates/infigraph-core/src/lib.rs` (add `pub mod daemon_protocol;`)

**Interfaces:**
- Produces: `WriteRequest` enum, `WriteResult` enum — used by Task 2 (client) and Task 3 (server).

- [ ] **Step 1: Write the failing test**

```rust
// crates/infigraph-core/src/daemon_protocol.rs
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A request for the daemon to perform a write. Carries references (paths),
/// never pre-computed data -- the daemon does its own parsing/extraction
/// using its own local filesystem access. See
/// docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WriteRequest {
    /// Index specific files. `None` means a full project reindex.
    Index { paths: Option<Vec<PathBuf>> },
    /// Import a SCIP index file at the given path.
    ScipImport { scip_path: PathBuf },
}

/// Small summary of what happened -- never the full `IndexResult` (which
/// carries every file's `FileExtraction`, already written to the graph by
/// the daemon and not needed again by the caller).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WriteResult {
    Ok {
        total_files: usize,
        indexed_files: usize,
    },
    Err {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_request_index_round_trips_through_json() {
        let req = WriteRequest::Index {
            paths: Some(vec![PathBuf::from("src/main.rs")]),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: WriteRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn write_request_full_reindex_round_trips_through_json() {
        let req = WriteRequest::Index { paths: None };
        let json = serde_json::to_string(&req).unwrap();
        let back: WriteRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn write_result_ok_round_trips_through_json() {
        let res = WriteResult::Ok {
            total_files: 10,
            indexed_files: 8,
        };
        let json = serde_json::to_string(&res).unwrap();
        let back: WriteResult = serde_json::from_str(&json).unwrap();
        assert_eq!(res, back);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --lib daemon_protocol`
Expected: FAIL with a compile error — `daemon_protocol` isn't a registered module yet.

- [ ] **Step 3: Register the module**

Add to `crates/infigraph-core/src/lib.rs`, alongside the other `pub mod` declarations near the top of the file:

```rust
pub mod daemon_protocol;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --lib daemon_protocol`
Expected: PASS, all 3 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/src/lib.rs
git commit -m "feat: add WriteRequest/WriteResult daemon protocol types"
```

---

### Task 2: Atomic file write helper

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs`

**Interfaces:**
- Produces: `fn write_atomic(path: &Path, contents: &str) -> Result<()>` — used by both Task 3 (client writes requests) and Task 4 (server writes results).

- [ ] **Step 1: Write the failing test**

Append to `crates/infigraph-core/src/daemon_protocol.rs`:

```rust
#[cfg(test)]
mod atomic_write_tests {
    use super::write_atomic;

    #[test]
    fn write_atomic_creates_file_with_exact_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        write_atomic(&path, r#"{"hello":"world"}"#).unwrap();
        let read_back = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read_back, r#"{"hello":"world"}"#);
    }

    #[test]
    fn write_atomic_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        write_atomic(&path, "content").unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one file, got {entries:?}");
    }

    #[test]
    fn write_atomic_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        write_atomic(&path, "first").unwrap();
        write_atomic(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --lib atomic_write_tests`
Expected: FAIL — `write_atomic` doesn't exist yet.

- [ ] **Step 3: Implement `write_atomic`**

Add to `crates/infigraph-core/src/daemon_protocol.rs`, above the `#[cfg(test)]` sections:

```rust
use std::io::Write;
use std::path::Path;

/// Writes `contents` to `path` atomically: a temp file in the same
/// directory, then `rename(2)` over the target. A reader must never
/// observe a partially-written request or result file. Same pattern as
/// R3.3.1's sidecar-atomicity convention (`DESIGN-hardening.md`).
pub fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent directory: {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let tmp_path = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .ok_or_else(|| anyhow::anyhow!("path has no file name: {}", path.display()))?
            .to_string_lossy(),
        std::process::id()
    ));
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --lib atomic_write_tests`
Expected: PASS, all 3 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon_protocol.rs
git commit -m "feat: add write_atomic helper for daemon request/result files"
```

---

### Task 3: Client-side request submission

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs`

**Interfaces:**
- Consumes: `WriteRequest`/`WriteResult` (Task 1), `write_atomic` (Task 2).
- Produces: `pub fn submit_write_request(staging_dir: &Path, request: &WriteRequest, timeout: Duration) -> Result<WriteResult>` — used by a later plan's `BackendKind::DaemonKuzu`.

- [ ] **Step 1: Write the failing test**

Append to `crates/infigraph-core/src/daemon_protocol.rs`:

```rust
#[cfg(test)]
mod submit_tests {
    use super::{submit_write_request, write_atomic, WriteRequest, WriteResult};
    use std::time::Duration;

    #[test]
    fn submit_write_request_writes_request_file_and_returns_matching_result() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = dir.path().join("requests");
        std::fs::create_dir_all(&staging_dir).unwrap();

        let request = WriteRequest::Index {
            paths: Some(vec!["src/main.rs".into()]),
        };

        // Simulate the server: a background thread watches for the request
        // file to appear, then writes a matching result file.
        let staging_dir_clone = staging_dir.clone();
        let handle = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            loop {
                let entries: Vec<_> = std::fs::read_dir(&staging_dir_clone)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|ext| ext == "request"))
                    .collect();
                if let Some(entry) = entries.first() {
                    let result_path = entry.path().with_extension("result");
                    let result = WriteResult::Ok {
                        total_files: 1,
                        indexed_files: 1,
                    };
                    write_atomic(&result_path, &serde_json::to_string(&result).unwrap())
                        .unwrap();
                    std::fs::remove_file(entry.path()).unwrap();
                    return;
                }
                if start.elapsed() > Duration::from_secs(2) {
                    panic!("test server never saw a request file appear");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let result =
            submit_write_request(&staging_dir, &request, Duration::from_secs(2)).unwrap();
        assert_eq!(
            result,
            WriteResult::Ok {
                total_files: 1,
                indexed_files: 1,
            }
        );

        handle.join().unwrap();
    }

    #[test]
    fn submit_write_request_times_out_cleanly_when_no_result_appears() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = dir.path().join("requests");
        std::fs::create_dir_all(&staging_dir).unwrap();

        let request = WriteRequest::Index { paths: None };
        let result =
            submit_write_request(&staging_dir, &request, Duration::from_millis(200));
        assert!(
            result.is_err(),
            "expected a timeout error when no server ever responds"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --lib submit_tests`
Expected: FAIL — `submit_write_request` doesn't exist yet.

- [ ] **Step 3: Implement `submit_write_request`**

Add to `crates/infigraph-core/src/daemon_protocol.rs`:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Process-local disambiguator: `SystemTime::now()`'s nanosecond field is
/// not guaranteed nanosecond-*resolution* on every platform, so two
/// threads in the same process racing this function could otherwise
/// collide on the same request name -- `write_atomic` overwrites
/// unconditionally (no existence check), so a collision would silently
/// drop one caller's request rather than erroring.
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `request` as a `.request` file into `staging_dir` (unique name:
/// pid + nanosecond timestamp + a process-local counter -- no new
/// dependency, no UUID crate), then polls for the matching `.result` file
/// until `timeout` expires. Bounded-wait-with-backoff, same idiom as
/// `lockfile::acquire`.
pub fn submit_write_request(
    staging_dir: &Path,
    request: &WriteRequest,
    timeout: Duration,
) -> anyhow::Result<WriteResult> {
    std::fs::create_dir_all(staging_dir)?;
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        counter
    );
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

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --lib submit_tests`
Expected: PASS, both tests.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon_protocol.rs
git commit -m "feat: add submit_write_request client-side polling function"
```

---

### Task 4: Server-side request handling

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs` (the `serve_one_request` function itself)
- Create: `crates/infigraph-core/tests/daemon_protocol_serve.rs` (tests — see note below, not a `#[cfg(test)]` unit-test module)

**Interfaces:**
- Consumes: `WriteRequest`/`WriteResult` (Task 1), `write_atomic` (Task 2), `Infigraph::index_files`/`Infigraph::index` (existing, `lib.rs:493`/`:299` on this branch — not upstream/main, where they're at different line numbers; this plan targets whatever's checked out, `feat/hardening`, not a specific pinned branch like the earlier ordering-fix plan did).
- Produces: `pub fn serve_one_request(infigraph: &Infigraph, request_path: &Path) -> Result<()>` — used by a later plan's watcher-loop wiring.

**Discovered during implementation (corrected here, not just in git history):** this task's tests must live in a separate integration test file (`crates/infigraph-core/tests/daemon_protocol_serve.rs`), not a `#[cfg(test)]` unit-test module inside `daemon_protocol.rs` itself. `infigraph-core`'s `Cargo.toml` has a dev-dependency cycle (`infigraph-languages` depends back on `infigraph-core`; see the comment above that dependency line), and `cargo test --lib` compiles a distinct `--cfg test` instance of `infigraph-core` from the normal instance the dev-dependency needs — so `bundled_registry()`'s returned `LanguageRegistry` type is incompatible with `crate::lang::LanguageRegistry` when called from a unit test, but compiles and works correctly from a separate integration test binary (confirmed pre-existing precedent: `crates/infigraph-core/tests/remote_cross_service.rs` already uses `bundled_registry()` successfully). The steps below are written as originally planned (unit-test module) for historical accuracy; follow the corrected file placement above instead — the test code itself is otherwise identical, just needs `infigraph_core::`-qualified imports instead of `crate::` ones.

- [ ] **Step 1: Write the failing test**

Append to `crates/infigraph-core/src/daemon_protocol.rs`:

```rust
#[cfg(test)]
mod serve_tests {
    use super::{serve_one_request, write_atomic, WriteRequest, WriteResult};
    use crate::Infigraph;
    use infigraph_languages::bundled_registry;

    #[test]
    fn serve_one_request_indexes_and_writes_result_and_removes_request() {
        let project_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            project_dir.path().join("main.py"),
            "def hello():\n    pass\n",
        )
        .unwrap();

        let registry = bundled_registry().unwrap();
        let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
        infigraph.init().unwrap();

        let staging_dir = project_dir.path().join(".infigraph").join("requests");
        std::fs::create_dir_all(&staging_dir).unwrap();
        let request_path = staging_dir.join("test-1.request");
        let result_path = staging_dir.join("test-1.result");
        write_atomic(
            &request_path,
            &serde_json::to_string(&WriteRequest::Index { paths: None }).unwrap(),
        )
        .unwrap();

        serve_one_request(&infigraph, &request_path).unwrap();

        assert!(!request_path.exists(), "request file should be removed after serving");
        assert!(result_path.exists(), "result file should have been written");
        let result: WriteResult =
            serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
        match result {
            WriteResult::Ok { indexed_files, .. } => assert_eq!(indexed_files, 1),
            WriteResult::Err { message } => panic!("expected Ok, got Err: {message}"),
        }
    }

    #[test]
    fn serve_one_request_writes_err_result_on_failure_without_panicking() {
        let project_dir = tempfile::tempdir().unwrap();
        let registry = bundled_registry().unwrap();
        let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
        infigraph.init().unwrap();

        let staging_dir = project_dir.path().join(".infigraph").join("requests");
        std::fs::create_dir_all(&staging_dir).unwrap();
        let request_path = staging_dir.join("test-2.request");
        let result_path = staging_dir.join("test-2.result");
        // A path that doesn't exist -- index_files should fail cleanly per-file,
        // not panic; serve_one_request must still produce a result file.
        write_atomic(
            &request_path,
            &serde_json::to_string(&WriteRequest::Index {
                paths: Some(vec!["does/not/exist.py".into()]),
            })
            .unwrap(),
        )
        .unwrap();

        serve_one_request(&infigraph, &request_path).unwrap();
        assert!(result_path.exists());
    }

    #[test]
    fn serve_one_request_writes_err_result_on_corrupt_request_json() {
        let project_dir = tempfile::tempdir().unwrap();
        let registry = bundled_registry().unwrap();
        let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
        infigraph.init().unwrap();

        let staging_dir = project_dir.path().join(".infigraph").join("requests");
        std::fs::create_dir_all(&staging_dir).unwrap();
        let request_path = staging_dir.join("test-3.request");
        let result_path = staging_dir.join("test-3.result");
        // Not valid JSON at all -- a caller must still get a result rather
        // than timing out with no explanation.
        write_atomic(&request_path, "not valid json {{{").unwrap();

        serve_one_request(&infigraph, &request_path).unwrap();

        assert!(result_path.exists(), "corrupt request must still produce a result file");
        let result: WriteResult =
            serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
        assert!(
            matches!(result, WriteResult::Err { .. }),
            "expected Err for a corrupt request, got {result:?}"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --lib serve_tests`
Expected: FAIL — `serve_one_request` doesn't exist yet.

- [ ] **Step 3: Implement `serve_one_request`**

Add to `crates/infigraph-core/src/daemon_protocol.rs`:

```rust
use crate::Infigraph;

/// Reads the request at `request_path`, executes it against `infigraph`
/// (an already-open, write-mode `Infigraph` -- the daemon's own persistent
/// connection), writes the matching `.result` file, then removes the
/// request file. Never panics on a failed operation -- a request that
/// fails to index still produces an `Err` result file, so the caller's
/// `submit_write_request` poll resolves instead of timing out. A request
/// file that fails to even parse (corrupt/truncated write) also produces
/// an `Err` result rather than silently leaving the caller to time out.
///
/// Assumes the single-daemon invariant this whole design is built on: at
/// most one process ever calls this function for a given `request_path`.
/// Under that invariant there's no read-execute-write-remove TOCTOU risk;
/// this function does not defend against a second concurrent caller (e.g.
/// via a claim-by-rename step) since that scenario should never arise in
/// the real architecture -- if it's ever reused somewhere that invariant
/// doesn't hold, add that defense first.
pub fn serve_one_request(infigraph: &Infigraph, request_path: &Path) -> anyhow::Result<()> {
    let result_path = request_path.with_extension("result");

    let result = match std::fs::read_to_string(request_path)
        .map_err(anyhow::Error::from)
        .and_then(|contents| Ok(serde_json::from_str::<WriteRequest>(&contents)?))
    {
        Ok(request) => match &request {
            WriteRequest::Index { paths: None } => match infigraph.index() {
                Ok(r) => WriteResult::Ok {
                    total_files: r.total_files,
                    indexed_files: r.indexed_files,
                },
                Err(e) => WriteResult::Err {
                    message: e.to_string(),
                },
            },
            WriteRequest::Index { paths: Some(paths) } => {
                match infigraph.index_files(paths) {
                    Ok(r) => WriteResult::Ok {
                        total_files: r.total_files,
                        indexed_files: r.indexed_files,
                    },
                    Err(e) => WriteResult::Err {
                        message: e.to_string(),
                    },
                }
            }
            WriteRequest::ScipImport { scip_path: _ } => WriteResult::Err {
                message: "scip-import serving not yet implemented".to_string(),
            },
        },
        Err(e) => WriteResult::Err {
            message: format!("failed to read/parse request: {e}"),
        },
    };

    write_atomic(&result_path, &serde_json::to_string(&result)?)?;
    // Tolerate the request file already being gone: a caller that timed
    // out (submit_write_request) removes its own request file, and this
    // function may race that removal if it finishes serving right around
    // the caller's timeout. The result file above is still written either
    // way -- an accepted, documented gap (unconsumed result files
    // accumulating in the staging directory) that the watcher-wiring plan
    // needs to address with a cleanup/TTL pass, not silently ignored here.
    std::fs::remove_file(request_path).ok();
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --lib serve_tests`
Expected: PASS, both tests.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon_protocol.rs
git commit -m "feat: add serve_one_request server-side request handler"
```

---

### Task 5: End-to-end round trip (client submits, server serves, client receives)

**Files:**
- Create: `crates/infigraph-core/tests/daemon_protocol_e2e.rs`

**Interfaces:**
- Consumes: `submit_write_request` (Task 3), `serve_one_request` (Task 4).

- [ ] **Step 1: Write the test**

```rust
// crates/infigraph-core/tests/daemon_protocol_e2e.rs
//
// Full round trip: a "client" thread submits a request via
// submit_write_request while a "server" thread polls the staging
// directory and calls serve_one_request -- proving the two halves built
// in this plan actually interoperate, not just pass their own unit tests
// against a hand-rolled stand-in for the other side.

use infigraph_core::daemon_protocol::{submit_write_request, WriteRequest};
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;
use std::time::Duration;

#[test]
fn client_and_server_interoperate_end_to_end() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    let registry = bundled_registry().unwrap();
    let mut infigraph = Infigraph::open(project_dir.path(), registry).unwrap();
    infigraph.init().unwrap();

    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();

    // Server thread: polls the staging dir for new .request files and
    // serves the first one it sees, using serve_one_request directly
    // (not the real watcher loop -- that's a later plan).
    let staging_dir_clone = staging_dir.clone();
    let server_handle = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        loop {
            let entries: Vec<_> = std::fs::read_dir(&staging_dir_clone)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "request"))
                .collect();
            if let Some(entry) = entries.first() {
                infigraph_core::daemon_protocol::serve_one_request(&infigraph, &entry.path())
                    .unwrap();
                return;
            }
            if start.elapsed() > Duration::from_secs(5) {
                panic!("server thread never saw a request file appear");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    let result = submit_write_request(
        &staging_dir,
        &WriteRequest::Index { paths: None },
        Duration::from_secs(5),
    )
    .unwrap();

    match result {
        infigraph_core::daemon_protocol::WriteResult::Ok { indexed_files, .. } => {
            assert_eq!(indexed_files, 1)
        }
        infigraph_core::daemon_protocol::WriteResult::Err { message } => {
            panic!("expected Ok, got Err: {message}")
        }
    }

    server_handle.join().unwrap();
}
```

- [ ] **Step 2: Run test**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test -p infigraph-core --test daemon_protocol_e2e`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/infigraph-core/tests/daemon_protocol_e2e.rs
git commit -m "test: end-to-end round trip for the daemon file-drop protocol"
```

---

### Task 6: Full workspace test pass

**Files:** none (verification task)

- [ ] **Step 1: Run the full workspace test suite**

Run: `CARGO_TARGET_DIR="$(pwd)/target" cargo test --workspace`
Expected: PASS. If `groups_watch_perf` or similar fails, verify it's the pre-existing `INFIGRAPH_WATCH_DAEMON` env-leak issue documented earlier in this project's history before treating it as a regression from this plan.

- [ ] **Step 2: Run `cargo fmt` and `cargo clippy`**

Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings`
Expected: clean, no warnings.

- [ ] **Step 3: Commit any final fixes, then stop — this plan's scope ends here**

This plan deliberately stops short of wiring `serve_one_request` into the real watcher loop, implementing `BackendKind::DaemonKuzu` as a full `GraphBackend`, or migrating any production call site — those are separate follow-up plans (per `docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md`'s Open Questions), building on this now-proven protocol.
