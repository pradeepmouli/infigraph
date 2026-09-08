# Daemon-Routed Reads Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the daemon the only process that opens `graph` and `docs.kuzu`, by moving reads onto a socket-based read service inside it.

**Architecture:** `GraphQuery` — which holds every read query and its row-parsing — is made generic over a `QueryExec` trait instead of borrowing a `kuzu::Connection` directly. `LocalExec` wraps a real connection (today's behaviour, unchanged). `RemoteExec` sends the same Cypher to the daemon over a local socket and decodes chunked Arrow back into the same row shape. Because the seam is the *connection* and not the backend, all 1045 lines of Cypher stay in one place and serve both paths. The daemon gains a read service — listener, worker pool, connection pool — that is entirely separate from the write coordinator: no watch-loop coupling, no `index.lock`, no queue.

**Tech Stack:** Rust; `lbug` 0.20.2 (`PreparedStatement::is_read_only`, `Connection::query_as_arrow`); `arrow` 58.3 and `memmap2` 0.9 (already dependencies); `interprocess` (new, for portable local sockets).

**Spec:** `docs/superpowers/specs/2026-09-08-daemon-routed-reads-design.md`

## Global Constraints

- **The write pipeline is not modified.** `route_or_serve_request` (`crates/infigraph-core/src/daemon/mod.rs:2111-2365`), the index work queue, `begin_index_op`, `serve_one_request` (`crates/infigraph-core/src/daemon_protocol.rs:660`) and the file-drop transport are out of scope. If a task appears to require changing them, stop and raise it.
- **Read-only is enforced by the database, never by inspecting query text.** Use `PreparedStatement::is_read_only` (`lbug-0.20.2/src/connection.rs:56`). Classifying Cypher by string was explicitly rejected in the 2026-08-01 design and again in this one.
- **The endpoint identifier is an opaque type, never a `PathBuf`.** macOS caps `sun_path` at ~104 bytes; a socket under `.infigraph/` for a nested worktree already measures 94. Windows named pipes are not filesystem paths.
- **A truncated result stream is an error, never an empty result set.** Silently returning zero rows is the "0-symbol graph served as healthy" failure `Infigraph::init` already shipped once.
- **Run tests with `--test-threads=1`** when a failure appears. Parallel runs on this repo produce mmap/buffer-manager exhaustion ("Mmap for size 8796093022208 failed") unrelated to any change. Confirm single-threaded before treating a failure as real.
- **Do not run `cargo test -p infigraph-mcp` in the background** while a user's MCP servers are live: those tests take `mcp.lock` with handover and kill in-use servers.
- Pre-commit runs fmt, clippy `-D warnings`, and three `--ignored` perf tests across the whole workspace. `groups_watch_perf` needs exclusive watcher ownership — no other `cargo test` may be running.

---

### Task 1: `QueryExec` trait and `LocalExec`

Pure refactor. No behaviour change; the deliverable is that every existing test still passes with `GraphQuery` no longer holding a `kuzu::Connection` directly.

**Files:**
- Create: `crates/infigraph-core/src/graph/query_exec.rs`
- Modify: `crates/infigraph-core/src/graph/queries.rs:1-16` (imports and the `GraphQuery` struct/constructor)
- Modify: `crates/infigraph-core/src/graph/mod.rs` (add `pub mod query_exec;`)

**Interfaces (AS BUILT — commit `e162b0b`; this task is COMPLETE):**
- Produces:
  - `QueryExec` trait: `fn query_rows(&self, cypher: &str) -> anyhow::Result<Vec<Vec<String>>>`
  - `LocalExec<'a,'db>::new(&'a kuzu::Connection<'db>)`, implementing it
  - blanket `impl<T: QueryExec + ?Sized> QueryExec for &T`, so a borrowed executor
    can be passed where an owned one is expected
  - `GraphQuery<E: QueryExec>` — **generic over an OWNED executor**, with
    `GraphQuery::new(&conn)` retained for the local case and
    `GraphQuery::new_with(exec)` taking `E` **by value**
  - free function `queries::derive_tested_by_edges(conn: &Connection) -> Result<usize>`
- Consumes: nothing.

**Two deviations from this plan's original text, already applied.** Later tasks
must use the as-built signatures above.

1. The plan specified a borrowed `&'a dyn QueryExec`, which forces
   `let exec = LocalExec::new(&conn);` at every construction site. The true site
   count is **94**, not the ~29 assumed here — 65 in `tests/graph_queries.rs`
   alone. With an owned generic and `new` retained, every existing site compiled
   unchanged: of 8 files staged, 3 came out byte-identical.
2. `GraphQuery` was not purely reads. `derive_tested_by_edges` issued `DELETE`
   and `CREATE`, and routing it through a read-only executor would make the
   remote path refuse a legitimate local write. It was the only writer among 21
   methods and is now a free function taking a connection.
   `GraphStore::derive_tested_by_edges` takes the write lock before calling it,
   and the indexer reaches it post-indexing via
   `WriteRequest::DeriveTestedBy` — squarely write-path, so the read service
   never sees it.

Row shape is `Vec<Vec<String>>` because that is already how `GraphQuery` consumes results — e.g. `queries.rs:32-38` does `row[0].to_string()` and `row[3].to_string().parse().unwrap_or(0)`. `GraphBackend::raw_query` already returns `Result<Vec<Vec<String>>>`, so this matches the established shape rather than inventing one.

- [x] **Step 1: Write the failing test**

Create `crates/infigraph-core/src/graph/query_exec.rs` with only the test module at first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A `GraphQuery` built over `LocalExec` must return exactly what it
    /// returned when it borrowed the connection directly.
    #[test]
    fn local_exec_returns_the_same_rows_as_a_direct_connection() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph");
        let store = crate::graph::GraphStore::open(&graph).unwrap();
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();

        let exec = LocalExec::new(&conn);
        let rows = exec.query_rows("MATCH (f:File) RETURN f.id").unwrap();
        assert_eq!(rows, vec![vec!["a.rs".to_string()]]);
    }
}
```

- [x] **Step 2: Run it to verify it fails**

Run: `cargo test -p infigraph-core --lib graph::query_exec -- --test-threads=1`
Expected: FAIL to compile — `LocalExec` not defined.

- [x] **Step 3: Write the trait and the local implementation**

Above the test module in the same file:

```rust
//! The seam that lets `GraphQuery` run against either a local Kuzu
//! connection or the daemon's read service.
//!
//! `GraphQuery` holds every read query in the codebase along with its
//! row-parsing (1045 lines). Abstracting at the *connection* rather than at
//! the backend keeps all of that in one place and serving both paths --
//! re-implementing those queries for a remote backend would duplicate every
//! one of them.

use anyhow::Result;

/// Executes a read query and returns its rows as strings.
///
/// Stringly rows are not a simplification: `GraphQuery` already consumes
/// results this way (`row[0].to_string()`, `row[3].to_string().parse()`),
/// and `GraphBackend::raw_query` already returns `Vec<Vec<String>>`.
pub trait QueryExec {
    fn query_rows(&self, cypher: &str) -> Result<Vec<Vec<String>>>;
}

