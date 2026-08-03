# Daemon Index Work-Queue Design

**Goal:** Replace the daemon watch loop's three independent, uncoordinated triggers for graph-mutating work (periodic reindex, watch-triggered batch reindex, ad-hoc daemon-protocol requests) with one shared work queue, so overlapping work on the same file is coalesced into a single execution instead of being run twice against a moving target.

**Architecture:** A new `IndexWorkQueue` type lives in the daemon's watch loop (`crates/infigraph-core/src/watch/mod.rs`), shared (`Arc<Mutex<_>>`) between the main loop thread and a background drain task. Producers keep their own existing timing/debounce logic for *deciding when to contribute* work; the queue itself has no timer and is drained as soon as it holds anything. One unified execution replaces the six separate handler bodies it subsumes, running on its own `lbug` connection via `tokio::task::spawn_blocking` so the main loop keeps accepting new work (fsevents, periodic ticks, other write types) while a drain is in flight.

**Tech Stack:** Rust, existing `infigraph-core` watch/daemon-protocol modules, plus `tokio` (new unconditional dependency — see "Verified against lbug's concurrency model" and the Compatibility note below).

## Background

Found live, against the real installed binary, while confirming `fix/daemonkuzu-index-routing` (merged to `feat/hardening` at `e74011d`) worked end-to-end: under `INFIGRAPH_INDEX_VIA_DAEMON=1`, creating a file and immediately running `infigraph index` within the daemon's ~500ms watch-debounce window produced a Kùzu duplicate-primary-key error. Root cause, confirmed by reading `crates/infigraph-core/src/watch/mod.rs` in full: the daemon's single-threaded loop has three independent sources of graph-mutating work — periodic reindex (`watch/mod.rs:170-236`), ad-hoc daemon-protocol request-serving (`watch/mod.rs:245-289`), and watch-triggered batch reindex (`watch/mod.rs:291-364`) — and although all three already serialize their actual writes through the same `.infigraph/index.lock`, each independently computes its own "what needs indexing" work-item list from its own trigger. Nothing reconciles one source's pending work against what another source already did moments earlier, so a file that one operation just inserted can be blindly re-inserted by another, unrelated operation that had already decided (before the first operation ran) that the same file needed indexing.

Isolated and confirmed live: retrying with the watcher's own batch fully settled first (~4s wait) succeeds cleanly (`infigraph index` reports "All N files up-to-date, nothing to reindex"); retrying within the debounce window reproduces the crash. This proved the bug is a work-item coordination gap, not a locking gap — the lock genuinely serializes the two writes in time, but the second write executes a stale plan that doesn't know the first write already covered the same file.

Confirmed this is compatible with Kùzu/lbug's actual concurrency model (see next section): the coalescing fix stands regardless of what the database can or can't do internally, because the bug is an application-level planning error (a stale "this file is new" decision), not a database-level corruption race — lbug correctly *rejected* the conflicting write rather than silently corrupting anything.

## Verified against lbug's actual concurrency model

`infigraph-core` depends on `lbug` (crates.io `lbug = 0.16.0`, aliased as `kuzu`), described in its own `Cargo.toml` comment as "a maintained fork of Kuzu... matching the upstream Kuzu API surface." Rather than assume it inherits upstream Kùzu's documented behavior unchanged, both were checked directly:

- **Kùzu** ([docs.kuzudb.com/concurrency](https://kuzudb.github.io/docs/concurrency/)): "you can spawn multiple threads T1, …, Tk, and each Ti obtains a connection from `db` and concurrently issues read or write queries. This is safe" — for one `READ_WRITE` Database object, within one process. Concurrent transactions issued this way "are safely executed by Kuzu's transaction manager." Explicitly does **not** extend across separate processes/Database objects, even a `READ_ONLY` one alongside the `READ_WRITE` one — "this is not safe."
- **lbug** ([docs.ladybugdb.com/concurrency](https://docs.ladybugdb.com/concurrency)): identical guarantee, same wording, same scope, confirming the fork preserved this behavior rather than diverging.

Two consequences for this design:

1. **The coalescing logic is still required.** This guarantee is about the database not corrupting under concurrent write *attempts* — it says nothing about an application issuing a write built on stale information. The duplicate-primary-key crash was lbug's transaction manager doing exactly what it's supposed to: refusing a conflicting write rather than corrupting state. The fix for *that* is still "don't let two operations plan against different snapshots of the same file" — the queue's job, unchanged by this finding.
2. **Draining on a background thread is safe, not just theoretically fine.** The original design cut a second drain thread as YAGNI, reasoning "a second thread cannot add write parallelism" — true in the narrow sense (lbug still ultimately serializes the actual write work internally), but that reasoning didn't account for what multithreaded *connection* access actually buys: the main loop can keep a separate connection open and keep issuing its own work (accepting new fsevents, periodic ticks, serving the other 9 write types) while a drain executes on another thread's connection, and lbug's own transaction manager — not this application — is responsible for making that safe. `.infigraph/index.lock` remains necessary for the one thing this guarantee explicitly excludes: a second, fully separate process (a different Database object entirely, e.g. a bypassed `infigraph index --full`) touching the same graph.

## Relationship to the existing `index.lock` coalescing design

`docs/superpowers/specs/2026-07-20-write-safety-locks-design.md` (PR 3) is where `.infigraph/index.lock` and `begin_index_op` originate. It made a deliberate, user-approved call: an independent, cross-process caller (CLI `infigraph index`, MCP `tool_index_project`, SCIP import) that finds `index.lock` held **skips with a note rather than queuing or waiting** — "It does not queue, wait, or re-run." That behavior is already shipped (`cmd_index`'s `AlreadyRunning` handling) and this design does not change it.

This design operates one level down, entirely *inside* the daemon process's own loop, and does not extend to independent external callers. Once `DaemonKuzu` is selected, a client has no write path other than the daemon — "skip and try again later" isn't an available answer the way it is for a directly-writing CLI invocation, because the daemon already accepted responsibility for serving that request the moment it was dropped into `.infigraph/requests/`. The work queue coordinates *what the daemon itself already committed to doing*, across its own internal trigger sources, so that commitment doesn't collide with itself — a narrower problem than PR 3's cross-process operation coalescing, and compatible with it: the queue's drain still acquires `.infigraph/index.lock` exactly as every daemon-side operation does today, so an external, non-daemon-routed process (e.g. a bypassed `infigraph index --full`) still sees the same `index.lock`-based skip/wait semantics PR 3 established.

## Scope

**In scope** — all daemon-loop sources of graph-mutating "index-shaped" work, folded into one shared `IndexWorkQueue`:

1. Periodic reindex (`watch/mod.rs:170-236`)
2. Watch-triggered batch reindex — file creates/modifies (`watch/mod.rs:291-364`, today's `ChangeBatch`)
3. Watch-triggered file removal (`watch/mod.rs:386-398`) — currently handled immediately/synchronously, not through `ChangeBatch` at all, and does not call `begin_index_op` at all (a separate, pre-existing gap; closed as part of this design since removal now flows through the same queue and drain, which does acquire the lock)
4. Ad-hoc `WriteRequest::Index` (`daemon_protocol.rs`'s `serve_one_request`, both `paths: None` and `paths: Some(paths)` forms)
5. Ad-hoc `WriteRequest::UpsertFilesBulk`
6. Ad-hoc `WriteRequest::RemoveFiles`
7. Ad-hoc `WriteRequest::ResolveCalls`

**Out of scope** — the other 6 `WriteRequest` variants (`ScipImport`, `IngestStructured`, `UpsertRepo`, `DeriveTestedBy`, `UpsertSimilarEdge`, `WriteCallsServiceEdges`, `WriteCrossServiceEdges`, `UpsertDependencies`, `StoreClusters`, `StoreConfigBindings`) keep their existing immediate-execution-under-lock handling in `serve_one_request`, unchanged — still called synchronously on the main loop thread, still blocking it for their duration. These don't do file-content-driven upserts against a moving "what changed on disk" target, so they don't share the coordination gap this design closes. Now that tokio is available, decoupling these too (their own `spawn_blocking` tasks) is a natural, low-risk follow-up — genuinely out of scope here rather than a hedge: doing it now would mean redesigning `serve_one_request`'s entire dispatch shape, not just the four request types this bug actually touches.

**Now in scope, reversing an earlier cut:** draining the queue on a background task (`tokio::task::spawn_blocking`), decoupled from the main loop's event-draining. See "Verified against lbug's actual concurrency model" above — lbug's own transaction manager safely handles concurrent connections from the same in-process Database object, so this is a real responsiveness win (the loop keeps accepting new fsevents/periodic ticks/other write types while a large drain executes) rather than a synchronization risk with no payoff.

## Components

### `IndexWorkQueue` (new, `crates/infigraph-core/src/watch/queue.rs`)

```rust
enum PendingIndexItem {
    Raw(PathBuf),             // needs fresh extraction from disk
    Structured(FileExtraction), // pre-parsed by a client (UpsertFilesBulk/ResolveCalls)
}

struct Waiter {
    request_kind: WaiterKind,        // which WriteResult shape to reply with
    use_learned: bool,               // ResolveCalls waiters only
    reply: PathBuf,                  // the .result path to write via write_atomic
}

struct IndexWorkQueue {
    items: HashMap<PathBuf, PendingIndexItem>, // keyed by relative file path
    removals: HashSet<PathBuf>,
    whole_project: bool,
    waiters: Vec<Waiter>,
}
```

Held as `Arc<Mutex<IndexWorkQueue>>`, shared between the main loop thread (which locks briefly on every `add_*`/`mark_whole_project`/`add_waiter` call, then releases) and the background drain task (which locks once to `drain()` the full state, then releases immediately — the actual extraction/upsert/resolve work runs *outside* the lock, against the drained snapshot, so producers can keep adding to the *next* round while a drain executes). This is the one piece of genuinely new thread-safety surface this design introduces — see "Verified against lbug's actual concurrency model" for why the database side of this doesn't need the same treatment (lbug's own transaction manager handles concurrent connection access safely; only this plain Rust struct is this application's responsibility to protect).

- `add_raw(path)` — inserts/overwrites `Raw(path)`, evicting any existing `Structured` entry for that path. Freshness always supersedes a possibly-stale pre-parsed extraction, matching the "reopen fresh rather than trust a cached view" precedent from this session's earlier `DaemonKuzuBackend` read-staleness fix.
- `add_structured(extraction)` — inserts `Structured(extraction)` **only if no `Raw` entry already exists** for that path; otherwise a no-op (the existing `Raw` entry already supersedes it).
- `add_removal(path)` — inserts into `removals`, and drops any `Raw`/`Structured` entry for the same path (a file being removed doesn't also need indexing).
- `mark_whole_project()` — sets the flag; the drain step will do a full `prism.index()`-equivalent scan in addition to whatever's explicitly queued.
- `add_waiter(waiter)` — registers a reply target for the next drain.
- `is_empty()` — true iff no items, no removals, `!whole_project`, no waiters.
- `drain()` — returns and clears the full accumulated state in one shot (matching `ChangeBatch::drain`'s existing shape), for the loop to execute against.

No timer, no debounce logic of its own — see "Debounce ownership" below.

### Debounce ownership — unchanged producer-side timers, no queue-side timer

- The watcher's own file-watch batching (today's `ChangeBatch`) is **kept as-is** for deciding *when* to contribute watch-detected changes: it still waits out the existing quiet window since the last fsevent before flushing. The only change is what "flushing" means — instead of executing `index_files` directly under the lock, it converts its drained paths into `queue.add_raw(path)` calls. This preserves the batching benefit for bursts (e.g. `git checkout` touching hundreds of files coalesces into one bulk pass) without the queue itself needing any timing logic.
- Periodic reindex keeps its existing `periodic_secs`/`changes_since_periodic` timer, unchanged, deciding when it's due — then calls `queue.mark_whole_project()`.
- Ad-hoc requests (`Index`, `UpsertFilesBulk`, `RemoveFiles`, `ResolveCalls`) and watch-triggered removals push into the queue immediately on arrival — no waiting, since each is already a complete, ready unit of work.
- The queue is drained as soon as it holds anything and no drain is currently in flight. This is what actually fixes the bug: a waiter's work is never held behind an unrelated debounce window it didn't ask for, and a passively-accumulated watch batch that's ready to flush lands in the same drain as anything else currently pending, computed fresh at execution time rather than planned earlier and executed stale.

### Drain scheduling — background task, own connection

The main loop, on any tick where the queue is non-empty and no drain task is currently running, spawns one via `tokio::task::spawn_blocking` (lbug's calls are synchronous Rust, not native-async, so `spawn_blocking` — not a plain `tokio::spawn` — is the correct primitive: it runs the closure on tokio's blocking-thread pool rather than blocking an async-executor thread). The task: locks the queue just long enough to `drain()` it, opens its own connection from the daemon's already-open `Database` object (safe per lbug's documented multithreaded-connection guarantee), and runs the unified execution below against the drained snapshot — entirely off the main loop thread. The main loop tracks whether a drain is currently in flight (e.g. a `JoinHandle`/completion flag) so it doesn't spawn a second concurrent drain; anything producers add while one is running simply accumulates in the queue for the *next* drain, which is scheduled the moment the current one's task completes and the queue is (still) non-empty.

### Unified drain execution (replaces the three inline execution blocks + 3 daemon-protocol handler bodies)

Runs inside the drain task described above, given a drained `(items, removals, whole_project, waiters)`:

1. If `whole_project`: compute the full changed-file set the same way `Infigraph::index()` does today (scan + hash-diff against `get_file_hashes()`), and treat every changed path as an additional `Raw` entry (deduped against anything already present).
2. Split `items` into `to_extract: Vec<PathBuf>` (the `Raw` entries) and `pre_parsed: Vec<FileExtraction>` (the `Structured` entries).
3. Extract `to_extract` fresh from disk, in parallel — identical mechanism to `index_files`'s existing `.par_iter()` extraction.
4. `extractions = freshly_extracted ∪ pre_parsed`.
5. Apply `removals`: loop `backend.remove_file(path)` per path — still no bulk-remove primitive, unchanged from today's per-file behavior (a reviewer-flagged, accepted limitation from the predecessor fix).
6. If `!extractions.is_empty()`: one `backend.upsert_files_bulk(&extractions, existing_hashes_empty)` call, where `existing_hashes_empty` is computed once for this drain via `backend.get_file_hashes()`, same rule `index_files` uses today.
7. One `backend.resolve_calls(&extractions, learned)` call, where `learned` is `Some` if **any** waiter in this drain set `use_learned: true` on a `ResolveCalls` request — safe to over-include, since more resolution context never produces a wrong answer.
8. Feed the combined result into the same downstream steps the batch-flush path already runs today: embedding updates (`crate::embed::update_embeddings`) and `on_event` callbacks for cross-file-call detection.
9. Reply to every waiter with the `WriteResult` shape *its own request type* expects:
   - `Index` waiters → `WriteResult::Ok { total_files, indexed_files }`, computed from the combined run.
   - `UpsertFilesBulk` waiters → `WriteResult::Ok { total_files: extractions.len(), indexed_files: extractions.len() }`, same shape `serve_one_request` returns today.
   - `RemoveFiles` waiters → `WriteResult::Ok { total_files: removals.len(), indexed_files: removals.len() }`, using the combined `removals` set from this drain — consistent with the other three types reporting the real combined execution rather than a caller-scoped subset.
   - `ResolveCalls` waiters → `WriteResult::ResolveOk(stats)`, using the combined `resolve_stats` from step 7.
10. The whole drain executes under one `begin_index_op(root, "infigraph daemon", Duration::from_secs(30))` acquisition — same lock, same role string, same cross-process contract as today, called from within the `spawn_blocking` task (it's a blocking file-lock wait, exactly what that primitive exists for). Worth being explicit about *why* this is still needed: `index.lock`'s in-process job — stopping the daemon from running two overlapping drains against itself — is now subsumed by the queue's own "one drain in flight" tracking (a plain Rust concern, no file lock required for that part). It's kept here purely for what the queue *can't* provide: cross-process consistency against a fully separate process (e.g. a bypassed `infigraph index --full`) running concurrently — exactly the boundary lbug's own multithreaded-connection guarantee stops at ("does not extend to multiple processes"). `.infigraph/graph.lock` (the finer-grained, per-write-call corruption floor) is unaffected by any of this and continues to apply beneath both.

### Removed/replaced code

- `ChangeBatch::drain()`'s caller in `watch_project_with_periodic` no longer calls `prism.index_files` directly — it calls `queue.add_raw` for each drained path.
- The periodic-reindex block's `prism.index()` call is replaced by `queue.mark_whole_project()`.
- The request-serving block's inline `serve_one_request` dispatch for `Index`/`UpsertFilesBulk`/`RemoveFiles`/`ResolveCalls` is replaced by parsing the request and calling the matching `queue.add_*`/`add_waiter`; the other 9 `WriteRequest` variants keep calling `serve_one_request` (or an equivalent narrowed dispatcher) immediately, unchanged.
- The `WatchEventKind::Removed` match arm's direct `prism.remove_file`/`remove_files_by_prefix` calls are replaced by `queue.add_removal(path)`.
- `ChangeBatch` itself (`crates/infigraph-core/src/watch/batch.rs`) stays as the watcher's own local debounce-timing primitive (used to decide *when* to call `queue.add_raw` in bulk) — not deleted, its role narrows rather than disappearing.

## Data Flow — the bug scenario, before and after

**Before:** (1) `third.py` fsevent arrives, added to `ChangeBatch`. (2) Client submits `WriteRequest::Index` (opt-in mode). (3) Same loop tick: request-serving step runs first, calls `infigraph.index()` — a full project scan that finds and inserts `fourth.py`. (4) Batch-flush step runs next in the *same* tick: `ChangeBatch` happens to be ready, drains `[fourth.py]` (queued from an earlier tick, unaware step 3 just handled it), calls `index_files([fourth.py])`, which attempts to insert a file row that step 3 already created — duplicate primary key.

**After:** (1) `fourth.py` fsevent arrives, `queue.add_raw(fourth.py)` (via the watcher's own debounce, once its window closes) — or, if the ad-hoc request arrives first, `queue.add_waiter` for the `Index` request happens first. Either way, both land as entries against the **same shared queue key** (`fourth.py`'s path) before any execution happens. (2) The next drain (triggered immediately, since a waiter is present) pops the full state: one `Raw(fourth.py)` entry (whichever producer got there — the second `add_raw`/`add_waiter` for the same path is a no-op against an already-present entry, or simply re-affirms it) plus the `Index` waiter. (3) One extraction, one `upsert_files_bulk` call, one `resolve_calls` call. (4) The waiter gets a reply reflecting the one real execution. No second, stale-planned execution exists to collide with the first.

## Error Handling

- **Drain execution failure** (parse error, backend error): every waiter folded into that drain receives `WriteResult::Err { message }` — a real, immediate error reply rather than the 30-600s timeout a client would otherwise wait out today. Watch-triggered `Raw`/`Structured` items that were part of a failed drain are **not** re-added automatically (matching today's `index_files` failure behavior, which logs and poisons the connection rather than retrying a possibly-permanently-broken file forever) — a subsequent, unrelated fsevent for the same file (edit-and-save-again) will naturally re-queue it.
- **Lock contention** (another process holds `.infigraph/index.lock`, e.g. a bypassed `infigraph index --full`): the drain's `begin_index_op` call retries with the existing 30s budget; on exhaustion, the queue's contents are **not** cleared — they remain queued for the next tick's drain attempt, and any waiters continue waiting (bounded by their own client-side timeout, unchanged from today).
- **Daemon shutdown with pending queue state:** out of scope for this design — matches today's behavior (a `stop_rx` signal or `watch.stop` sentinel breaks the loop without draining pending work; a waiting client's request eventually times out client-side). Not a regression this design introduces.
- **Drain task panic** (new failure mode introduced by running the drain on `spawn_blocking`, not present in the old fully-synchronous design): the main loop checks the task's `JoinHandle` on completion; a panicked task is treated the same as a drain execution failure — every waiter that was folded into that drain gets `WriteResult::Err`, and the queue moves on to schedule its next drain from whatever's accumulated since. No waiter is left hanging silently.

## Compatibility

Adding `tokio` as an unconditional dependency of `infigraph-core` (today it's feature-gated behind `neo4j`/`postgres` only) is a real build-footprint cost for every local-Kuzu-only install — this workspace is already large to compile (100+ crates including native `lbug` builds), and this session independently hit repeated disk exhaustion during ordinary development on this machine. Accepted here as a deliberate tradeoff for the responsiveness win verified above, not an oversight. A minimal feature set (`rt`, `rt-multi-thread` for the blocking-thread pool, `sync` for `Mutex`) should be selected rather than `features = ["full"]`, to keep the added footprint as small as the design actually needs.

## Testing

- **Unit tests for `IndexWorkQueue`** (`crates/infigraph-core/src/watch/queue.rs`, in-module `#[cfg(test)]`): the eviction rule (`add_raw` after `add_structured` for the same path drops the structured entry; `add_structured` after `add_raw` is a no-op), `add_removal` clearing any pending index entry for the same path, `is_empty`/`drain` round-tripping, waiter accumulation across multiple `add_waiter` calls before a single drain.
- **Regression test reproducing the exact bug**, without a real spawned daemon process (unit/integration level, driving `IndexWorkQueue` + the drain function directly): simulate the exact sequence from the live repro — a `Raw` entry already queued for a path, then an `Index`-shaped waiter arriving for the same path — assert exactly one extraction/upsert occurs and both "producers'" expectations are satisfied by the single execution.
- **Real end-to-end regression test**, extending `crates/infigraph-core/tests/daemon_kuzu_e2e.rs` in the same style as `real_cli_index_against_a_real_daemon_completes_and_writes`: spawn a real `infigraph daemon`, create a file, and immediately (no settling delay) run `INFIGRAPH_INDEX_VIA_DAEMON=1 infigraph index` — assert it completes successfully rather than producing the duplicate-primary-key error. This is the test that would have caught the original bug; per this session's established pattern, verify it actually does by confirming it fails against the pre-fix code path before landing the fix (or by reverting locally and re-running, as was done for the predecessor deadlock fix).
- **Watch-triggered removal now taking the lock**: a regression test confirming `begin_index_op` is genuinely acquired during a watch-triggered file removal (e.g. via a testable hook/counter, or by asserting removal correctly contends with a concurrently-held lock rather than proceeding unlocked) — closes the pre-existing gap identified during this design's exploration.
- **Producers keep accepting work while a drain is in flight**: with a real spawned daemon, hold one drain artificially slow (e.g. a large batch, or a test-only delay hook) and assert new `add_raw`/`add_waiter` calls issued mid-drain don't block on the queue's mutex for the drain's duration, and are correctly picked up by the *next* scheduled drain once the first completes. This is the actual behavioral claim the tokio decoupling makes — worth a real test, not just an assumption from the design reasoning.
- **Drain task panic is surfaced, not silently dropped**: a regression test that forces a panic inside the drain (e.g. a deliberately malformed extraction in a test-only code path) and asserts every waiter folded into that drain receives `WriteResult::Err` rather than hanging until client-side timeout.
- Existing `daemon_kuzu_backend`, `daemon_protocol_serve`, `backend_selection`, and `watch_daemon` suites must continue passing unmodified in behavior (their assertions are about per-request outcomes, which the unified drain still produces correctly per request type).
