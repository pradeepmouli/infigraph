# DaemonKuzu Daemon Wiring: Watcher Integration, Backend Selection, and Write Coverage

## History

`docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md` redesigned infigraph's cross-process write coordination around a single-daemon architecture (DaemonKuzu): one process (the watcher, extended) holds the sole write connection to the local embedded Kuzu database; every other process routes writes through it via a file-drop request/result protocol. `docs/superpowers/plans/2026-07-31-daemonkuzu-file-drop-protocol.md` implemented and merged (PR #49, `feat/hardening` at `9d8a9d5`) the protocol *primitives* — `WriteRequest`/`WriteResult` types, `write_atomic`, `submit_write_request`, `serve_one_request` — proven to interoperate via direct function calls and an end-to-end test, but deliberately stopping short of any watcher-loop integration, `GraphBackend` implementation, or call-site migration.

This spec covers two of the three originally-identified follow-ups: wiring the daemon loop to actually serve requests, and implementing `BackendKind::DaemonKuzu` so clients route writes through it automatically. The third follow-up ("call-site migration") turned out to be largely unnecessary on its own: backend selection in infigraph is already centralized (see `is_remote_backend()`, `crates/infigraph-core/src/watch/daemon.rs:38-42`), so once `DaemonKuzu` is a real `GraphBackend`-compatible backend, every caller going through `Infigraph`'s own methods gets it for free. What *is* real, concrete migration work is enumerated precisely in the Write Coverage Audit below.

**Revision (2026-08-01).** An independent review of this spec's first draft, verified against source, found three problems the original Write Coverage Audit missed:

1. **A class of external writes the audit's method could not see:** production code that writes raw Cypher (`CREATE`/`MERGE`/`SET`) through `GraphBackend::raw_query` — manifest dependency storage, cluster storage, config-binding storage, and cross-service edge writes. The first draft's wrapper classified `raw_query` as a read; under DaemonKuzu those writes would have gone to the read-only connection and, because several call sites discard the `Result` (`let _ =`), *silently* done nothing — exactly the partial-silent-success failure mode the Goals forbid.
2. **A real external caller of `derive_tested_by_edges`** on the main `infigraph index` path (`crates/infigraph-cli/src/index.rs:118-127`) that `trace_callers` cannot see through `&dyn` dispatch. The first draft called this method "no callers found, low risk."
3. **An inverted call-site-fix flag:** the draft required a call-site fix for `import_scip_and_cleanup` (which needs none — it obtains its backend from `prism.backend()`, so the wrapper intercepts it) while not flagging the three real `index_manifests` call sites whose writes the wrapper could not intercept at all.

This revision restructures the wrapper contract around a DB-enforced read-only read connection (so unknown `raw_query` writes fail loudly instead of silently), promotes the raw-Cypher writers to named trait methods so wrapper interception covers them with no call-site routing changes, adds the missing `DeriveTestedBy` coverage, and rebuilds the audit with grep verification of every claim.

## Goals

- The `infigraph daemon` process (renamed from `infigraph watch`) becomes a real write server: it serves file-dropped write requests using its own long-held `Infigraph` connection, alongside its existing file-watching duties.
- A new `BackendKind::DaemonKuzu` lets any local-mode process route writes through the daemon instead of opening its own embedded Kuzu connection, opt-in via the same env-var convention `INFIGRAPH_BACKEND=neo4j` already uses.
- Every currently-working write path keeps working under `DaemonKuzu` — or fails with a clear, specific error, never silently succeeds partially or reintroduces a direct-write collision. This guarantee is only as strong as the weakest call site: the backend layer returns accurate `Result`s, and the migration removes the audit-listed `let _ =` discards so failures are at minimum surfaced as logged warnings (call sites may still degrade gracefully, but never invisibly).

## Non-Goals