/// Runs queries on a Kuzu connection in this process.
pub struct LocalExec<'a, 'db> {
    conn: &'a kuzu::Connection<'db>,
}

impl<'a, 'db> LocalExec<'a, 'db> {
    pub fn new(conn: &'a kuzu::Connection<'db>) -> Self {
        Self { conn }
    }
}

impl QueryExec for LocalExec<'_, '_> {
    fn query_rows(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        let result = self
            .conn
            .query(cypher)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        Ok(result
            .map(|row| row.iter().map(|v| v.to_string()).collect())
            .collect())
    }
}
```

Add to `crates/infigraph-core/src/graph/mod.rs`:

```rust
pub mod query_exec;
```

- [x] **Step 4: Run it to verify it passes**

Run: `cargo test -p infigraph-core --lib graph::query_exec -- --test-threads=1`
Expected: PASS, `1 passed`.

- [x] **Step 5: Make `GraphQuery` generic over an owned executor**

In `queries.rs`, replace the struct and constructor:

```rust
pub struct GraphQuery<E: crate::graph::query_exec::QueryExec> {
    exec: E,
}

impl<'a, 'db> GraphQuery<crate::graph::query_exec::LocalExec<'a, 'db>> {
    /// Build over a local Kuzu connection. Unchanged signature: every
    /// existing call site keeps working, which is why the executor is owned
    /// and generic rather than a borrowed trait object.
    pub fn new(conn: &'a Connection<'db>) -> Self {
        Self {
            exec: crate::graph::query_exec::LocalExec::new(conn),
        }
    }
}

impl<E: crate::graph::query_exec::QueryExec> GraphQuery<E> {
    /// Build over any executor -- notably the daemon's read service.
    pub fn new_with(exec: E) -> Self {
        Self { exec }
    }
```

Then replace every connection use in this file with `self.exec.query_rows(...)`.
There are **16**, and they matter: rustfmt splits them across lines, so a
single-line search for `self.conn` reports 4. Search for `.conn` alone.
`query_rows` also returns a `Result`, so existing `.map_err(...)?` tails stay
valid; the only downstream changes are that rows are now `Vec<String>` (so
`.next()` on a result becomes `.into_iter().next()`, and `row[0].to_string()`
may become `row[0].clone()`).

Do not change any Cypher string or any parsing logic — only how rows are obtained.

- [x] **Step 6: Point `GraphQuery::raw_query` at the executor**

`GraphQuery` already has a `raw_query` method, and `KuzuBackend::raw_query`
delegates to it. Make it the one place that reaches the executor:

```rust
    /// Execute arbitrary read Cypher and return stringly rows.
    ///
    /// This is the primitive every other method in this file is built on
    /// once `GraphQuery` runs on a `QueryExec`.
    pub fn raw_query(&self, query: &str) -> Result<Vec<Vec<String>>> {
        self.exec.query_rows(query)
    }
```

Note the direction of dependency, because inverting it deadlocks the design:
`LocalExec::query_rows` must do the `conn.query()` + stringify itself and must
NOT delegate to `GraphQuery::raw_query`. `GraphQuery` is generic over
`QueryExec`, so an executor calling back into it would recurse forever.
`QueryExec` is the primitive; `GraphQuery::raw_query` is the thin wrapper.

- [x] **Step 7: Update `GraphQuery::new` call sites**

`KuzuBackend` (`crates/infigraph-core/src/graph/kuzu_backend.rs`) creates a connection per read method (`let conn = self.store.connection()?;` at lines 83, 89, 95, 101, 109, 115, 121, 127, 133, 139, 147, 153, 159, and onwards). At each, wrap it:

```rust
let conn = self.store.connection()?;
let exec = crate::graph::query_exec::LocalExec::new(&conn);
let q = GraphQuery::new_with(&exec);
```

- [x] **Step 8: Run the whole core suite**

Run: `cargo test -p infigraph-core --lib -- --test-threads=1`
Expected: PASS, same count as before the task (565 at the time of writing) plus the one new test.

This is the task's real gate: a pure refactor that changes no behaviour must not change any test outcome.

- [x] **Step 9: Commit**

```bash
git add crates/infigraph-core/src/graph/query_exec.rs \
        crates/infigraph-core/src/graph/queries.rs \
        crates/infigraph-core/src/graph/kuzu_backend.rs \
        crates/infigraph-core/src/graph/mod.rs
git commit -m "refactor(core): GraphQuery runs on a QueryExec, not a connection"
```

---

### Task 2: Endpoint naming

**Files:**
- Create: `crates/infigraph-core/src/daemon/read_endpoint.rs`
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (add `pub mod read_endpoint;`)

**Interfaces:**
- Produces: `ReadEndpoint` (opaque), `ReadEndpoint::for_root(root: &Path) -> ReadEndpoint`, `ReadEndpoint::as_name(&self) -> String`.
- Consumes: nothing.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The endpoint name must not grow with the project path. macOS caps
    /// `sun_path` at ~104 bytes, and a socket under `.infigraph/` for a
    /// nested worktree already measures 94 -- deep paths are routine here
    /// (`scratchpad/wt-*`), and overflowing produces an obscure bind error.
    #[test]
    fn endpoint_name_is_bounded_regardless_of_project_path_depth() {
        let deep = std::path::PathBuf::from("/Users/someone/GitHub.nosync/active/rust/infigraph")
            .join("scratchpad/wt-a-very-long-worktree-name/nested/deeper/deeper-still");
        let name = ReadEndpoint::for_root(&deep).as_name();
        assert!(
            name.len() <= 80,
            "endpoint name must stay well under the ~104-byte sun_path cap, got {} bytes: {name}",
            name.len()
        );
    }

    /// Two different roots must not collide onto one endpoint.
    #[test]
    fn distinct_roots_get_distinct_endpoints() {
        let a = ReadEndpoint::for_root(std::path::Path::new("/tmp/alpha")).as_name();
        let b = ReadEndpoint::for_root(std::path::Path::new("/tmp/beta")).as_name();
        assert_ne!(a, b);
    }

    /// The same root must resolve to the same endpoint across processes.
    #[test]
    fn the_same_root_is_stable() {
        let p = std::path::Path::new("/tmp/alpha");
        assert_eq!(ReadEndpoint::for_root(p).as_name(), ReadEndpoint::for_root(p).as_name());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-core --lib daemon::read_endpoint -- --test-threads=1`
Expected: FAIL to compile — `ReadEndpoint` not defined.

- [ ] **Step 3: Implement**

```rust
//! Naming for the daemon's read-service endpoint.
//!
//! Deliberately NOT a path under `.infigraph/`. macOS caps a Unix socket's
//! `sun_path` at about 104 bytes, and this project's own worktree
//! convention (`scratchpad/wt-*`) already produces 94-byte candidates --
//! the failure mode is an obscure bind error, not a clear one. Windows
//! named pipes are not filesystem paths at all, so the identifier is opaque
//! from the outset.

use std::hash::{Hash, Hasher};
use std::path::Path;

/// An opaque local-socket identity for one project's read service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadEndpoint {
    name: String,
}

impl ReadEndpoint {
    /// Derive the endpoint for a project root. The root is canonicalised
    /// when possible so that `.` and a symlinked path reach the same daemon.
    pub fn for_root(root: &Path) -> Self {
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        canonical.to_string_lossy().hash(&mut hasher);
        Self {
            name: format!("infigraph-read-{:016x}", hasher.finish()),
        }
    }

    /// The transport-level name. Fixed length regardless of project depth.
    pub fn as_name(&self) -> String {
        self.name.clone()
    }
}
```

Add `pub mod read_endpoint;` to `crates/infigraph-core/src/daemon/mod.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p infigraph-core --lib daemon::read_endpoint -- --test-threads=1`
Expected: PASS, `3 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon/read_endpoint.rs crates/infigraph-core/src/daemon/mod.rs
git commit -m "feat(core): bounded, opaque endpoint naming for the read service"
```

---

### Task 3: Pin `interprocess` and prove the transport API

The plan does not assume this crate's exact API surface. Establish it with a smoke test that compiles and runs before anything depends on it.

**Files:**
- Modify: `crates/infigraph-core/Cargo.toml`
- Create: `crates/infigraph-core/tests/read_socket_smoke.rs`

**Interfaces:**
- Produces: a verified minimal bind/connect/echo pattern that Tasks 5 and 7 copy.
- Consumes: `ReadEndpoint` from Task 2.

- [ ] **Step 1: Add the dependency**

In `crates/infigraph-core/Cargo.toml` under `[dependencies]`, add `interprocess` at the current 2.x release. Pin the exact version you resolve (`cargo add interprocess` then record it here in this step before continuing).

- [ ] **Step 2: Write the smoke test**

```rust
//! Establishes the local-socket API this crate will use, on the platform
//! actually running the tests. Everything in the read service copies this
//! shape, so it is proven once here rather than assumed in five places.

use std::io::{BufRead, BufReader, Write};

#[test]
fn a_local_socket_round_trips_one_line() {
    let name = infigraph_core::daemon::read_endpoint::ReadEndpoint::for_root(
        std::path::Path::new("/tmp/interprocess-smoke"),
    )
    .as_name();

    // Server: accept one connection, echo one line back.
    let listener = bind_listener(&name).expect("bind");
    let server = std::thread::spawn(move || {
        let stream = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let mut stream = reader.into_inner();
        write!(stream, "echo:{line}").expect("write");
    });

    let mut client = connect(&name).expect("connect");
    writeln!(client, "hello").expect("write");
    let mut reader = BufReader::new(client);
    let mut got = String::new();
    reader.read_line(&mut got).expect("read");
    assert_eq!(got.trim_end(), "echo:hello");
    server.join().unwrap();
}
```

Write `bind_listener` and `connect` as two small helpers in this test file using `interprocess`'s local-socket API for the version you pinned. They are the only two places the crate's API shape appears.

- [ ] **Step 3: Run it**

Run: `cargo test -p infigraph-core --test read_socket_smoke -- --test-threads=1`
Expected: PASS. If the API differs from your first attempt, fix the two helpers until it passes — that is the point of this task.

- [ ] **Step 4: Promote the helpers**

Move `bind_listener` and `connect` into `crates/infigraph-core/src/daemon/read_endpoint.rs` as `ReadEndpoint::bind(&self)` and `ReadEndpoint::connect(&self)`, returning the crate's listener and stream types. Update the smoke test to call them.

- [ ] **Step 5: Re-run and commit**

Run: `cargo test -p infigraph-core --test read_socket_smoke -- --test-threads=1`
Expected: PASS.

```bash
git add crates/infigraph-core/Cargo.toml Cargo.lock \
        crates/infigraph-core/src/daemon/read_endpoint.rs \
        crates/infigraph-core/tests/read_socket_smoke.rs
git commit -m "feat(core): pin interprocess and prove the local-socket round trip"
```

---

### Task 4: Read-only enforcement

**Files:**
- Create: `crates/infigraph-core/src/daemon/read_guard.rs`
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (add `pub mod read_guard;`)

**Interfaces:**
- Produces: `fn ensure_read_only(conn: &kuzu::Connection, cypher: &str) -> anyhow::Result<kuzu::PreparedStatement>`.
- Consumes: nothing.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon holds a READ-WRITE database, so the read service must not
    /// become a write backdoor. The refusal must come from the database's
    /// own parser -- classifying Cypher by string was rejected in the
    /// 2026-08-01 design as fragile in both directions.
    #[test]
    fn a_write_statement_is_refused_by_the_database_not_by_string_matching() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::graph::GraphStore::open(&dir.path().join("graph")).unwrap();
        let conn = store.connection().unwrap();

