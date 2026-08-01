# DaemonKuzu Daemon Wiring: Watcher Integration, Backend Selection, and Write Coverage

## History

`docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md` redesigned infigraph's cross-process write coordination around a single-daemon architecture (DaemonKuzu): one process (the watcher, extended) holds the sole write connection to the local embedded Kuzu database; every other process routes writes through it via a file-drop request/result protocol. `docs/superpowers/plans/2026-07-31-daemonkuzu-file-drop-protocol.md` implemented and merged (PR #49, `feat/hardening` at `9d8a9d5`) the protocol *primitives* — `WriteRequest`/`WriteResult` types, `write_atomic`, `submit_write_request`, `serve_one_request` — proven to interoperate via direct function calls and an end-to-end test, but deliberately stopping short of any watcher-loop integration, `GraphBackend` implementation, or call-site migration.

This spec covers two of the three originally-identified follow-ups: wiring the daemon loop to actually serve requests, and implementing `BackendKind::DaemonKuzu` so clients route writes through it automatically. The third follow-up ("call-site migration") turned out to be largely unnecessary on its own: backend selection in infigraph is already centralized (see `is_remote_backend()`, `crates/infigraph-core/src/watch/daemon.rs:38-42`), so once `DaemonKuzu` is a real `GraphBackend`-compatible backend, every caller going through `Infigraph`'s own methods gets it for free. What *is* real, concrete migration work is enumerated precisely in the Write Coverage Audit below — a handful of call sites that bypass `Infigraph`'s methods and call `.backend()` directly.

## Goals

- The `infigraph daemon` process (renamed from `infigraph watch`) becomes a real write server: it serves file-dropped write requests using its own long-held `Infigraph` connection, alongside its existing file-watching duties.
- A new `BackendKind::DaemonKuzu` lets any local-mode process route writes through the daemon instead of opening its own embedded Kuzu connection, opt-in via the same env-var convention `INFIGRAPH_BACKEND=neo4j` already uses.
- Every currently-working write path keeps working under `DaemonKuzu` — or fails with a clear, specific error, never silently succeeds partially or reintroduces a direct-write collision.

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

`BackendKind` gains a `DaemonKuzu` variant, selected opt-in via an env var following the existing `INFIGRAPH_BACKEND=neo4j` convention (exact variant name — e.g. `daemon` — is an implementation detail for the plan, not fixed here).

Selecting `DaemonKuzu` implies daemon-mode watching is required: the backend's write path calls `ensure_daemon_running` itself if no daemon is currently detected for the project, rather than requiring `INFIGRAPH_WATCH_DAEMON=1` to be set independently. Two toggles that could disagree with each other is a footgun; one implies the other.

The `infigraph daemon` subcommand's own `Infigraph::open` call always forces `BackendKind::Kuzu` directly, regardless of what `INFIGRAPH_BACKEND` is set to in its environment. This is correct by construction: `ensure_daemon_running`/`spawn_daemon` re-exec the CLI binary as a detached child, which would otherwise inherit `INFIGRAPH_BACKEND=daemon` from whichever process spawned it — and a daemon that routed its own writes through itself would deadlock waiting on a request nothing serves.

### Watcher-loop wiring

`watch_project_with_periodic` (`crates/infigraph-core/src/watch/mod.rs:90-394`) gains a `serve_requests: bool` parameter, true only when invoked from the `infigraph daemon` process — never from in-process MCP watcher threads (today's default model, where each MCP worker spawns its own watcher thread that dies with the worker; DaemonKuzu's single-writer premise only holds against the one external daemon-mode process, not a potentially-multiple in-process threads).

When `serve_requests` is true, each loop iteration — already ticking at least every 200ms via the existing `rx.recv_timeout(Duration::from_millis(200))` — additionally lists `.infigraph/requests/` for `.request` files and calls `serve_one_request` against the loop's existing `watch_db`/`held_prism` connection (the same lazily-opened, session-held `Infigraph` the batch-flush and periodic-refresh sections already use), gated by the same `begin_index_op` lock those sections already acquire. No `notify`-based watch on the requests directory, and no change to `ignore_dirs` (which currently excludes `.infigraph` entirely) — a plain periodic directory listing, piggybacked on the loop's existing cadence, is sufficient given `submit_write_request`'s own poll-with-backoff starts at 10ms and only reaches 200ms after several rounds.

### Write coverage

`WriteRequest` (already carrying `Index`) gains:

- **`ScipImport`** — already stubbed in the type from the prior plan; this spec finishes the `serve_one_request` handler.
- **`IngestStructured { schema_id: String, source: IngestSource }`**, where `IngestSource` is `File(PathBuf) | Directory(PathBuf) | Inline(Vec<serde_json::Value>)`. The daemon calls `discover_schemas()` itself and looks up by `schema_id` — no need to serialize the schema struct into the request. `Inline` uses the sibling-file mechanism already established in the original design for bulk-data methods (the array is written to a file next to the request; the request carries the path).
- **`IndexManifests`** — reference-only; the daemon already knows its own root.
- **`UpsertRepo { namespace: String }`**
- **`WriteCallsServiceEdges { edges: Vec<CallsServiceEdge> }`** — matches its one existing call site, which already batches all edges into a single call.
- **`UpsertSimilarEdge { id_a: String, id_b: String, score: f32 }`** — deliberately *not* batched. Its one call site (`cmd_clones`) calls it in a loop, once per matched pair; `Neo4jBackend`'s existing implementation (`neo4j_backend.rs:1399-1413`) does the same — one network round-trip per pair, unbatched, and that's working, accepted behavior today. Matching that precedent means `cmd_clones`/`tool_detect_clones` need zero changes — the wrapper (below) submits one `WriteRequest` per call, transparently.

### `Infigraph::backend()` wrapper

For a `DaemonKuzu`-backed `Infigraph`, `backend()` returns a wrapper `GraphBackend` implementation, not a real `KuzuBackend`:

- Every read method (`raw_query`, `symbols_with_docstring`, `get_file_deps`, etc.) delegates to a real, directly-opened, read-only Kuzu connection — reads never route through the daemon (per Non-Goals).
- The six write methods above route through `submit_write_request` with the corresponding `WriteRequest` variant, translating `WriteResult` back into the method's normal return type.
- Any other write method call returns a clear `anyhow` error ("not supported via direct backend access under DaemonKuzu — use X instead") rather than either silently performing a real write (reintroducing the exact collision this design exists to prevent) or returning a confusing "not initialized".

This single wrapper object is why functions that interleave reads and a final write through the same `&dyn GraphBackend` — like `detect_dynamic_urls` — work correctly without any restructuring: the reads pass through, the write gets intercepted.

### Known call-site fix required

`infigraph index`'s automatic SCIP-enrichment step (`import_scip_and_cleanup`, `crates/infigraph-cli/src/index.rs:450-503`) calls `backend.import_scip_index(&scip_out, Some(root))` directly on the raw backend object — a *different* code path from `WriteRequest::ScipImport`. Finishing `ScipImport`'s server-side handler does not, by itself, make this call site use it. This call site needs its own fix: replace the direct `backend.import_scip_index` call with `submit_write_request(ScipImport)`.

### Message encoding

The request/result envelope (`WriteRequest`/`WriteResult` themselves) stays JSON via `serde_json` — small, heterogeneous, tagged-union-shaped messages where Arrow's columnar model is a poor fit and would likely produce *larger* messages than JSON given Arrow's fixed schema/buffer/footer overhead. The bulk sibling-file payloads (`IngestStructured::Inline`'s data array, `WriteCallsServiceEdges`'s edge batch) use Arrow IPC file format instead of a raw JSON array — genuinely tabular data, where Arrow's columnar efficiency is a real fit, and `arrow` is already a direct dependency of `infigraph-core` (`Cargo.toml:38`, v58.3) so this adds no new dependency.

## Write Coverage Audit

Exhaustive trace of every external (CLI/MCP) call site that invokes a `GraphBackend` write method without going through `Infigraph::index()`/`index_files()`/`index_file()`, performed via `trace_callers` on each candidate trait method plus direct source reads of every caller found. Recorded here for whoever picks up the implementation plan, so the `WriteRequest` variant list above isn't taken on faith.

**Confirmed external bypasses (5 call sites, all addressed above — row 2 maps to two `WriteRequest` variants, `IndexManifests` and `UpsertRepo`; row 3 needs no new variant, just a call-site fix to use the existing `ScipImport`):**