- No change to how reads work. Reads always go direct to embedded Kuzu, never through the daemon protocol — unchanged from the original design.
- No full `GraphBackend` trait implementation for `DaemonKuzu`. Only the write operations enumerated in the Write Coverage Audit are covered.
- No transport change. File-drop (temp file + atomic rename, already merged) stays the transport. See Alternatives Considered for why real IPC (sockets, `ipc-channel`, Arrow Flight) was reconsidered and set aside.
- No MCP tool renaming. `watch_project`, `watch_docs`, `stop_watch_docs`, `get_watch_status` keep their names — they're a stable external API surface, and "watch" is still accurate framing from a caller's perspective even though the underlying process does more now.
- No change to `.infigraph/watch.lock` / `.infigraph/watch.stop`. Their job — controlling the watching subsystem's singleton/stop behavior — stays a coherent, narrower concept than the daemon process's full identity, and renaming them would touch the largest blast-radius part of the naming change (`watcher_concurrency.rs`, `watcher_reindex.rs`, `watcher_daemon_mode.rs`, `ensure_daemon_running`, `tool_watch_project`, etc.) for no functional gain.

## Design

### Naming

The CLI subcommand `infigraph watch` is renamed to `infigraph daemon` (via LSP-driven symbol rename, not manual find/replace, to avoid missed references), reflecting that the process now serves writes in addition to watching files. No compatibility alias — this isn't yet a widely-depended-on stable CLI surface. `cmd_watch` and directly-related internal identifiers get renamed along with it; `.infigraph/watch.lock`, `.infigraph/watch.stop`, and all MCP tool names are explicitly out of scope for renaming (see Non-Goals).

### Backend selection

`BackendKind` gains a `DaemonKuzu` variant, selected opt-in via an env var following the existing `INFIGRAPH_BACKEND=neo4j` convention (exact variant name — e.g. `daemon` — is an implementation detail for the plan, not fixed here). Note `is_remote_backend()` matches `== "neo4j"` exactly (`daemon.rs:38-42`), so a new value cannot accidentally trip remote-mode checks.

Selecting `DaemonKuzu` implies daemon-mode watching is required: the backend's write path calls `ensure_daemon_running` itself if no daemon is currently detected for the project, rather than requiring `INFIGRAPH_WATCH_DAEMON=1` to be set independently. Two toggles that could disagree with each other is a footgun; one implies the other.

**Self-referential-daemon prevention is enforced process-wide, in two layers.** `ensure_daemon_running`/`spawn_daemon` re-exec the CLI binary as a detached child, and `spawn_daemon` (`daemon.rs:111-149`) builds its `Command` without clearing the environment — the child inherits `INFIGRAPH_BACKEND=daemon` from whichever process spawned it, and a daemon that routed its own writes through itself would deadlock waiting on a request nothing serves. Layer one: `spawn_daemon` calls `env_remove("INFIGRAPH_BACKEND")` on the child's `Command`, so *no* `Infigraph::open` anywhere in the daemon process can ever select `DaemonKuzu` — this matters because served request handlers may reuse helpers that open fresh `Infigraph` instances (e.g. `import_scip_and_cleanup`'s fallback path opens its own prism, `index.rs:481-494`), which a per-call-site fix would not protect. Layer two (belt-and-braces, covers a daemon started manually rather than via `spawn_daemon`): the `infigraph daemon` subcommand's own `Infigraph::open` call forces `BackendKind::Kuzu` directly, regardless of environment.

### Watcher-loop wiring

`watch_project_with_periodic` (`crates/infigraph-core/src/watch/mod.rs:90-394`) gains a `serve_requests: bool` parameter, true only when invoked from the `infigraph daemon` process — never from in-process MCP watcher threads (today's default model, where each MCP worker spawns its own watcher thread that dies with the worker; DaemonKuzu's single-writer premise only holds against the one external daemon-mode process, not a potentially-multiple in-process threads).

When `serve_requests` is true, each loop iteration — already ticking at least every 200ms via the existing `rx.recv_timeout(Duration::from_millis(200))` (`watch/mod.rs:303`) — additionally lists `.infigraph/requests/` for `.request` files and calls `serve_one_request` against the loop's existing `watch_db`/`held_prism` connection (the same lazily-opened, session-held `Infigraph` the batch-flush and periodic-refresh sections already use), gated by the same `begin_index_op` lock those sections already acquire. No `notify`-based watch on the requests directory, and no change to `ignore_dirs` (which currently excludes `.infigraph` entirely, `watch/mod.rs:111`) — a plain periodic directory listing, piggybacked on the loop's existing cadence, is sufficient given `submit_write_request`'s own poll-with-backoff starts at 10ms and only reaches 200ms after several rounds.