        let err = ensure_read_only(
            &conn,
            "CREATE (:File {id: 'x', name: 'x', path: 'x', language: 'rust', symbol_count: 0})",
        )
        .expect_err("a CREATE must be refused");
        assert!(
            err.to_string().contains("not a read"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_read_statement_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::graph::GraphStore::open(&dir.path().join("graph")).unwrap();
        let conn = store.connection().unwrap();
        assert!(ensure_read_only(&conn, "MATCH (f:File) RETURN f.id").is_ok());
    }

    /// A MATCH whose *text* contains a write keyword must still be accepted.
    /// This is the false-positive half of why string classification was
    /// rejected.
    #[test]
    fn a_read_whose_text_contains_a_write_keyword_is_still_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::graph::GraphStore::open(&dir.path().join("graph")).unwrap();
        let conn = store.connection().unwrap();
        assert!(ensure_read_only(
            &conn,
            "MATCH (s:Symbol) WHERE s.name = 'CREATE' RETURN s.id"
        )
        .is_ok());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-core --lib daemon::read_guard -- --test-threads=1`
Expected: FAIL to compile — `ensure_read_only` not defined.

- [ ] **Step 3: Implement**

```rust
//! Read-only enforcement for the daemon's read service.
//!
//! The daemon holds a read-write `Database`, so nothing here may allow a
//! write through. The verdict comes from the database's own parser via
//! `PreparedStatement::is_read_only` (lbug-0.20.2 connection.rs:56), not
//! from inspecting the query text -- the 2026-08-01 DaemonKuzu design
//! rejected string classification as fragile in both directions: a false
//! negative silently reintroduces a direct write, a false positive breaks a
//! legitimate `MATCH` whose text happens to contain a keyword.

use anyhow::Result;

/// Prepare `cypher` and return the statement only if the database judges it
/// read-only.
pub fn ensure_read_only(
    conn: &kuzu::Connection,
    cypher: &str,
) -> Result<kuzu::PreparedStatement> {
    let stmt = conn
        .prepare(cypher)
        .map_err(|e| anyhow::anyhow!("failed to prepare query: {e}"))?;
    if !stmt.is_read_only() {
        anyhow::bail!(
            "refused: this is not a read query, and the read service will not execute writes"
        );
    }
    Ok(stmt)
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p infigraph-core --lib daemon::read_guard -- --test-threads=1`
Expected: PASS, `3 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon/read_guard.rs crates/infigraph-core/src/daemon/mod.rs
git commit -m "feat(core): DB-enforced read-only gate for the read service"
```

---

### Task 5: Wire protocol

**Files:**
- Create: `crates/infigraph-core/src/daemon/read_protocol.rs`
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (add `pub mod read_protocol;`)

**Interfaces:**
- Produces: `Store` (`Graph` | `Docs`), `ReadRequest { store, query, params, chunk_size }`, `ReadFrame` (`Rows(Vec<Vec<String>>)` | `End` | `Error(String)`), `write_frame`, `read_frame`, `write_request`, `read_request`.
- Consumes: nothing.

Framing is length-prefixed JSON. Arrow is the *result* encoding inside `Rows` in a later optimisation; the frame boundary is what makes truncation detectable, which is this task's real purpose.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips() {
        let req = ReadRequest {
            store: Store::Graph,
            query: "MATCH (f:File) RETURN f.id".to_string(),
            params: vec![],
            chunk_size: 1024,
        };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let got = read_request(&mut buf.as_slice()).unwrap();
        assert_eq!(got.query, req.query);
        assert_eq!(got.store, Store::Graph);
    }

    /// A stream that ends without an `End` frame must be an error, never an
    /// empty result. Returning zero rows silently is the "0-symbol graph
    /// served as healthy" failure `Infigraph::init` already shipped once.
    #[test]
    fn a_truncated_stream_is_an_error_not_an_empty_result() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["a".to_string()]])).unwrap();
        // Deliberately no End frame, and cut mid-frame.
        buf.truncate(buf.len() - 2);

        let err = collect_rows(&mut buf.as_slice())
            .expect_err("a truncated stream must not read as a complete empty result");
        assert!(err.to_string().contains("truncated"), "unexpected: {err}");
    }

    #[test]
    fn a_complete_stream_collects_all_rows() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["a".to_string()]])).unwrap();
        write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["b".to_string()]])).unwrap();
        write_frame(&mut buf, &ReadFrame::End).unwrap();
        let rows = collect_rows(&mut buf.as_slice()).unwrap();
        assert_eq!(rows, vec![vec!["a".to_string()], vec!["b".to_string()]]);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-core --lib daemon::read_protocol -- --test-threads=1`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
//! Framing for the daemon read service.
//!
//! Length-prefixed JSON frames. The explicit `End` frame is the point: a
//! stream that stops without one is a truncation, and the client must be
//! able to tell that from a query that legitimately returned no rows.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Store {
    Graph,
    Docs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadRequest {
    pub store: Store,
    pub query: String,
    pub params: Vec<(String, serde_json::Value)>,
    pub chunk_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReadFrame {
    Rows(Vec<Vec<String>>),
    End,
    Error(String),
}

fn write_len_prefixed<W: Write>(w: &mut W, bytes: &[u8]) -> Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

fn read_len_prefixed<R: Read>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .map_err(|_| anyhow::anyhow!("truncated frame: expected {len} bytes"))?;
    Ok(Some(body))
}

pub fn write_request<W: Write>(w: &mut W, req: &ReadRequest) -> Result<()> {
    write_len_prefixed(w, &serde_json::to_vec(req)?)
}

pub fn read_request<R: Read>(r: &mut R) -> Result<ReadRequest> {
    let body = read_len_prefixed(r)?.ok_or_else(|| anyhow::anyhow!("no request"))?;
    Ok(serde_json::from_slice(&body)?)
}

pub fn write_frame<W: Write>(w: &mut W, frame: &ReadFrame) -> Result<()> {
    write_len_prefixed(w, &serde_json::to_vec(frame)?)
}

pub fn read_frame<R: Read>(r: &mut R) -> Result<Option<ReadFrame>> {
    match read_len_prefixed(r)? {
        None => Ok(None),
        Some(body) => Ok(Some(serde_json::from_slice(&body)?)),
    }
}

/// Read frames until `End`. A stream that ends first is an error.
pub fn collect_rows<R: Read>(r: &mut R) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    loop {
        match read_frame(r)? {
            Some(ReadFrame::Rows(mut chunk)) => rows.append(&mut chunk),
            Some(ReadFrame::End) => return Ok(rows),
            Some(ReadFrame::Error(msg)) => anyhow::bail!("read service error: {msg}"),
            None => anyhow::bail!(
                "truncated result stream: the daemon closed the connection before sending \
                 an end-of-results frame. This is not an empty result set."
            ),
        }
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p infigraph-core --lib daemon::read_protocol -- --test-threads=1`
Expected: PASS, `3 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon/read_protocol.rs crates/infigraph-core/src/daemon/mod.rs
git commit -m "feat(core): framed read protocol where truncation is not an empty result"
```

---

### Task 6: Read service

**Files:**
- Create: `crates/infigraph-core/src/daemon/read_service.rs`
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (add `pub mod read_service;`)
- Test: `crates/infigraph-core/tests/read_service.rs`

**Interfaces:**
- Produces: `ReadService::start(root: &Path, store: Arc<GraphStore>, workers: usize) -> Result<ReadService>`, `ReadService::shutdown(self)`.
- Consumes: `ReadEndpoint` (Task 2), `ensure_read_only` (Task 4), `read_protocol` (Task 5), the daemon's existing `GraphStore` (Task 10).

The service owns nothing the write coordinator owns: no `index.lock`, no queue, no watch-loop coupling.

**It takes `Arc<GraphStore>`, never a bare `kuzu::Database`, and this is load-bearing.**
`GraphStore` is not a thin wrapper: opening through it carries `validate_db_file`'s
truncation preflight (the guard against Kuzu parsing a bogus size field and aborting
the process), `refuse_newer_schema`, the bounded write buffer pool from 13b4065, and
the `write_phase::enter` breadcrumbs. A read service holding its own `Database`
re-derives none of that — and, worse, a second handle on the same file cannot see the
writer's uncommitted WAL even inside one process. That is #149 reproduced inside the
daemon, and it would pass every test that starts a service and queries it, because
those tests have no concurrent writer. There must be exactly one `Database` in the
process, reached through the daemon's existing store.

- [ ] **Step 1: Write the failing test**

`crates/infigraph-core/tests/read_service.rs`:

```rust
use std::sync::Arc;

/// End-to-end: a client gets rows back over the socket.
#[test]
fn a_client_reads_rows_over_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    {
        let store = infigraph_core::graph::GraphStore::open(&graph).unwrap();
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();
    }
    let db = Arc::new(open_shared_database(&graph));
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, db, 4).unwrap();

    let rows = client_query(root, "MATCH (f:File) RETURN f.id").unwrap();
    assert_eq!(rows, vec![vec!["a.rs".to_string()]]);

    svc.shutdown();
}

/// A write sent to the read service is refused, and the refusal comes from
/// the database, not from a keyword check.
#[test]
fn a_write_sent_to_the_read_service_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    drop(infigraph_core::graph::GraphStore::open(&graph).unwrap());
    let db = Arc::new(open_shared_database(&graph));
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, db, 2).unwrap();

