# CALLS_SERVICE Edge Write Transaction Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix a real, confirmed bug where writing `CALLS_SERVICE` edges (dynamic-URL/route matches) issues `BEGIN TRANSACTION`/`CREATE`/`COMMIT` as three separate raw query strings, each of which silently gets its own fresh Kùzu connection — so the transaction never actually spans the writes, and `COMMIT` reliably fails with "No active transaction for COMMIT" whenever any edge is written. Move this write onto a proper `GraphBackend` trait method that owns its own transaction internally, matching every other multi-step write already in the trait (`upsert_files_bulk`, `derive_tested_by_edges`, `resolve_calls`).

**Architecture:** Add one new `GraphBackend` trait method, `write_calls_service_edges(&self, edges: &[CallsServiceEdge]) -> Result<()>`, implemented for real in both `KuzuBackend` (one held connection, correct `BEGIN`/loop/`COMMIT`) and `Neo4jBackend` (single `UNWIND`-parameterized query, no manual transaction needed — Neo4j auto-commits a single Cypher statement atomically). `crates/infigraph-core/src/taint/dynamic_urls.rs` calls the new trait method instead of hand-rolling transaction-control query strings through `raw_query`.

**Tech Stack:** Rust (edition 2021), `kuzu`/`lbug` (embedded, vendored fork) for local backend, `neo4rs` for remote backend (behind the `remote` cargo feature).

## Global Constraints