| # | Entry point | File:line | Method(s) | Payload shape |
|---|---|---|---|---|
| 1 | `infigraph ingest` / `mcp__infigraph__ingest_structured` | `infigraph-cli/src/info_commands.rs:71-132`, `infigraph-mcp/src/tools/analysis/structured.rs:6-79` | `ingest_structured_directory/_file/_data` | file/dir: reference; inline: bulk (sibling-file) |
| 2 | `infigraph group index` / `group_build` | `infigraph-core/src/multi/mod.rs:~805-820` (`index_group`'s post-index step) | `manifest::index_manifests`, `backend.upsert_repo` | reference-only |
| 3 | `infigraph index`'s auto-SCIP-enrichment | `infigraph-cli/src/index.rs:450-503` (`import_scip_and_cleanup`) | `backend.import_scip_index` | reference-only — needs its own call-site fix, see above |
| 4 | `infigraph detect-clones` / `mcp__infigraph__detect_clones` | `infigraph-cli/src/analysis_commands.rs:431-523` (`cmd_clones`) | `upsert_similar_edge`, looped once per matched pair | reference-only, unbatched (matches Neo4j precedent) |
| 5 | `infigraph detect-dynamic-urls` / `mcp__infigraph__detect_dynamic_urls` | `infigraph-core/src/taint/dynamic_urls.rs:94-159` (`detect_dynamic_urls`) | `write_calls_service_edges`, called once with the whole batch | bulk, already batched at the call site |

**Checked, ruled out (internal-only, no external bypass):**

- `remove_file`/`remove_files_by_prefix` — only called by `index_via_backend` (runs server-side, as part of `index()`/`index_files()`) and the watcher's own file-deletion handling (always server-side, direct `BackendKind::Kuzu`).
- `resolve_calls`/`re_resolve_for_files` — only called by `resolve_calls_incremental` and `watch_project_auto_resolve`, both internal/watcher-side.
- `clear_all_data` — no-op by design for local/Kuzu backends regardless.

**Not fully verified — genuine residual gap:**

- `upsert_file` and `derive_tested_by_edges` (trait-level declarations in `graph/backend.rs`) show **no callers found** via `trace_callers`. This may mean genuinely unreachable through that exact trait signature, or that static call-graph resolution can't fully trace calls through `&dyn GraphBackend` dynamic dispatch for these two specifically — not confirmed either way. Low risk regardless: the backend() wrapper's default (error loudly on any uncovered write) means if either of these turns out to have a real external caller after all, it fails safely rather than silently colliding.
- `upsert_files_bulk` was not traced directly (its trait-level `backend.rs` declaration symbol wasn't cleanly located during the audit); implementations exist in both `kuzu_backend.rs` and `neo4j_backend.rs`. Worth a direct check during implementation before assuming it's internal-only like `upsert_file`.

## Alternatives Considered

**Real IPC (raw Unix domain socket, `std::os::unix::net`, no new dependency).** Given the now-finalized message shapes are small and bounded, a real request/response socket would eliminate `submit_write_request`'s poll-with-backoff loop and the documented "orphaned result files accumulate in the staging directory" gap entirely. Rejected for this spec: no first-class named-pipe support in Rust's `std` on Windows (would need a crate or raw FFI, undermining the "no new dependency" win), and it would mean reworking the already-merged, tested file-drop primitives from PR #49 rather than extending them. Left as an explicit future consideration if polling latency or orphaned-result-file accumulation turn out to matter in practice — a separate, deliberate transport-replacement decision, not bundled into this spec.

**`ipc-channel` crate.** Rejected for the same reason real IPC was rejected in the original 2026-07-31 design: it's built for high-throughput, low-latency shared-memory workloads (browser-process-style), which doesn't match this design's low-frequency, small-message traffic. Would be a genuinely new dependency for no real gain here.

**Arrow Flight as full transport.** Arrow's core crate (already a dependency) only provides the IPC *serialization format*, transport-agnostic — it doesn't give real socket/RPC transport by itself. `arrow-flight` (gRPC-based, cross-platform including Windows, would actually solve the Windows gap the raw-socket option has) is not a current dependency and pulls in a real gRPC stack (`tonic` + `prost` + `arrow-flight`) — a meaningfully heavier new-dependency cost than the already-rejected `ipc-channel` option. Rejected for the same reason.

**Arrow Plasma (shared memory) via `rust-plasma`.** Not part of official `arrow-rs` at all — Plasma is a C++/Python-native Arrow component; the only Rust binding is an unofficial third-party crate maintained under the now-defunct Meta/Diem project's GitHub org. Would be a new dependency with real maintenance risk, for no benefit over the file-drop mechanism already proven working.

**Batching `UpsertSimilarEdge`.** Considered given the file-drop round-trip cost, but `Neo4jBackend::upsert_similar_edge`'s existing implementation is *also* unbatched (one Cypher `MERGE` per call) and this is working, accepted behavior in production today. An upstream issue (`intuit/infigraph#34`) that superficially looked like supporting evidence for a chattiness problem was read in full — its own investigation explicitly measured and ruled out network-round-trip count as the cause (250ms total edge-write time out of a 25s step; the real bottleneck was unrelated, redundant CPU-bound language-registry rebuilding). No evidence found supporting batching this specific method; matching Neo4j's existing precedent means zero changes needed to `cmd_clones`.

## Open Questions

- Exact env var name/value for selecting `BackendKind::DaemonKuzu` (implementation detail for the plan).
- `upsert_files_bulk`'s external reachability, per the Write Coverage Audit's residual gap — worth a direct check at implementation time.
- The orphaned-result-file cleanup gap (documented in the merged protocol-primitives plan's `serve_one_request`) is still unaddressed by this spec — remains a follow-up for whoever wires this into a long-running production daemon.