    let err = client_query(
        root,
        "CREATE (:File {id: 'x', name: 'x', path: 'x', language: 'rust', symbol_count: 0})",
    )
    .expect_err("a write must be refused");
    assert!(err.to_string().contains("not a read"), "unexpected: {err}");

    svc.shutdown();
}
```

Write `open_shared_database` (opens the graph read-write with the bounded write buffer pool, matching `GraphStore::open`) and `client_query` (connects via `ReadEndpoint::connect`, writes a `ReadRequest`, calls `collect_rows`) as helpers at the bottom of this test file.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-core --test read_service -- --test-threads=1`
Expected: FAIL to compile — `ReadService` not defined.

- [ ] **Step 3: Implement the service**

```rust
//! The daemon's read service.
//!
//! Structurally parallel to the write coordinator and sharing nothing with
//! it but the process and the `Database` handle. The write pipeline is a
//! coalescing pipeline -- queue, dedupe, serialize, lock -- because writes
//! want fan-in. Reads want fan-out, so this takes no lock, joins no queue,
//! and never runs on the watch loop.

use anyhow::Result;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::read_endpoint::ReadEndpoint;
use super::read_protocol::{read_request, write_frame, ReadFrame};

pub struct ReadService {
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
    endpoint: ReadEndpoint,
}

impl ReadService {
    pub fn start(root: &Path, store: Arc<crate::graph::GraphStore>, workers: usize) -> Result<Self> {
        let endpoint = ReadEndpoint::for_root(root);
        let listener = endpoint.bind()?;
        let stop = Arc::new(AtomicBool::new(false));

        let pool = threadpool_of(workers);
        let accept_stop = stop.clone();
        let accept = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if accept_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let store = store.clone();
                pool.execute(move || {
                    if let Err(e) = serve_one(&store, stream) {
                        eprintln!("[read] connection failed: {e:#}");
                    }
                });
            }
        });

        Ok(Self {
            stop,
            accept: Some(accept),
            endpoint,
        })
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock `accept` by connecting to ourselves once.
        let _ = self.endpoint.connect();
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

fn serve_one<S: std::io::Read + std::io::Write>(
    store: &crate::graph::GraphStore,
    mut stream: S,
) -> Result<()> {
    let req = read_request(&mut stream)?;
    // Same call the write path uses (store.rs:976) -- one Database, one
    // buffer pool, one WAL.
    let conn = store.connection()?;

    match super::read_guard::ensure_read_only(&conn, &req.query) {
        Err(e) => {
            write_frame(&mut stream, &ReadFrame::Error(e.to_string()))?;
            return Ok(());
        }
        Ok(_stmt) => {}
    }

    // Execute through the EXISTING backend API, not a hand-rolled
    // connection + stringify. `KuzuBackend::raw_query` already returns
    // `Vec<Vec<String>>` -- the exact wire shape -- and already no-ops bare
    // BEGIN/COMMIT/ROLLBACK, which a fresh-connection-per-call design must
    // do and which a hand-rolled path here would silently drop.
    match backend.raw_query(&req.query) {
        Ok(rows) => {
            for chunk in rows.chunks(req.chunk_size.max(1)) {
                write_frame(&mut stream, &ReadFrame::Rows(chunk.to_vec()))?;
            }
            write_frame(&mut stream, &ReadFrame::End)?;
        }
        Err(e) => write_frame(&mut stream, &ReadFrame::Error(e.to_string()))?,
    }
    Ok(())
}
```