### Write coverage

Two classes of external writes exist (see the Write Coverage Audit for the full trace). The first already goes through `GraphBackend` trait methods, so the wrapper (next section) intercepts it with **zero call-site routing changes**. The second currently writes raw Cypher through `raw_query`; those writes are **promoted to named trait methods** in this spec, after which the wrapper intercepts them the same way — again with no call-site routing changes, because the promotion happens inside the functions that own the Cypher, not at their callers.

`WriteRequest` (already carrying `Index`) gains:

**Class 1 — existing trait-method writes:**

- **`ScipImport`** — already stubbed in the type from the prior plan (`daemon_protocol.rs:12-17`); this spec finishes the `serve_one_request` handler. Covers *both* external call sites — `import_scip_and_cleanup` (`infigraph-cli/src/index.rs:450-503`) and `tool_scip_import` (`infigraph-mcp/src/tools/index.rs:329-359`) — automatically, because both obtain their backend from `prism.backend()` (directly, or as a `&dyn GraphBackend` passed down from one). *No call-site fix is needed; the first draft claimed the opposite and was wrong.*
- **`IngestStructured { schema_id: String, source: IngestSource }`**, where `IngestSource` is `File(PathBuf) | Directory(PathBuf) | Inline(Vec<serde_json::Value>)`. The daemon calls `discover_schemas()` itself and looks up by `schema_id` — no need to serialize the schema struct into the request. `Inline` uses the sibling-file mechanism already established in the original design for bulk-data methods.
- **`UpsertRepo { namespace: String }`**
- **`WriteCallsServiceEdges { edges: Vec<CallsServiceEdge> }`** — both writers (`detect_dynamic_urls` and `detect_dynamic_urls_with_cache`, the latter called from `cmd_index` at `index.rs:187`) already batch all edges into a single call.
- **`UpsertSimilarEdge { id_a: String, id_b: String, score: f32 }`** — deliberately *not* batched. Its two call sites (`cmd_clones`, `tool_detect_clones`) call it in a loop, once per matched pair; `Neo4jBackend`'s existing implementation (`neo4j_backend.rs:1399-1413`) does the same — one network round-trip per pair, unbatched, and that's working, accepted behavior today. Matching that precedent means the clone-detection call sites need zero routing changes — the wrapper submits one `WriteRequest` per call, transparently.
- **`DeriveTestedBy { files: Option<Vec<String>> }`** — covers `derive_tested_by_edges`, called on the main `infigraph index` path (`infigraph-cli/src/index.rs:118-127`, scoped to changed files for incremental runs; `None` = full). Already a trait method, so the wrapper intercepts it; the call site already surfaces failure as a printed warning, which is acceptable under the Goals.

**Class 2 — raw-Cypher writers, promoted to trait methods (each gains a `GraphBackend` method + a variant):**

