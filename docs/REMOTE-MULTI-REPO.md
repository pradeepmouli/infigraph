# Remote Multi-Repo Mode

Index 30+ repositories into a shared code intelligence graph using Neo4j and Postgres sidecars — all running in the same pod with zero external dependencies.

## Architecture

```
┌─── Pod ──────────────────────────────────┐
│ ┌──────────────┐  ┌──────────────────┐   │
│ │ infigraph    │  │ neo4j:5-community│   │
│ │ MCP :8090    │→ │ :7687 (Bolt)     │   │
│ │ --features   │  │ code graph +     │   │
│ │   remote     │  │ doc graph        │   │
│ └──────┬───────┘  └──────────────────┘   │
│        │          ┌──────────────────┐   │
│        └─────────→│ postgres+pgvector│   │
│                   │ :5432            │   │
│                   │ registry, sessions│  │
│                   │ embeddings        │  │
│                   └──────────────────┘   │
│  All localhost — zero network latency    │
└──────────────────────────────────────────┘
```

## How It Works

### Backend Selection

Set `INFIGRAPH_BACKEND=neo4j` to activate remote mode. The default is `daemon` (reads and writes route through this project's daemon, which is auto-started if none is listening); `kuzu` opens the embedded local graph in the calling process.

| Component | Local Mode (default) | Remote Mode (`neo4j`) |
|-----------|---------------------|----------------------|
| Code graph | Kùzu (embedded) | Neo4j (Bolt on localhost:7687) |
| Registry | `~/.infigraph/registry.json` | Postgres table `repos` |
| Groups | JSON file | Postgres tables `groups`, `group_repos` |
| Sessions | `.infigraph/sessions/` | Postgres table `sessions` |
| File hashes | Kùzu node properties | Postgres table `file_hashes` |
| Embeddings | `.infigraph/embeddings.bin` | Postgres + pgvector |
| Search | Local BM25 + HNSW + grep | Neo4j symbols + pgvector brute-force |

### Namespace Prefixing

When indexing multiple repos into a shared Neo4j database, file paths and symbol IDs are automatically prefixed with the repo name to prevent collisions:

```
# Without namespace (single repo):
src/main.rs::main

# With namespace (multi-repo):
svc-auth/src/main.rs::main
svc-gateway/src/main.rs::main
```

This happens transparently — queries return namespaced paths, and cross-repo references resolve correctly.

### Parallel Group Indexing

With Neo4j (concurrent-write safe), `group build` indexes all repos in parallel using rayon:

```
[group] parallel indexing 30 repos via Neo4j backend
```

Kùzu mode remains sequential (single-writer constraint).

## Environment Variables

| Variable | Required | Default | Purpose |
|----------|----------|---------|---------|
| `INFIGRAPH_BACKEND` | Yes | `kuzu` | Set to `neo4j` for remote mode |
| `NEO4J_URI` | Yes (remote) | `127.0.0.1:7687` | Neo4j Bolt endpoint |
| `NEO4J_USER` | Yes (remote) | — | Neo4j username |
| `NEO4J_PASSWORD` | Yes (remote) | — | Neo4j password |
| `DATABASE_URL` | Yes (remote) | — | Postgres connection string |

## Quick Start (Docker)

```bash
# 1. Start Neo4j
docker run -d --name neo4j -p 7687:7687 -p 7474:7474 \
  -e NEO4J_AUTH=neo4j/infigraph neo4j:5-community

# 2. Start Postgres + pgvector
docker run -d --name postgres -p 5432:5432 \
  -e POSTGRES_USER=infigraph \
  -e POSTGRES_PASSWORD=infigraph \
  -e POSTGRES_DB=infigraph \
  pgvector/pgvector:pg16

# 3. Build with remote features
cargo install infigraph-cli --features remote
cargo install infigraph-mcp --features remote

# 4. Set environment
export INFIGRAPH_BACKEND=neo4j
export NEO4J_URI=127.0.0.1:7687
export NEO4J_USER=neo4j
export NEO4J_PASSWORD=infigraph
export DATABASE_URL="host=localhost user=infigraph password=infigraph dbname=infigraph"

# 5. Index a single project
cd /path/to/your/repo
infigraph index

# 6. Or index multiple repos as a group
infigraph group create my-org
infigraph register /path/to/svc-auth
infigraph register /path/to/svc-gateway
infigraph group add my-org svc-auth
infigraph group add my-org svc-gateway
infigraph group build my-org
```

## Feature Flags

The remote backend is behind Cargo feature flags to keep local builds lean:

```toml
# Cargo.toml features (infigraph-core)
[features]
neo4j = ["neo4rs", "tokio"]       # Neo4j graph backend
postgres = ["tokio-postgres", "tokio"]  # Postgres metadata store
remote = ["neo4j", "postgres"]    # Both (convenience)
```

Local builds (`cargo install infigraph-cli`) have zero extra dependencies. Remote builds (`cargo install infigraph-cli --features remote`) add `neo4rs` and `tokio-postgres`.

## Resource Budget (IKS Pod)

| Container | CPU | Memory | Role |
|-----------|-----|--------|------|
| infigraph-mcp | 8 | 24Gi | Main app + indexing |
| neo4j | 2 | 4Gi | Graph DB (Bolt :7687) |
| postgres | 1 | 2Gi | Metadata + vectors (:5432) |
| mesh sidecar | 1 | 2Gi | Service mesh |
| **Total** | **12** | **32Gi** | |

## GraphBackend Trait

The `GraphBackend` trait abstracts graph storage with 24 methods covering lifecycle, read (symbol queries, traversal, aggregates), write (upsert, remove), and resolve operations:

```rust
pub trait GraphBackend: Send + Sync {
    fn stats(&self) -> Result<GraphStats>;
    fn symbols_in_file(&self, file: &str) -> Result<Vec<SymbolRow>>;
    fn find_symbol_by_id(&self, id: &str) -> Result<Option<SymbolDetail>>;
    fn callers_of(&self, symbol_id: &str) -> Result<Vec<String>>;
    fn callees_of(&self, symbol_id: &str) -> Result<Vec<String>>;
    fn raw_query(&self, query: &str) -> Result<Vec<Vec<String>>>;
    fn upsert_files_bulk(&self, extractions: &[FileExtraction], initial: bool) -> Result<()>;
    fn resolve_calls(&self, extractions: &[FileExtraction], learned: Option<&LearnedStore>) -> Result<ResolveStats>;
    // ... 16 more methods
}
```

Two implementations:
- **`KuzuBackend`** — wraps existing `GraphStore` + `GraphQuery`. Zero behavior change from pre-trait code.
- **`Neo4jBackend`** — uses `neo4rs` crate (async Bolt driver). `UNWIND` batches for bulk writes. Concurrent-write safe.

### `get_symbols_for_search()`

The `GraphBackend` trait includes `get_symbols_for_search()` — returns all symbols as 7 columns in fixed order: `[id, name, kind, file, docstring, start_line, end_line]`. The default implementation uses `raw_query` (safe for Kùzu where column order matches `RETURN` order). Neo4j overrides this with named column access because `raw_query` uses `HashMap::into_values().collect()` — which produces **random column order**.

## Search in Remote Mode

In remote mode, `search` and `semantic_search` load data from Neo4j + pgvector instead of local files:

| Data | Local source | Remote source |
|------|-------------|---------------|
| Symbol rows (BM25 index) | Kùzu `raw_query` | `Neo4jBackend::get_symbols_for_search()` |
| Embeddings (vector scores) | `.infigraph/embeddings.bin` | `PostgresMetaStore::all_embeddings("symbol")` |
| HNSW index | `.infigraph/hnsw.bin` | Not used — brute-force fallback |

**How it works:**

1. `is_remote_mode()` checks `#[cfg(feature = "remote")]` + `INFIGRAPH_BACKEND=neo4j`
2. `get_or_build_search_ctx` splits into `get_search_data_local()` / `get_search_data_remote()`
3. Remote path: `Neo4jBackend::connect_from_env()` → `get_symbols_for_search()` for BM25 rows
4. Remote path: `PostgresMetaStore::connect_from_env()` → `all_embeddings("symbol")` for vector scores
5. Vector scoring uses `brute_force_vector_scores` (no HNSW index in remote mode)
6. BM25 + vector scores fuse via existing `compute_raw_scores` / `hybrid_search`

**Brute-force is correct** for remote mode — HNSW only wins above ~200K embeddings, and symbol counts per deployment stay well below that threshold.

### Known Gaps

- **Symbol embedding write path:** No production code currently writes `kind="symbol"` embeddings to pgvector. `all_embeddings("symbol")` returns empty, so search degrades to BM25-only (keyword search still works, vector ranking does not). Needs an `update_symbol_embeddings_remote()` function analogous to `update_doc_embeddings_remote()`.
- **Project scoping:** Remote search queries all symbols in Neo4j (all repos), not filtered by `path` argument. Local mode is per-project. Acceptable for org-wide search; may need `WHERE s.file STARTS WITH $prefix` filter for single-repo queries.
- **Cache invalidation:** Uses `UNIX_EPOCH` sentinel for mtime (no local `embeddings.bin`). Cache effectively never invalidates within a session. Future: use pgvector row count or `max(updated_at)`.

## Testing

```bash
# Unit tests (no Docker needed)
cargo test -p infigraph-core --test namespace_prefix

# Integration tests (requires Docker)
# Neo4j:
docker run -d -p 7687:7687 -e NEO4J_AUTH=neo4j/testpass neo4j:5-community
NEO4J_URI=127.0.0.1:7687 NEO4J_USER=neo4j NEO4J_PASSWORD=testpass \
  cargo test -p infigraph-core --features neo4j --test neo4j_backend -- --ignored --test-threads=1

# Postgres:
docker run -d -p 5432:5432 -e POSTGRES_USER=infigraph -e POSTGRES_PASSWORD=infigraph -e POSTGRES_DB=infigraph pgvector/pgvector:pg16
DATABASE_URL="host=localhost user=infigraph password=infigraph dbname=infigraph" \
  cargo test -p infigraph-core --features postgres --test postgres_registry -- --ignored --test-threads=1
```