Implement `threadpool_of(workers)` as a small fixed-size worker pool over a channel — do not add a thread-pool dependency for this; the crate already spawns threads directly elsewhere. The pool must be fixed-size, not unbounded, so a burst of clients cannot spawn unbounded threads inside the daemon.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p infigraph-core --test read_service -- --test-threads=1`
Expected: PASS, `2 passed`.

- [ ] **Step 5: Decide the BEGIN/COMMIT ordering, deliberately**

`ensure_read_only` runs *before* `raw_query`, so a bare `COMMIT` — which
`raw_query` today silently no-ops — will instead be refused as not-read-only.
For a read service that is arguably correct, but it is a behaviour change and
must be a decision, not an accident. Add a test pinning whichever you choose:

```rust
#[test]
fn a_bare_commit_sent_to_the_read_service_is_refused_not_silently_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    drop(infigraph_core::graph::GraphStore::open(&graph).unwrap());
    let store = std::sync::Arc::new(infigraph_core::graph::GraphStore::open(&graph).unwrap());
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, store, 2).unwrap();

    let got = client_query(root, "COMMIT");
    assert!(
        got.is_err(),
        "a transaction control statement has no meaning on the read service and must \
         be refused rather than silently returning an empty result"
    );

    svc.shutdown();
}
```

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/daemon/read_service.rs \
        crates/infigraph-core/src/daemon/mod.rs \
        crates/infigraph-core/tests/read_service.rs
git commit -m "feat(core): daemon read service, off the watch loop and off the lock"
```

---

### Task 7: Prove reads do not take `index.lock`

The concurrency property is the entire justification for a separate service. Assert it.

**Files:**
- Modify: `crates/infigraph-core/tests/read_service.rs`

**Interfaces:**
- Consumes: everything from Task 6.

- [ ] **Step 1: Write the failing test**

```rust
/// Reads must be served in parallel *while* an index operation holds
/// `index.lock`. If reads ever queue behind indexing, the read service has
/// been wired into the write pipeline by mistake and the whole point is
/// lost.
#[test]
fn reads_are_served_concurrently_while_an_index_operation_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    {
        let store = infigraph_core::graph::GraphStore::open(&graph).unwrap();
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();
    }
    let db = std::sync::Arc::new(open_shared_database(&graph));
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, db, 8).unwrap();

    // Hold index.lock for the duration, as a real index operation would.
    let _index_lock = infigraph_core::lockfile::acquire(
        &root.join(".infigraph").join("index.lock"),
        "test-index-op",
        std::time::Duration::from_secs(5),
    )
    .expect("acquire index.lock");

    let start = std::time::Instant::now();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let root = root.to_path_buf();
            std::thread::spawn(move || client_query(&root, "MATCH (f:File) RETURN f.id"))
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap().unwrap(), vec![vec!["a.rs".to_string()]]);
    }
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "8 reads took {:?} with index.lock held -- they are queueing behind it",
        start.elapsed()
    );

    svc.shutdown();
}
```

- [ ] **Step 2: Write the same-`Database` visibility test**

This is the test that catches the whole class of "the read service opened its
own handle". A second handle passes every test that has no concurrent writer.