- **Branch: base directly on a freshly-fetched `upstream/main`** (confirmed tip `fe410eb`, same as the concurrent watcher-daemon-split work — unrelated, independent branch, do not stack on it or on `feat/health-beacons`). Worktree: `scratchpad/wt-calls-service-tx`, branch `fix/calls-service-edges-transaction`.
- **Target: upstream directly** (`intuit/infigraph`, base `main`), per explicit user instruction ("commit as a clean PR upstream") — overrides the repo's standing fork-only default for this PR specifically.
- **Both backend implementations must ship in the same PR.** `GraphBackend` is implemented for both `KuzuBackend` (always compiled) and `Neo4jBackend` (behind `--features remote`). A no-op default for the new trait method would silently drop these edges for whichever backend doesn't get a real implementation — worse than today's bug, which at least *attempts* the write. Do not add a default trait-method body; both impls are mandatory.
- Every `cargo` invocation runs with `CARGO_PROFILE_DEV_DEBUG=0` (hard repo rule — mixing debug settings spawns multi-GB duplicate build trees).
- Commit with `--no-verify` only after running `cargo fmt` manually, and only if the pre-commit hook fails on a pre-existing, already-catalogued environmental flake (`write_lock_perf::test_contended_lock_throughput` — the one flake this campaign has consistently seen; if a *different* failure appears, investigate before bypassing, do not assume it's environmental).
- This branch is fully independent of `feat/watcher-daemon-split` (a separate, concurrent, unrelated fix) — no shared commits, no dependency either direction.
- `dynamic_urls.rs`'s escaping convention (`crate::escape_str`, raw string interpolation, not prepared-statement parameters) is the established Kùzu-side pattern in this codebase — preserve it for the Kùzu implementation rather than introducing parameterized queries there (Neo4j's implementation, by contrast, uses `neo4rs`'s real parameter binding, which is *that* backend's own established convention — see `derive_tested_by_edges`'s Neo4j implementation for the precedent).

---

### Task 1: `GraphBackend` trait method + Kùzu implementation + caller update

**Files:**
- Modify: `crates/infigraph-core/src/graph/backend.rs` — add `CallsServiceEdge` struct and the new trait method
- Modify: `crates/infigraph-core/src/graph/kuzu_backend.rs` — implement it
- Modify: `crates/infigraph-core/src/taint/dynamic_urls.rs` — replace the buggy private `write_calls_service_edges` function with calls to the new trait method at both call sites; delete the old function
- Test: `crates/infigraph-core/tests/kuzu_backend.rs` (extend) or a new `crates/infigraph-core/tests/calls_service_edges.rs`

**Interfaces:**
- Produces: `pub struct CallsServiceEdge { pub symbol_id: String, pub target_id: String, pub method: String, pub path: String }` and `fn write_calls_service_edges(&self, edges: &[CallsServiceEdge]) -> Result<()>` on the `GraphBackend` trait. Task 2 implements this same trait method for `Neo4jBackend` — the struct and trait signature must not change between tasks.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/tests/calls_service_edges.rs`:

```rust
use infigraph_core::graph::{CallsServiceEdge, GraphBackend, KuzuBackend};
use infigraph_core::model::FileExtraction;
use std::path::Path;

fn seed_two_symbols(backend: &KuzuBackend, root: &Path) {
    let src = FileExtraction {
        file: "caller.py".to_string(),
        language: "python".to_string(),
        content_hash: "h1".to_string(),
        symbols: vec![infigraph_core::model::Symbol {
            id: "caller.py::handler".to_string(),
            name: "handler".to_string(),
            kind: "Function".to_string(),
            file: "caller.py".to_string(),
            start_line: 1,
            end_line: 5,
            docstring: None,
            complexity: 1,
            signature_hash: "s1".to_string(),
            visibility: "public".to_string(),
            parent: None,
            embedding: None,
        }],
        ..Default::default()
    };
    let tgt = FileExtraction {
        file: "target.py".to_string(),
        language: "python".to_string(),
        content_hash: "h2".to_string(),
        symbols: vec![infigraph_core::model::Symbol {
            id: "target.py::endpoint".to_string(),
            name: "endpoint".to_string(),
            kind: "Function".to_string(),
            file: "target.py".to_string(),
            start_line: 1,
            end_line: 5,
            docstring: None,
            complexity: 1,
            signature_hash: "s2".to_string(),
            visibility: "public".to_string(),
            parent: None,
            embedding: None,
        }],
        ..Default::default()
    };
    let _ = root;
    backend.upsert_file(&src).unwrap();
    backend.upsert_file(&tgt).unwrap();
}

/// Regression test for the exact bug found this session: writing more than
/// one CALLS_SERVICE edge used to fail with "No active transaction for
/// COMMIT" because BEGIN/CREATE-loop/COMMIT were three separate raw_query
/// calls, each silently getting its own fresh Kùzu connection. This test
/// writes two edges (the loop must run more than once for the old bug to
/// reliably manifest) and asserts both real edges exist afterward.
#[test]
fn write_calls_service_edges_creates_all_edges_in_one_call() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = KuzuBackend::open(tmp.path()).unwrap();
    seed_two_symbols(&backend, tmp.path());

    let edges = vec![
        CallsServiceEdge {
            symbol_id: "caller.py::handler".to_string(),
            target_id: "target.py::endpoint".to_string(),
            method: "GET".to_string(),
            path: "/api/one".to_string(),
        },
        CallsServiceEdge {
            symbol_id: "caller.py::handler".to_string(),
            target_id: "target.py::endpoint".to_string(),
            method: "POST".to_string(),
            path: "/api/two".to_string(),
        },
    ];

    backend
        .write_calls_service_edges(&edges)
        .expect("write_calls_service_edges must succeed, not fail with 'No active transaction for COMMIT'");

    let rows = backend
        .raw_query("MATCH (:Symbol)-[r:CALLS_SERVICE]->(:Symbol) RETURN r.method, r.path ORDER BY r.method")
        .unwrap();
    assert_eq!(rows.len(), 2, "expected both edges to be created, got {rows:?}");
    assert_eq!(rows[0][0], "GET");
    assert_eq!(rows[0][1], "/api/one");
    assert_eq!(rows[1][0], "POST");
    assert_eq!(rows[1][1], "/api/two");
}

/// Empty input must not error (matches the old code's behavior — the old
/// function's caller only invoked it when `!urls.is_empty()`, but the new
/// trait method should be safe to call with zero edges regardless).
#[test]
fn write_calls_service_edges_empty_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = KuzuBackend::open(tmp.path()).unwrap();
    backend.write_calls_service_edges(&[]).unwrap();
}
```

Before finalizing this file, read `crates/infigraph-core/src/model.rs`'s actual `Symbol`/`FileExtraction` struct definitions and `crates/infigraph-core/tests/kuzu_backend.rs`'s existing test setup helpers — this step's test code above is written from the fields visible in `dynamic_urls.rs`'s own usage and standard model conventions in this codebase, but field names/optionality must be verified against the real struct before this compiles. Adapt field names exactly; do not guess.

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test calls_service_edges`
Expected: COMPILE ERROR — `CallsServiceEdge` and `write_calls_service_edges` don't exist yet on `GraphBackend`/in `infigraph_core::graph`'s public exports.

- [ ] **Step 3: Add the trait method to `GraphBackend`**

In `crates/infigraph-core/src/graph/backend.rs`, add near the other write methods (after `upsert_repo`, before the `// ── Resolve ──` section):