- **`UpsertDependencies { result: ManifestResult }`** ← new trait method `upsert_dependencies(&self, result: &ManifestResult)`, replacing `store_manifest`'s inline `raw_query` CREATEs/SETs (`manifest/mod.rs:729-777`). `index_manifests` stays a free function (parsing stays client-side); only the store step routes. This covers all three external callers — `index_group` (`multi/mod.rs:825`), `cmd_index_manifests` (`info_commands.rs:134-184`), `tool_index_manifests` (`infigraph-mcp/src/tools/docs.rs:676-728`) — plus `scan_csproj`, which also funnels through `store_manifest`. Small, serde-serializable payload, inline in the envelope.
- **`StoreClusters`** ← new trait method replacing `store_clusters`'s inline CREATEs (`cluster/mod.rs:251-320`), reached via `detect_clusters` from `infigraph detect-clusters` / `mcp detect_clusters`. Payload mirrors `store_clusters`'s current input (cluster id/name/description plus membership; exact struct is an implementation detail for the plan). Bounded size; inline JSON.
- **`StoreConfigBindings`** ← new trait method replacing the inline CREATEs in `detect_config_bindings` (`config/mod.rs:232`), reached from `cmd_config_bindings`, `tool_detect_config_bindings`, *and* `cmd_index` itself (`index.rs:139`). Payload mirrors the ConfigBinding fields written today (id/kind/key/value/profile/source_file plus HAS_CONFIG links). Bounded size; inline JSON.
- **`WriteCrossServiceEdges`** ← new trait method replacing the `MERGE`/`CREATE` `raw_query` writes in `link_cross_service_calls` (`multi/cross_service.rs:593-618` and `693-711`), reached via `infigraph group link` / `group_build`. Payload mirrors the fields written today (ExternalService target id/name/docstring, caller symbol id, method/path/target_service edge attributes). Batched at the call site (collect edges, one call), Arrow IPC sibling file like `WriteCallsServiceEdges`. Group-mode DaemonKuzu end-to-end behavior is flagged as an open question; this variant covers the write mechanics.

**Implementation constraint for the promoted methods:** none of them may be given a *default* trait implementation written in terms of `self.raw_query(...)` — the wrapper would inherit that default and route the writes straight back into its read-only connection, recreating the exact hole this revision closes. Each backend implements them explicitly; shared Cypher-building logic lives in free helper functions used by both `KuzuBackend` and `Neo4jBackend` impls (DRY without the trait-default trap), and the wrapper explicitly implements every one of them as a `submit_write_request`.

### `Infigraph::backend()` wrapper

For a `DaemonKuzu`-backed `Infigraph`, `backend()` returns a wrapper `GraphBackend` implementation, not a real `KuzuBackend`, with a three-tier contract:

1. **Reads** (`raw_query`, `symbols_with_docstring`, `get_file_deps`, etc.) delegate to a real, directly-opened, **read-only** Kuzu connection — reads never route through the daemon (per Non-Goals). The read-only open already exists: `KuzuBackend::open_read_only` (`kuzu_backend.rs:34`) → `GraphStore::open_read_only` (`store.rs:145-151`, `SystemConfig::default().read_only(true)`). This is not just a hygiene choice — it is the design's **safety net for the `raw_query` hole**: Kuzu rejects write statements on a read-only database at the database level, so any write Cypher that reaches `raw_query` (a call site the audit missed, or one added in the future without a trait-method promotion) fails loudly with a real error instead of silently colliding or silently no-op'ing. The implementation plan must include a regression test asserting that a `CREATE` submitted through the wrapper's `raw_query` returns `Err` (the existing read-only test at `store.rs:375` covers open-failure, not write-rejection, so this is a new assertion).
2. **The write methods enumerated in Write Coverage above** route through `submit_write_request` with the corresponding `WriteRequest` variant, translating `WriteResult` back into the method's normal return type.
3. **Any other write method** returns a clear `anyhow` error ("not supported via direct backend access under DaemonKuzu — use X instead") rather than either silently performing a real write (reintroducing the exact collision this design exists to prevent) or returning a confusing "not initialized".

This single wrapper object is why functions that interleave reads and a final write through the same `&dyn GraphBackend` — like `detect_dynamic_urls` (`dynamic_urls.rs:94-159`: two `raw_query` reads, one `write_calls_service_edges` at line 156) — work correctly without any restructuring: the reads pass through, the write gets intercepted.

### Call-site changes required

**Routing:** none. Every covered write is intercepted at the trait-method boundary (Class 1 directly; Class 2 after promotion), so no caller changes how it invokes anything.

**Error surfacing:** the following `Result` discards are removed as part of the migration, because the Goals' "never silently" guarantee dies at a `let _ =` no matter how accurate the backend's errors are:

- `store_manifest`'s three `let _ = backend.raw_query(...)` (`manifest/mod.rs:729-777`) — become propagated `Result`s naturally as the body moves into `upsert_dependencies` implementations. `scan_csproj`'s `let _ = store_manifest(...)` (`manifest/mod.rs:720`) at minimum logs a warning.
- `cmd_clones`'s and `tool_detect_clones`'s `let _ = backend.upsert_similar_edge(...)` (`analysis_commands.rs:503`; the `store_edges` block of `infigraph-mcp/src/tools/analysis/clones.rs`) — at minimum log a warning per failed pair.
- `link_cross_service_calls`'s `let _ = backend.raw_query(&create_target)` (`cross_service.rs:601`, `:701`) — subsumed by the `WriteCrossServiceEdges` promotion.

### Message encoding

The request/result envelope (`WriteRequest`/`WriteResult` themselves) stays JSON via `serde_json` — small, heterogeneous, tagged-union-shaped messages where Arrow's columnar model is a poor fit and would likely produce *larger* messages than JSON given Arrow's fixed schema/buffer/footer overhead. Genuinely tabular bulk payloads (`IngestStructured::Inline`'s data array, `WriteCallsServiceEdges`'s and `WriteCrossServiceEdges`'s edge batches) use Arrow IPC file format as sibling files — columnar efficiency is a real fit there, and `arrow` is already a direct dependency of `infigraph-core` (`Cargo.toml:38`, v58.3) so this adds no new dependency. The small bounded payloads (`UpsertDependencies`, `StoreClusters`, `StoreConfigBindings`) ride inline in the JSON envelope.

## Write Coverage Audit

Exhaustive trace of every external (CLI/MCP) call site that writes to the graph without going through `Infigraph::index()`/`index_files()`/`index_file()`. Recorded here for whoever picks up the implementation plan, so the `WriteRequest` variant list above isn't taken on faith.

**Methodology — and a real limitation found in the first draft's method.** The first draft ran `trace_callers` on each candidate trait write method plus direct source reads. That method has two demonstrated blind spots, both of which produced wrong conclusions in the first draft:

1. **`trace_callers` misses `&dyn GraphBackend` dynamic-dispatch call sites.** Evidence: it reports test-only callers for `remove_file` and `resolve_calls` despite real internal callers (`lib.rs:377/583/610`, `watch/mod.rs:325/463`), and reported *zero* callers for `derive_tested_by_edges` despite a production caller on the main `infigraph index` path (`index.rs:122`). Every "no callers" claim below is therefore additionally grep-verified (`\.method_name\(` across `crates/`).
2. **Trait-method tracing cannot see writes issued as raw Cypher through `raw_query`.** That class was found by grepping for write-statement string literals (`"CREATE `, `"MERGE `, `"DETACH DELETE `) across `crates/` and tracing each hit's enclosing function to its entry points.

**Table A — external bypasses via trait write methods (6, all covered by Class 1 variants):**

| # | Entry point(s) | File:line | Method(s) | Payload shape |
|---|---|---|---|---|
| 1 | `infigraph ingest` / `mcp ingest_structured` | `infigraph-cli/src/info_commands.rs:71-132`, `infigraph-mcp/src/tools/analysis/structured.rs:6-79` | `ingest_structured_directory/_file/_data` | file/dir: reference; inline: bulk (sibling-file) |
| 2 | `infigraph group index` / `group_build` | `infigraph-core/src/multi/mod.rs:823-840` (`index_group`'s post-index step) | `upsert_repo` (also calls `index_manifests` → Table B row 1) | reference-only |
| 3 | `infigraph index` auto-SCIP / `mcp scip_import` | `infigraph-cli/src/index.rs:450-503` (`import_scip_and_cleanup`), `infigraph-mcp/src/tools/index.rs:329-359` (`tool_scip_import`) | `import_scip_index` | reference-only; both via `prism.backend()`, wrapper-intercepted, no call-site fix |
| 4 | `infigraph detect-clones` / `mcp detect_clones` | `infigraph-cli/src/analysis_commands.rs:431-523` (loop at `:503`), `infigraph-mcp/src/tools/analysis/clones.rs` (`tool_detect_clones`) | `upsert_similar_edge`, looped once per matched pair | reference-only, unbatched (matches Neo4j precedent) |
| 5 | `infigraph detect-dynamic-urls` / `mcp detect_dynamic_urls` / `infigraph index` | `infigraph-core/src/taint/dynamic_urls.rs:94-159` (`detect_dynamic_urls`, write at `:156`) and `:161+` (`detect_dynamic_urls_with_cache`, called from `cmd_index` at `index.rs:187`) | `write_calls_service_edges`, once with the whole batch | bulk, already batched at the call sites |
| 6 | `infigraph index` (TESTED_BY derivation) | `infigraph-cli/src/index.rs:118-127` | `derive_tested_by_edges`, scoped to changed files | reference-only; found by grep only — invisible to `trace_callers` |

**Table B — external bypasses via raw Cypher through `raw_query` (4, all covered by Class 2 promotions):**

| # | Entry point(s) | Write location | What it writes | Covered by |
|---|---|---|---|---|
| 1 | `infigraph group index`, `infigraph index-manifests`, `mcp index_manifests` | `manifest/mod.rs:729-777` (`store_manifest`; callers: `multi/mod.rs:825`, `info_commands.rs:134-184`, `infigraph-mcp/src/tools/docs.rs:676-728`) | Dependency nodes + DEPENDS_ON edges; results discarded via `let _ =` | `UpsertDependencies` |
| 2 | `infigraph detect-clusters` / `mcp detect_clusters` | `cluster/mod.rs:251-320` (`store_clusters` ← `detect_clusters`) | Cluster nodes + membership | `StoreClusters` |
| 3 | `infigraph config-bindings` / `mcp detect_config_bindings` / `infigraph index` (`index.rs:139`) | `config/mod.rs:232` (inline in `detect_config_bindings`) | ConfigBinding nodes + HAS_CONFIG edges | `StoreConfigBindings` |
| 4 | `infigraph group link` / `group_build` | `multi/cross_service.rs:593-618`, `:693-711` (`link_cross_service_calls`) | ExternalService Symbol nodes (MERGE) + CALLS_SERVICE edges; node-create results discarded via `let _ =` | `WriteCrossServiceEdges` |

**Checked, ruled out (internal-only; each grep-verified, not trusted to `trace_callers` alone):**

- `remove_file` — callers: `index_via_backend` (`lib.rs:377`, runs server-side as part of `index()`/`index_files()`), `Infigraph::remove_file` (`lib.rs:583`) and `Infigraph::remove_files_by_prefix` (`lib.rs:588-614`, fans out to `backend.remove_file` at `:610` — note this is an `Infigraph` method, not a trait method), whose only production callers are the watcher's file/directory-deletion handling (`watch/mod.rs:325`, `:327`). Always server-side, direct `BackendKind::Kuzu`.
- `resolve_calls`/`re_resolve_for_files` — callers: index paths (`lib.rs:387/546`), `watch_project_auto_resolve` (`watch/mod.rs:463`), and `Neo4jBackend`'s own re-resolve (`neo4j_backend.rs:2155`). All internal/watcher-side.
- `clear_all_data` — default no-op trait impl (`backend.rs:124-126`).
- `upsert_file` — grep finds only tests, `GraphStore`-level (non-trait) callers, and a benchmark binary (`cozo_vs_kuzu.rs`). *(Promoted from the first draft's "not fully verified" to verified.)*
- `upsert_files_bulk` — grep finds only test callers (`tests/kuzu_backend.rs`, `tests/neo4j_backend.rs`). *(Was untraced in the first draft; now verified.)*
- The `concerns`/`taint`/`reflect`/`routes` analyzer modules — contain no `raw_query` calls at all and appear in no write-method caller list; they are read-only against the backend. `cmd_index` invokes them inline with warn-and-continue degradation, which keeps working under DaemonKuzu since reads work.

**Residual risk:** a future `raw_query` write added without a trait-method promotion. Structurally mitigated: the wrapper's read-only connection makes any such write fail loudly under DaemonKuzu (see the wrapper section) rather than silently colliding — the failure mode is a visible bug report, not corruption.

## Alternatives Considered

**Real IPC (raw Unix domain socket, `std::os::unix::net`, no new dependency).** Given the now-finalized message shapes are small and bounded, a real request/response socket would eliminate `submit_write_request`'s poll-with-backoff loop and the documented "orphaned result files accumulate in the staging directory" gap entirely. Rejected for this spec: no first-class named-pipe support in Rust's `std` on Windows (would need a crate or raw FFI, undermining the "no new dependency" win), and it would mean reworking the already-merged, tested file-drop primitives from PR #49 rather than extending them. Left as an explicit future consideration if polling latency or orphaned-result-file accumulation turn out to matter in practice — a separate, deliberate transport-replacement decision, not bundled into this spec.

**`ipc-channel` crate.** Rejected for the same reason real IPC was rejected in the original 2026-07-31 design: it's built for high-throughput, low-latency shared-memory workloads (browser-process-style), which doesn't match this design's low-frequency, small-message traffic. Would be a genuinely new dependency for no real gain here.

**Arrow Flight as full transport.** Arrow's core crate (already a dependency) only provides the IPC *serialization format*, transport-agnostic — it doesn't give real socket/RPC transport by itself. `arrow-flight` (gRPC-based, cross-platform including Windows, would actually solve the Windows gap the raw-socket option has) is not a current dependency and pulls in a real gRPC stack (`tonic` + `prost` + `arrow-flight`) — a meaningfully heavier new-dependency cost than the already-rejected `ipc-channel` option. Rejected for the same reason.

**Arrow Plasma (shared memory) via `rust-plasma`.** Not part of official `arrow-rs` at all — Plasma is a C++/Python-native Arrow component; the only Rust binding is an unofficial third-party crate maintained under the now-defunct Meta/Diem project's GitHub org. Would be a new dependency with real maintenance risk, for no benefit over the file-drop mechanism already proven working.

**Batching `UpsertSimilarEdge`.** Considered given the file-drop round-trip cost, but `Neo4jBackend::upsert_similar_edge`'s existing implementation is *also* unbatched (one Cypher `MERGE` per call) and this is working, accepted behavior in production today. An upstream issue (`intuit/infigraph#34`) that superficially looked like supporting evidence for a chattiness problem was read in full — its own investigation explicitly measured and ruled out network-round-trip count as the cause (250ms total edge-write time out of a 25s step; the real bottleneck was unrelated, redundant CPU-bound language-registry rebuilding). No evidence found supporting batching this specific method; matching Neo4j's existing precedent means zero changes needed to the clone-detection call sites.

**Classifying `raw_query` calls by parsing the Cypher string.** The obvious way to handle the raw-Cypher-write class inside the wrapper — sniff the query text for `CREATE`/`MERGE`/`SET`/`DELETE` and route matches through the daemon. Rejected as fragile in both directions: a false negative (a write formulated in syntax the blocklist doesn't recognize — `COPY FROM`, `CALL` procedures, future Cypher surface) silently reintroduces the exact direct-write collision this design exists to prevent, and a false positive breaks a legitimate read (e.g. a `MATCH` whose string content happens to contain a keyword). Promoting the known writers to named trait methods plus DB-enforced read-only rejection of everything else gives the same guarantee with zero parsing, enforced by the database rather than by a regex.

## Open Questions

- Exact env var name/value for selecting `BackendKind::DaemonKuzu` (implementation detail for the plan).
- Group-mode DaemonKuzu end-to-end: `WriteCrossServiceEdges` covers the write mechanics of `group link`, but multi-repo daemon fan-out (one daemon per member root, `ensure_daemon_running` per repo, interaction with parallel member indexing) has not been validated end-to-end and may warrant its own follow-up.
- Kuzu read-only write-rejection semantics: the wrapper's safety net assumes a write statement through a read-only connection returns an error. The existing test (`store.rs:375`) covers open-failure only; the plan must add a test asserting `CREATE`-via-read-only-`raw_query` returns `Err`.
- The orphaned-result-file cleanup gap (documented in the merged protocol-primitives plan's `serve_one_request`) is still unaddressed by this spec — remains a follow-up for whoever wires this into a long-running production daemon.