```rust
/// A write committed by the daemon must be visible to the very next read.
///
/// If the read service ever holds its own `Database` -- even in the same
/// process -- it cannot see the writer's uncommitted WAL, and this fails.
/// That is #149 reproduced inside the daemon, and no test without a
/// concurrent writer would notice.
#[test]
fn a_write_is_visible_to_the_next_read_through_the_service() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();

    let store = std::sync::Arc::new(
        infigraph_core::graph::GraphStore::open(&graph).unwrap(),
    );
    let svc = infigraph_core::daemon::read_service::ReadService::start(
        root,
        store.clone(),
        4,
    )
    .unwrap();

    assert!(client_query(root, "MATCH (f:File) RETURN f.id").unwrap().is_empty());

    // Write through the SAME store the service holds, without checkpointing.
    {
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'fresh.rs', name: 'fresh.rs', path: 'fresh.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();
    }

    let rows = client_query(root, "MATCH (f:File) RETURN f.id").unwrap();
    assert_eq!(
        rows,
        vec![vec!["fresh.rs".to_string()]],
        "the read service must observe the daemon's own uncheckpointed write; \
         if this is empty, the service is holding a second Database handle"
    );

    svc.shutdown();
}
```

- [ ] **Step 3: Run both**

Run: `cargo test -p infigraph-core --test read_service -- --test-threads=1`
Expected: PASS. If the elapsed-time assertion fails, the service is taking a lock it must not take — fix the service, never the bound. If the visibility assertion returns an empty result, the service is on its own `Database` — fix the ownership, never the assertion.

- [ ] **Step 4: Commit**

```bash
git add crates/infigraph-core/tests/read_service.rs
git commit -m "test(core): reads are concurrent and see the daemon's own writes"
```

---

### Task 8: `RemoteExec` client

**Files:**
- Create: `crates/infigraph-core/src/graph/remote_exec.rs`
- Modify: `crates/infigraph-core/src/graph/mod.rs` (add `pub mod remote_exec;`)

**Interfaces:**
- Produces: `RemoteExec::new(root: &Path) -> RemoteExec`, implementing `QueryExec` from Task 1.
- Consumes: `ReadEndpoint` (Task 2), `read_protocol` (Task 5), `QueryExec` (Task 1).

- [ ] **Step 1: Write the failing test**

Add to `crates/infigraph-core/tests/read_service.rs`:

```rust
/// The client-side executor speaks the same protocol the service serves,
/// and satisfies the same `QueryExec` trait `GraphQuery` runs on -- which is
/// what lets all 1045 lines of Cypher serve both paths unchanged.
#[test]
fn remote_exec_satisfies_query_exec_against_a_live_service() {
    use infigraph_core::graph::query_exec::QueryExec;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    {
        let store = infigraph_core::graph::GraphStore::open(&graph).unwrap();
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();
    }
    let db = std::sync::Arc::new(open_shared_database(&graph));
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, db, 2).unwrap();

    // `RemoteExec` satisfies `QueryExec`, so it can be handed to
    // `GraphQuery::new_with(exec)` by value exactly like `LocalExec`.
    let exec = infigraph_core::graph::remote_exec::RemoteExec::new(root);
    let rows = exec.query_rows("MATCH (f:File) RETURN f.id").unwrap();
    assert_eq!(rows, vec![vec!["a.rs".to_string()]]);

    svc.shutdown();
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-core --test read_service remote_exec -- --test-threads=1`
Expected: FAIL to compile — `RemoteExec` not defined.

- [ ] **Step 3: Implement**

```rust
//! Client-side `QueryExec` that runs queries on the daemon's read service.
//!
//! Deliberately implements the same trait `LocalExec` does, so `GraphQuery`
//! -- and therefore every read query in the codebase -- runs unchanged over
//! either. Re-implementing those queries for a remote backend would
//! duplicate all 1045 lines of them.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::daemon::read_endpoint::ReadEndpoint;
use crate::daemon::read_protocol::{collect_rows, write_request, ReadRequest, Store};
use crate::graph::query_exec::QueryExec;

pub struct RemoteExec {
    root: PathBuf,
    store: Store,
}

impl RemoteExec {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            store: Store::Graph,
        }
    }

    pub fn for_docs(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            store: Store::Docs,
        }
    }
}

impl QueryExec for RemoteExec {
    fn query_rows(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        let endpoint = ReadEndpoint::for_root(&self.root);
        let mut stream = endpoint
            .connect()
            .with_context(|| "no daemon read service is listening for this project")?;
        write_request(
            &mut stream,
            &ReadRequest {
                store: self.store,
                query: cypher.to_string(),
                params: vec![],
                chunk_size: 1024,
            },
        )?;
        collect_rows(&mut stream)
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p infigraph-core --test read_service -- --test-threads=1`
Expected: PASS, all four tests.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/graph/remote_exec.rs crates/infigraph-core/src/graph/mod.rs \
        crates/infigraph-core/tests/read_service.rs
git commit -m "feat(core): RemoteExec runs GraphQuery against the daemon read service"
```

---

### Task 9: Truncation is an error end-to-end

**Files:**
- Modify: `crates/infigraph-core/tests/read_service.rs`

- [ ] **Step 1: Write the test**

```rust
/// Killing the service mid-response must surface an error, never a
/// successful empty result. This is the one failure mode with a precedent
/// in this codebase: `Infigraph::init` once served a 0-symbol graph as
/// healthy after a "successful" rebuild.
#[test]
fn a_service_that_dies_mid_response_produces_an_error_not_an_empty_result() {
    use infigraph_core::daemon::read_protocol::{collect_rows, write_frame, ReadFrame};

    // Simulate the wire directly: rows, then the connection dies.
    let mut buf = Vec::new();
    write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["a.rs".to_string()]])).unwrap();
    // No End frame -- the peer went away.

    let err = collect_rows(&mut buf.as_slice())
        .expect_err("a stream with no End frame must be an error");
    assert!(
        err.to_string().contains("not an empty result set"),
        "the error must say plainly that this is not an empty result: {err}"
    );
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p infigraph-core --test read_service -- --test-threads=1`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/infigraph-core/tests/read_service.rs
git commit -m "test(core): a truncated read stream is an error, not an empty result"
```

---

### Task 10: Share the daemon's `GraphStore` and start the read service

This is where the single-`Database` invariant is actually established. It is two
things — threading ownership, then starting the service — kept as one task because
starting the service without the shared store is exactly the mistake this plan exists
to prevent.

**Files:**
- Modify: `crates/infigraph-core/src/graph/kuzu_backend.rs` (`KuzuBackend` holds `Arc<GraphStore>`)
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (daemon startup, alongside the write coordinator)
- Test: `crates/infigraph-core/tests/read_service.rs`

**Interfaces:**
- Produces: `KuzuBackend::store(&self) -> Arc<GraphStore>`.
- Consumes: `ReadService::start(root, store, workers)` (Task 6).

**The ownership problem.** The daemon reaches its store through
`Infigraph → BackendKind::Kuzu(KuzuBackend) → GraphStore`, and `KuzuBackend` owns it
by value (`pub struct KuzuBackend { store: GraphStore }`). The read service needs a
handle to that same store, so ownership must become shared.