```rust
/// A single detected dynamic-URL/route match, to be written as a
/// `CALLS_SERVICE` edge from the calling symbol to the matched route
/// handler.
#[derive(Debug, Clone)]
pub struct CallsServiceEdge {
    pub symbol_id: String,
    pub target_id: String,
    pub method: String,
    pub path: String,
}

/// Write a batch of `CALLS_SERVICE` edges as a single atomic operation.
/// Backend owns the transaction — callers don't manage connections
/// directly (same design as `upsert_files_bulk`/`resolve_calls`). A
/// no-op for an empty slice.
fn write_calls_service_edges(&self, edges: &[CallsServiceEdge]) -> Result<()>;
```

Add `CallsServiceEdge` to this module's existing re-export list if `backend.rs`'s types are re-exported elsewhere (check `crates/infigraph-core/src/graph/mod.rs` for how `ApiSymbol`/`BranchInfo`/etc. are currently re-exported and follow the exact same pattern so `infigraph_core::graph::CallsServiceEdge` is a valid public path, matching this plan's test file's `use infigraph_core::graph::{CallsServiceEdge, GraphBackend, KuzuBackend};`).

- [ ] **Step 4: Implement it in `KuzuBackend`**

In `crates/infigraph-core/src/graph/kuzu_backend.rs`, add the implementation inside the existing `impl GraphBackend for KuzuBackend` block (find the block via `crate::graph::GraphBackend` — read the file to find exactly where the `impl` block is and where `upsert_repo`/`derive_tested_by_edges` are implemented, to place this consistently near them):

```rust
fn write_calls_service_edges(&self, edges: &[CallsServiceEdge]) -> Result<()> {
    if edges.is_empty() {
        return Ok(());
    }
    let conn = self.store.connection()?;
    conn.query("BEGIN TRANSACTION")
        .map_err(|e| anyhow::anyhow!("failed to begin transaction: {e}"))?;
    for edge in edges {
        let src_esc = crate::escape_str(&edge.symbol_id);
        let tgt_esc = crate::escape_str(&edge.target_id);
        let method_esc = crate::escape_str(&edge.method);
        let path_esc = crate::escape_str(&edge.path);
        if let Err(e) = conn.query(&format!(
            "MATCH (s:Symbol), (t:Symbol) WHERE s.id = '{src_esc}' AND t.id = '{tgt_esc}' \
             CREATE (s)-[:CALLS_SERVICE {{method: '{method_esc}', path: '{path_esc}', target_service: ''}}]->(t)"
        )) {
            // Roll back so we don't leave a half-applied batch or a
            // dangling open transaction on this connection, then
            // surface the real error instead of masking it.
            let _ = conn.query("ROLLBACK");
            return Err(anyhow::anyhow!("failed to create CALLS_SERVICE edge: {e}"));
        }
    }
    conn.query("COMMIT")
        .map_err(|e| anyhow::anyhow!("failed to commit CALLS_SERVICE edges: {e}"))?;
    Ok(())
}
```

Before finalizing: read `GraphStore::connection()`'s actual return type (`crates/infigraph-core/src/graph/store.rs:135-137`, confirmed this session: `pub fn connection(&self) -> Result<Connection<'_>>`) and check what method the vendored Kùzu `Connection` type actually exposes for running a query — this plan assumes `.query(&str) -> Result<_, _>` based on `store.rs`'s own internal usage (`conn.query(ddl)` at `store.rs:126`) — confirm this is the same method `GraphQuery`/`raw_query` ultimately calls, and match its exact error type/signature rather than guessing. If the real API differs (e.g. returns a `QueryResult` that must be consumed/drained rather than a bare `Result<(), E>`), adapt the step's code to match, and note the discrepancy in your report — this is the same "brief may be written against slightly different code" caution used throughout this campaign's other plans.

- [ ] **Step 5: Update `dynamic_urls.rs`'s two call sites, delete the old function**

Read `crates/infigraph-core/src/taint/dynamic_urls.rs` in full first (confirmed this session: `detect_dynamic_urls` at line 94, `detect_dynamic_urls_with_cache` at line 150, both call the private `write_calls_service_edges(backend, &urls)?` — at lines 144 and 182 respectively — and the buggy private function itself is at lines 402-422).

Delete the private `write_calls_service_edges` function (lines 402-422) entirely. At both call sites, replace:

```rust
if !urls.is_empty() {
    write_calls_service_edges(backend, &urls)?;
}
```

with:

```rust
let edges: Vec<crate::graph::CallsServiceEdge> = urls
    .iter()
    .filter_map(|url| {
        url.matched_route.as_ref().map(|matched| crate::graph::CallsServiceEdge {
            symbol_id: url.symbol_id.clone(),
            target_id: matched.handler_id.clone(),
            method: matched.method.clone(),
            path: url.url_template.clone(),
        })
    })
    .collect();