Do **not** solve this by opening a second store in the daemon. A second handle on one
graph file cannot see the first's uncommitted WAL even inside one process — #149
reproduced inside the daemon, and invisible to any test without a concurrent writer.
Task 7's visibility test exists to catch exactly this.

- [ ] **Step 1: Make `KuzuBackend` hold `Arc<GraphStore>`**

```rust
pub struct KuzuBackend {
    store: std::sync::Arc<GraphStore>,
}

impl KuzuBackend {
    /// A handle to this backend's store, so the daemon's read service can
    /// serve from the SAME `Database` the write path uses.
    pub fn store(&self) -> std::sync::Arc<GraphStore> {
        self.store.clone()
    }
}
```

Wrap the store in the constructor(s). Existing `self.store.connection()` call sites
compile unchanged through `Deref`.

- [ ] **Step 2: Confirm the ownership change is inert**

Run: `cargo test -p infigraph-core -- --test-threads=1`
Expected: PASS with unchanged counts. This is a pure ownership change; any movement
means something depended on `GraphStore` being owned by value.

- [ ] **Step 3: Locate the startup site**

Run: `rg -n "run_write_coordinator" crates/infigraph-core/src/daemon/mod.rs crates/infigraph-cli/src/main.rs`

The read service starts in the same function that starts the write coordinator, before
it enters its loop, and shuts down when that function returns.

- [ ] **Step 4: Write the failing test**

```rust
/// A real daemon must answer reads on its endpoint. Without this the service
/// exists but nothing starts it.
#[test]
#[ignore = "spawns a real daemon; run explicitly"]
fn a_running_daemon_answers_reads_on_its_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.rs"), "pub fn hello() {}\n").unwrap();

    let daemon = spawn_daemon_and_wait(root);

    let rows = client_query(root, "MATCH (f:File) RETURN f.id").unwrap();
    assert!(!rows.is_empty(), "a running daemon must serve reads");

    stop_daemon(daemon);
}
```

Write `spawn_daemon_and_wait` and `stop_daemon` using the pattern
`crates/infigraph-core/tests/watch_daemon.rs` already uses to start a daemon and await
its `watch.lock`; do not invent a new mechanism.

- [ ] **Step 5: Run to verify it fails**

Run: `cargo test -p infigraph-core --test read_service -- --ignored a_running_daemon --test-threads=1`
Expected: FAIL — nothing is listening on the endpoint.

- [ ] **Step 6: Wire it in**

Start `ReadService::start(root, backend.store(), 8)` where the daemon already holds its
`Infigraph`, keeping the `ReadService` alive for the daemon's lifetime and calling
`shutdown()` on exit. The store handle comes from the daemon's existing backend — never
from a fresh `GraphStore::open`.

- [ ] **Step 7: Run to verify it passes**

Run: `cargo test -p infigraph-core --test read_service -- --ignored a_running_daemon --test-threads=1`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/infigraph-core/src/graph/kuzu_backend.rs \
        crates/infigraph-core/src/daemon/mod.rs \
        crates/infigraph-core/tests/read_service.rs
git commit -m "feat(core): daemon shares one GraphStore with its read service"
```

---

### Task 11: Route `DaemonKuzuBackend` reads, with the escape hatch

**Files:**
- Modify: `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs:83` (`open_read`)

**Interfaces:**
- Consumes: `RemoteExec` (Task 8).

`open_read` has fan-in 260 — every one of the ~30 read methods delegates to it. It is the single site that decides local-vs-remote.

- [ ] **Step 1: Write the failing test**

Add to `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs`'s test module:

```rust
/// Reads are daemon-mandatory. The escape hatch exists so a graph is
/// recoverable when the daemon itself is broken, and is deliberately
/// explicit: a silent fallback would keep both paths permanently live,
/// which is how `Infigraph::init` and `GraphStore::open_read_only_or_degrade`
/// drifted apart until one quarantined healthy graphs (5818aa1).
#[test]
fn the_escape_hatch_restores_a_direct_read_when_set() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    drop(crate::graph::GraphStore::open(&graph).unwrap());

    // No daemon is running, so without the hatch this must fail.
    let backend = DaemonKuzuBackend::open(root).unwrap();
    assert!(
        backend.stats().is_err(),
        "with no daemon and no escape hatch, a read must fail rather than silently \
         opening the file directly"
    );

    std::env::set_var("INFIGRAPH_DIRECT_READS", "1");
    let got = backend.stats();
    std::env::remove_var("INFIGRAPH_DIRECT_READS");
    assert!(got.is_ok(), "the escape hatch must restore a direct read: {got:?}");
}
```

Note: this test mutates process environment. Guard it with the crate's existing `ENV_LOCK` (see `crates/infigraph-core/src/graph/store.rs` tests for the established pattern) so it cannot race other env-mutating tests.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p infigraph-core --lib daemon_kuzu_backend -- --test-threads=1`
Expected: FAIL — `stats()` currently succeeds by opening the file directly.

- [ ] **Step 3: Implement**

Replace `open_read`'s body so that it returns a backend whose `GraphQuery` runs on `RemoteExec`, unless `INFIGRAPH_DIRECT_READS` is set, in which case it keeps today's `KuzuBackend::open_read_only` behaviour. Document at the call site why the hatch is explicit rather than automatic, citing 5818aa1.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p infigraph-core --lib daemon_kuzu_backend -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Full suite**

Run: `cargo test -p infigraph-core -- --test-threads=1`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/graph/daemon_kuzu_backend.rs
git commit -m "feat(core): DaemonKuzuBackend reads route through the daemon"
```

---

### Task 12: Route document-store reads

**Files:**
- Modify: `crates/infigraph-docs/src/backend.rs`
- Test: `crates/infigraph-docs/tests/` (new file, matching that crate's conventions)

`search` with `scope='all'` touches both stores in one call, so routing only the graph leaves the flagship read tool still opening `docs.kuzu` directly. `docs.kuzu` has its own lock file and its own wipe-on-any-open-failure history (#143).

- [ ] **Step 1: Read the docs backend**

Run: `cargo run -q -p infigraph-cli -- --root . get-skeleton crates/infigraph-docs/src/backend.rs` or read the file directly. Identify the read entry points and whether they, like `KuzuBackend`, funnel through a single connection call.

- [ ] **Step 2: Write the failing test**

Create `crates/infigraph-docs/tests/docs_reads_via_daemon.rs`:

```rust
use std::sync::Arc;

/// `search` with `scope='all'` hits the code graph AND the document store in
/// one call, so routing only the graph would leave the flagship read tool
/// still opening `docs.kuzu` directly. `docs.kuzu` has its own lock file and
/// its own wipe-on-any-open-failure history (#143).
#[test]
fn document_reads_are_served_by_the_daemon_read_service() {
    use infigraph_core::graph::query_exec::QueryExec;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let docs = root.join(".infigraph").join("docs.kuzu");
    std::fs::create_dir_all(docs.parent().unwrap()).unwrap();

    // Seed one document through the docs store's own open path.
    seed_one_document(&docs);

    let db = Arc::new(open_shared_docs_database(&docs));
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, db, 2).unwrap();

    let exec = infigraph_core::graph::remote_exec::RemoteExec::for_docs(root);
    let rows = exec.query_rows("MATCH (d:Document) RETURN d.id").unwrap();
    assert!(!rows.is_empty(), "the docs store must answer over the read service");

    svc.shutdown();
}

/// With no daemon and no escape hatch, a document read must fail rather than
/// silently opening `docs.kuzu` directly.
#[test]
fn document_reads_fail_without_a_daemon_unless_the_hatch_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let docs = root.join(".infigraph").join("docs.kuzu");
    std::fs::create_dir_all(docs.parent().unwrap()).unwrap();
    seed_one_document(&docs);

    let exec = infigraph_core::graph::remote_exec::RemoteExec::for_docs(root);
    assert!(
        infigraph_core::graph::query_exec::QueryExec::query_rows(
            &exec,
            "MATCH (d:Document) RETURN d.id"
        )
        .is_err(),
        "no daemon and no hatch must be an error, not a silent direct open"
    );
}
```

Write `seed_one_document` and `open_shared_docs_database` as helpers at the bottom of this file, using `infigraph-docs`'s own store-open path (the same one `crates/infigraph-docs/tests/wal_guard.rs` uses) rather than constructing a Kuzu database by hand.

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p infigraph-docs --test docs_reads_via_daemon -- --test-threads=1`
Expected: FAIL — `RemoteExec::for_docs` reads are not wired into the docs backend yet.

- [ ] **Step 4: Route the docs read path**

In `crates/infigraph-docs/src/backend.rs`, change the read entry points identified in Step 1 to obtain rows through `RemoteExec::for_docs(root)` rather than a locally-opened store, honouring the same `INFIGRAPH_DIRECT_READS` escape hatch as Task 11. Do not duplicate the hatch check — factor it into one helper shared with `DaemonKuzuBackend::open_read` if it is not already one.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p infigraph-docs -- --test-threads=1`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-docs/
git commit -m "feat(docs): document-store reads route through the daemon"
```

---

### Task 13: Retire the machinery reads no longer need

Do this last and separately: it is deletion, and it must not be entangled with the change that made the deletion safe.

**Files:**
- Modify: `crates/infigraph-core/src/graph/store.rs` (`classify_read_only_open_failure`, `live_graph_writer`, `READ_ONLY_BUFFER_POOL_BYTES`, `open_read_only_or_degrade`)

- [ ] **Step 1: Confirm each is genuinely unreachable**

For each of `classify_read_only_open_failure`, `live_graph_writer`, `open_read_only_or_degrade`, `degrade_or_refuse` and `READ_ONLY_BUFFER_POOL_BYTES`, run `rg -n "<name>" crates/` and confirm the only remaining callers are the escape-hatch path and tests. Anything still reachable from a non-hatch path stays.

- [ ] **Step 2: Remove only what is provably dead, one item per commit**

Each removal is its own commit with its own full-suite run, so a bisect lands on exactly one deletion.

Run after each: `cargo test -p infigraph-core -- --test-threads=1`

- [ ] **Step 3: Update the invariants in CLAUDE.md**

The "Cross-cutting invariants" section states that the graph DB is single-writer and that write paths take an advisory lock. Add that reads no longer open the store outside the daemon, and that `INFIGRAPH_DIRECT_READS` is the sole exception.

- [ ] **Step 4: Close the issues this retires**

`#149` (idle daemon's WAL blocks external readers) and `#124` (PID-liveness polling) are both resolved by this work. Comment on each with the commit that closed it rather than closing silently.

---

## Self-Review

**Spec coverage.** Architecture → Tasks 6, 10. Endpoint/portability → Task 2, 3. Wire API → Task 5. Read-only enforcement → Task 4. Execution/connection pool → Task 6. Client integration → Tasks 8, 11, 12. Escape hatch → Task 11. Error handling → Tasks 5, 9, 11. Testing → Tasks 4, 7, 9. What this retires → Task 13. Sequencing (reads on their own socket, writes untouched) → Global Constraints.

**Deviations from the spec, deliberate and flagged for the executor:**

1. **Rows cross the wire as JSON `Vec<Vec<String>>`, not Arrow.** The spec specifies chunked Arrow via `query_as_arrow`. `GraphQuery` consumes rows stringly already, so Arrow would be encoded and immediately decoded back to strings for no gain until the row shape itself is widened. Framing, chunking and truncation semantics are all in place, so substituting Arrow inside `ReadFrame::Rows` is a contained change. **Raise this with the spec author before starting Task 5 if you disagree** — it is the one place this plan does not do what the spec says.
2. **Connection pool exhaustion is not resolved.** Spec Open Question 1. This plan uses a fixed 8-worker pool where a burst simply waits for a free worker. If refusal is wanted instead, that is a change to Task 6.
3. **Group/multi-repo reads are not covered.** Spec Open Question 2, untouched here.
4. **The escape hatch trusts the operator.** Spec Open Question 3; this plan does not require the daemon to be provably unstartable.

**Corrections applied after review (2026-09-08).** Three defects were found in the
first draft and fixed:

1. `ReadService` took a bare `Arc<kuzu::Database>`. That discarded everything
   `GraphStore` guarantees (truncation preflight, schema refusal, the bounded pool
   from 13b4065, `write_phase` breadcrumbs) and left the handle's provenance
   unspecified — inviting a second `Database` in the daemon, which cannot see the
   writer's uncommitted WAL and would have reproduced #149 inside the process. Now
   `Arc<GraphStore>`, threaded from the daemon's own backend in Task 10, with Task 7's
   visibility test to catch any regression.
2. `serve_one` hand-rolled connection + stringify instead of calling the existing
   `KuzuBackend::raw_query`, which already returns the exact wire shape and already
   no-ops bare `BEGIN`/`COMMIT`/`ROLLBACK`. The hand-rolled path would have silently
   dropped that handling.
3. The `QueryExec` / `GraphQuery::raw_query` dependency direction was unstated, and
   inverting it recurses forever. Task 1 Step 6 now fixes the direction explicitly.

**Type consistency.** `QueryExec::query_rows(&self, &str) -> Result<Vec<Vec<String>>>` is defined in Task 1 (as built, `e162b0b`) and used identically in Tasks 6 and 8; `GraphQuery` is generic over an owned `E: QueryExec`, so executors are passed by value (a blanket `impl QueryExec for &T` covers the borrowed case). `ReadEndpoint::for_root`/`as_name` (Task 2) gain `bind`/`connect` in Task 3 and are used in Tasks 6 and 8. `ReadRequest`/`ReadFrame`/`collect_rows` (Task 5) are used in Tasks 6, 8 and 9. `ReadService::start(root: &Path, store: Arc<GraphStore>, workers: usize)`/`shutdown` (Task 6) is used identically in Tasks 7-10; `KuzuBackend::store() -> Arc<GraphStore>` (Task 10) is what supplies it.