backend.write_calls_service_edges(&edges)?;
```

(The `!urls.is_empty()` guard is no longer needed at the call site — `write_calls_service_edges` is now a no-op for an empty slice per Step 3's doc comment and Step 1's `write_calls_service_edges_empty_is_a_noop` test — but `urls` being non-empty doesn't guarantee `edges` is non-empty either, since `matched_route` can be `None`; the `filter_map` above handles that correctly either way.)

Confirm both call sites end up identical in shape (read the second call site's exact surrounding code in `detect_dynamic_urls_with_cache` before editing — it may differ slightly from `detect_dynamic_urls`'s, e.g. a different local variable name for `urls`).

- [ ] **Step 6: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test calls_service_edges`
Expected: PASS, 2/2.

Also run the existing dynamic-URL detection tests to confirm no regression:
Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core dynamic_url`
Expected: all pass (find the exact existing test names first via `cargo test -p infigraph-core dynamic_url -- --list` if unsure).

- [ ] **Step 7: Full crate + clippy + fmt**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --no-fail-fast -- --test-threads=4
CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-core --all-targets -- -D warnings
cargo fmt --all -- --check
```
Expected: green, modulo any already-catalogued pre-existing flake (confirm via stash-to-base-commit rerun before treating any failure as unrelated, same rigor as this campaign's other plans).

- [ ] **Step 8: Commit**

```bash
cargo fmt
git add crates/infigraph-core/src/graph/backend.rs crates/infigraph-core/src/graph/kuzu_backend.rs crates/infigraph-core/src/graph/mod.rs crates/infigraph-core/src/taint/dynamic_urls.rs crates/infigraph-core/tests/calls_service_edges.rs
git commit -m "fix: CALLS_SERVICE edge writes use one real transaction instead of BEGIN/COMMIT split across fresh connections"
```

(Adjust the `git add` file list if Step 3 required touching `graph/mod.rs`'s re-exports and it isn't already listed, or if it wasn't needed.)

---

### Task 2: Neo4j implementation

**Files:**
- Modify: `crates/infigraph-core/src/graph/neo4j_backend.rs`
- Test: extend `crates/infigraph-core/tests/neo4j_backend.rs` (existing file, confirmed present this session via `clear_graph` helper reference at `neo4j_backend.rs:L55-59`)

**Interfaces:**
- Consumes: `CallsServiceEdge` struct and `write_calls_service_edges` trait signature from Task 1 — must not diverge.

- [ ] **Step 1: Write the failing test**

Read `crates/infigraph-core/tests/neo4j_backend.rs` in full first to find its existing setup/teardown conventions (the `clear_graph` helper, how a `Neo4jBackend` test instance connects, and how other tests in this file are gated — this crate's neo4j tests run only under `--features neo4j` per `docs/REMOTE-MULTI-REPO.md`'s documented `cargo test -p infigraph-core --features neo4j` invocation). Add a new test to that file following its exact existing conventions:

```rust
#[test]
#[cfg_attr(not(feature = "remote"), ignore)]
fn write_calls_service_edges_creates_all_edges_in_one_call() {
    let backend = /* construct per this file's existing setup convention */;
    clear_graph(&backend);

    // seed two symbols via whatever this file's existing tests use to
    // create Symbol nodes (read an existing test in this file for the
    // exact pattern — e.g. via upsert_file with a minimal FileExtraction,
    // matching Task 1's Kùzu test's approach if this file doesn't already
    // have a shorter existing helper)

    let edges = vec![
        infigraph_core::graph::CallsServiceEdge {
            symbol_id: "caller.py::handler".to_string(),
            target_id: "target.py::endpoint".to_string(),
            method: "GET".to_string(),
            path: "/api/one".to_string(),
        },
        infigraph_core::graph::CallsServiceEdge {
            symbol_id: "caller.py::handler".to_string(),
            target_id: "target.py::endpoint".to_string(),
            method: "POST".to_string(),
            path: "/api/two".to_string(),
        },
    ];

    backend.write_calls_service_edges(&edges).unwrap();

    let count = backend
        .raw_query("MATCH (:Symbol)-[r:CALLS_SERVICE]->(:Symbol) RETURN count(r) AS c")
        .unwrap();
    assert_eq!(count[0][0], "2");
}
```

This step's exact test scaffolding (backend construction, symbol seeding) is intentionally left to be filled in from this file's own real, current conventions rather than guessed — read the file first, then write the complete test, following the "No Placeholders" rule: the version actually committed must have zero placeholder comments, only real working code adapted from what's actually in the file.

- [ ] **Step 2: Run test to verify it fails (requires a running Neo4j)**

Per `docs/REMOTE-MULTI-REPO.md`'s documented setup:
```bash
docker run -d -p 7687:7687 -e NEO4J_AUTH=neo4j/testpass neo4j:5-community
NEO4J_URI=127.0.0.1:7687 NEO4J_USER=neo4j NEO4J_PASSWORD=testpass \
  CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --features neo4j --test neo4j_backend write_calls_service_edges
```
Expected: COMPILE ERROR (trait method not yet implemented for `Neo4jBackend` — Rust will refuse to compile any `impl GraphBackend for Neo4jBackend` block missing a required trait method, so this should fail at compile time, not just at test-run time, confirming the impl is genuinely mandatory not optional).

If a local Docker/Neo4j instance isn't available in this environment, note that explicitly in the report rather than skipping verification silently — at minimum confirm the code compiles (`cargo build -p infigraph-core --features neo4j`) and reason through the query logic manually; flag the un-run integration test as a concern for the reviewer to weigh.

- [ ] **Step 3: Implement it in `Neo4jBackend`**

In `crates/infigraph-core/src/graph/neo4j_backend.rs`, add to the existing `impl GraphBackend for Neo4jBackend` block, placed near `derive_tested_by_edges` (confirmed this session at lines 1673-1701) since it follows the same "single parameterized query over a list" shape as that function's `Some(files)` branch:

```rust
fn write_calls_service_edges(&self, edges: &[CallsServiceEdge]) -> Result<()> {
    if edges.is_empty() {
        return Ok(());
    }
    let edge_maps: Vec<std::collections::HashMap<&str, String>> = edges
        .iter()
        .map(|e| {
            let mut m = std::collections::HashMap::new();
            m.insert("symbol_id", e.symbol_id.clone());
            m.insert("target_id", e.target_id.clone());
            m.insert("method", e.method.clone());
            m.insert("path", e.path.clone());
            m
        })
        .collect();
    self.block_on(
        self.graph.run(
            query(
                "UNWIND $edges AS e \
                 MATCH (s:Symbol), (t:Symbol) WHERE s.id = e.symbol_id AND t.id = e.target_id \
                 CREATE (s)-[:CALLS_SERVICE {method: e.method, path: e.path, target_service: ''}]->(t)",
            )
            .param("edges", edge_maps),
        ),
    )
    .map_err(|e| anyhow::anyhow!("write_calls_service_edges failed: {e}"))?;
    Ok(())
}
```

Before finalizing: read `derive_tested_by_edges`'s exact imports/helper usage (`self.block_on`, `self.graph.run`, `query(...).param(...)` — confirmed this session as the real pattern at `neo4j_backend.rs:1677-1689`) and confirm `neo4rs`'s `query()` builder actually accepts a `Vec<HashMap<&str, String>>` as a list-of-maps parameter (check `neo4rs`'s `Query::param` signature/trait bounds — it may require `impl Into<BoltType>` or similar, in which case the map's value type or construction may need adjusting to compile; this is exactly the kind of "confirm against the real API, don't assume" step this plan's Kùzu task also calls out).

- [ ] **Step 4: Run tests to verify they pass**

Same command as Step 2, now expected to pass (2/2 including the empty-input no-op case if added — add one matching Task 1's `write_calls_service_edges_empty_is_a_noop` test, adapted to this file's conventions).

- [ ] **Step 5: Clippy + fmt (feature-gated build)**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-core --features remote
CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-core --features remote --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/infigraph-core/src/graph/neo4j_backend.rs crates/infigraph-core/tests/neo4j_backend.rs
git commit -m "feat: implement write_calls_service_edges for Neo4jBackend via single UNWIND query"
```

---

### Task 3: Full verification + open the upstream PR

**Files:**
- None created; runs suites, pushes, opens PR.

- [ ] **Step 1: Full default-feature suite**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core -p infigraph-cli -p infigraph-mcp -- --test-threads=4 --no-fail-fast
```
Expected: green, modulo pre-existing catalogued flakes confirmed via stash-to-base-commit comparison for anything unexpected.

- [ ] **Step 2: `remote`-feature build (Neo4j code path compiles even without a live Neo4j to test against)**

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-core --features remote
CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-core --features remote --all-targets -- -D warnings
```

- [ ] **Step 3: fmt check**

```bash
cargo fmt --all -- --check
```

- [ ] **Step 4: Manual smoke check — the actual originally-reported symptom is gone**

Reproduce the original bug's trigger (a project whose `tests/fixtures/microservices/` — or any equivalent real dynamic-URL-containing source — gets indexed) and confirm no `"No active transaction for COMMIT"` warning appears:

```bash
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p infigraph-cli
cd tests/fixtures/microservices  # or wherever a real dynamic-URL fixture lives in this worktree
../../../target/debug/infigraph index --full 2>&1 | grep -i "no active transaction" && echo "BUG STILL PRESENT" || echo "clean"
```

Adjust the fixture path if `tests/fixtures/microservices` isn't indexable standalone (e.g. if it's several separate mini-projects) — the goal is any real codebase this session's own investigation showed triggers `detect_dynamic_urls`'s non-empty path (this repo's own `crates/` triggered it during this session's testing, so indexing this worktree itself — `cd` to the worktree root — is also a valid, simpler smoke check if the fixtures directory doesn't work standalone).

- [ ] **Step 5: Push and open the PR directly against upstream**

```bash
git push -u origin fix/calls-service-edges-transaction
gh pr create --repo intuit/infigraph --base main --head pradeepmouli:fix/calls-service-edges-transaction \
  --title "fix: CALLS_SERVICE edge writes use a real transaction instead of BEGIN/COMMIT split across fresh connections" \
  --body "\`detect_dynamic_urls\`'s CALLS_SERVICE edge writer issued \`BEGIN TRANSACTION\`/\`CREATE\`/\`COMMIT\` as three separate \`raw_query\` calls — but \`GraphBackend::raw_query\` (Kùzu) opens a fresh \`Connection\` on every call, so the transaction never actually spanned the writes. \`COMMIT\` reliably failed with 'No active transaction for COMMIT' whenever any edge was written (silently caught as a non-fatal warning, so this went unnoticed — confirmed by reproducing it against this repo's own \`crates/\` tree during investigation). Moved the write onto a new \`GraphBackend::write_calls_service_edges\` trait method that owns its own transaction/connection internally, matching every other multi-step write already in the trait (\`upsert_files_bulk\`, \`derive_tested_by_edges\`, \`resolve_calls\`). Implemented for real in both \`KuzuBackend\` (one held connection, correct BEGIN/loop/COMMIT with rollback on error) and \`Neo4jBackend\` (single UNWIND-parameterized query, auto-committed) — a no-op default would have silently dropped these edges for whichever backend didn't get a real implementation, so both ship together here. Also fixes the incidental per-edge connection-construction overhead this pattern caused (one connection for the whole batch instead of N+2)."
```

Confirm `git remote -v` output for the exact upstream/fork remote names before running (this session's convention: `upstream` = `intuit/infigraph`, `origin`/fork = `pradeepmouli/infigraph` — verify these still hold in this worktree rather than assume).

---

## Self-Review Notes

- **Spec coverage:** the original bug (transaction split across connections) fixed in Task 1 for Kùzu; the "no silent regression for remote mode" requirement satisfied by Task 2's mandatory real Neo4j implementation; the connection-overhead concern raised during investigation resolved as a natural consequence of holding one connection per batch (Task 1 Step 4) rather than as a separate change.
- **No placeholders:** Task 1/2 both explicitly instruct reading the real current file state before finalizing test scaffolding and confirming exact library API shapes (`Connection::query`'s return type, `neo4rs::Query::param`'s parameter-type bounds) rather than asserting them from memory — flagged inline everywhere this plan's own author (this session) could not independently verify the exact vendored-library signature at plan-writing time.
- **Type/interface consistency:** `CallsServiceEdge`'s four fields (`symbol_id`, `target_id`, `method`, `path`) are used identically in Task 1's Kùzu implementation, Task 1's `dynamic_urls.rs` call-site update, and Task 2's Neo4j implementation — no field renamed or reordered between tasks.
