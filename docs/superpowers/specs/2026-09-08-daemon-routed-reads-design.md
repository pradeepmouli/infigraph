# Daemon-Routed Reads Design

**Goal:** Stop every process except the daemon from opening the graph and document
stores. Reads move to a socket-based read service inside the daemon; the write
pipeline is untouched.

## History

`docs/superpowers/specs/2026-08-01-daemonkuzu-daemon-wiring-design.md` routed
*writes* through the daemon and explicitly excluded reads:

> **Non-Goals** — No change to how reads work. Reads always go direct to embedded
> Kuzu, never through the daemon protocol.

> **Non-Goals** — No transport change. File-drop (temp file + atomic rename,
> already merged) stays the transport.

It also named the condition for revisiting the transport:

> Left as an explicit future consideration if polling latency or orphaned-result-file
> accumulation turn out to matter in practice — a separate, deliberate
> transport-replacement decision, not bundled into this spec.

This document is that separate decision. It does not overturn the earlier one: every
alternative rejected there was rejected against *write* traffic — low-frequency,
small, bounded messages. Reads invert each of those premises, most explicitly
`ipc-channel`, rejected because it is "built for high-throughput, low-latency
shared-memory workloads, which doesn't match this design's low-frequency,
small-message traffic." The query API is that workload.

## Motivation

Every graph-integrity bug fixed in the last five weeks has the same shape: a second
process opened a file the first process owned, and then had to *infer* from
filesystem leftovers whether the file was damaged.

Re-probing every quarantined image on this machine on 2026-09-07, out of process via
`INFIGRAPH_PROBE_GRAPH_PATH`, with and without each image's WAL family:

| class | count | reality |
|---|---|---|
| torn WAL | 7 | intact base image under a torn tail; recoverable (2bdfeab) |
| already healthy | 5 | opened exactly as they stood |
| genuinely corrupt | **0** | — |

Twelve images, eight repositories, five weeks of lost indexes. None of them was
corrupt. Each was destroyed by a corruption verdict that an external opener derived
from its own failed open.

The machinery built to contain that is itself evidence: `classify_read_only_open_failure`
(busy vs. corrupt vs. WAL-vanished), `live_graph_writer`'s `pids_holding_file`
fallback, `unclean_shutdown_wal_holder`, `quarantine::WalRecovery`, and an
out-of-process `probe::graph_opens` that exists because a damaged image can SIGBUS
the process asking about it. All of it is machinery for guessing at another process's
state. None of it is needed by a process that holds the file itself.

The cost is also ongoing, not just historical. On 2026-09-07 the sittir daemon sat
idle holding a 6.6 MB uncheckpointed WAL; every *new* external read-only open was
refused for ~45 minutes (issue #149) while a reader that already held the file was
served normally. `daemon-restart` checkpointed the WAL and reads resumed against a
byte-identical graph.

## Goals

- No process other than the daemon opens `graph` or `docs.kuzu` on the default path.
- Reads are concurrent, unlocked, and not coupled to the file-watching loop.
- Read-only is enforced by the database, not by inspecting query text.
- The wire format admits a shared-memory implementation later without a protocol change.

## Non-Goals

- **No change to the write pipeline.** `route_or_serve_request`
  (`crates/infigraph-core/src/daemon/mod.rs:2111-2365`), the index work queue, the
  file-drop transport, `begin_index_op` and `serve_one_request`
  (`crates/infigraph-core/src/daemon_protocol.rs:660`) are untouched. Writes are
  low-frequency and bounded, and the existing transport is proven; changing it here
  would rework the tested file-drop primitives for no gain, which is one of the
  reasons the 2026-08-01 spec declined a transport change.
- **No shared-memory implementation.** The response is a byte stream. Substituting a
  handle later does not change `ReadRequest`. Building it now is unjustified.
- **No removal of corruption handling.** A genuinely torn image still needs a verdict.
  What changes is that only one process is entitled to make it.
- **No MCP tool renaming or signature changes.** This is a transport and ownership
  change beneath a stable API surface.

## Design

### Two pipelines, one process

```
infigraph daemon
├── write coordinator                                  UNCHANGED
│     watch loop (200ms recv_timeout)
│       └─ route_or_serve_request
│            ├─ enqueue → index work queue (coalescing, Waiter)
│            └─ serve_one_request → WriteResult
│     file-drop transport; begin_index_op / index.lock
│
└── read service                                       NEW
      socket listener → worker pool
        └─ Connection (from pool, over the daemon's existing Database)
      no lock, no queue, no watch-loop coupling
```

The write pipeline is a *coalescing* pipeline: queue, dedupe, serialize, lock. Every
layer exists to make many write requests behave like fewer. Reads want fan-out.
Reusing any of those layers would mean disabling each one's purpose in turn, so the
read service shares the process and the `Database` handle and nothing else.

### Endpoint and portability

`interprocess` local sockets: Unix domain socket on Unix, named pipe on Windows,
behind one API. Rust's `std` has no first-class named-pipe support, which is why the
2026-08-01 spec's "raw UDS, no new dependency" option was rejected on Windows
grounds; a single small crate resolves that.

The endpoint name is derived from a hash of the canonical project root and placed in
a short OS temp directory — **not** under `.infigraph/`. macOS caps `sun_path` at
about 104 bytes, and a socket inside `.infigraph/` for a nested worktree already
measures 94 bytes:

```
/Users/<user>/.../infigraph/scratchpad/wt-example/.infigraph/daemon.sock   94 bytes
```

The `scratchpad/wt-*` worktree convention makes exceeding the limit routine, and the
failure would be an obscure bind error. The endpoint identifier is therefore an
opaque type from the outset, never a `PathBuf` — Windows named pipes are not
filesystem paths either.

A stale Unix socket file surviving a crash is resolved by unlink-and-rebind, using
the existing `lockfile` identity and PID-reuse machinery to decide ownership. No
second ownership protocol.

### Wire API

One request type, not one per tool:

```rust
struct ReadRequest {
    store: Store,          // Graph | Docs
    query: String,
    params: Vec<(String, Value)>,
    chunk_size: usize,
}
```

A variant per read operation would couple the protocol to every tool's signature and
make each new tool a protocol change; the MCP surface is ~98 tools. Routing at the
`GraphBackend` trait instead would be smaller but still grows whenever the trait
grows. Typed accessors stay client-side and serialize down to this.

### Read-only enforcement

The daemon holds a read-write `Database`, so the read service must not become a write
backdoor. It is enforced by the database's own parser:

```rust
let stmt = conn.prepare(&req.query)?;
if !stmt.is_read_only() { return Err(NotAReadQuery); }
```

`PreparedStatement::is_read_only` (`lbug-0.20.2/src/connection.rs:56`, backed by
`ffi::prepared_statement_is_read_only`) is exactly the mechanism the 2026-08-01 spec
chose over classifying Cypher by string, which it rejected as fragile in both
directions — a false negative silently reintroduces a direct write, a false positive
breaks a legitimate `MATCH` whose text happens to contain a keyword. Nothing here
parses the query.

### Execution and results

`Connection::query_as_arrow(&self, query, chunk_size)`
(`lbug-0.20.2/src/connection.rs:205`) returns chunked Arrow natively; results go back
as an Arrow IPC stream. `arrow 58.3` and `memmap2 0.9` are already dependencies of
`infigraph-core`, so the data path needs no new crates.

Connections come from a small fixed pool over the daemon's single `Database`. This is
the mechanism by which reads stop multiplying buffer pools: today every reader opens
its own Kuzu with `READ_ONLY_BUFFER_POOL_BYTES` (256 MB), so eight readers hold eight
cold pools and eight copies of hot pages. One pool, shared and warm, replaces that —
and `READ_ONLY_BUFFER_POOL_BYTES` and the multi-process pool arithmetic behind the
39 GB incident both go away.

### Client integration

`DaemonKuzuBackend`'s read methods stop passing through to a local read-only open and
serialize to `ReadRequest` instead. The document store gets the equivalent treatment:
`search` with `scope='all'` touches both stores in one call, so routing only the graph
would leave the flagship read tool still opening `docs.kuzu` directly. `docs.kuzu`
has its own lock file and its own wipe-on-any-open-failure history (#143, the same
R3.1.1 class as #140).

### Escape hatch

Reads are daemon-mandatory. If no daemon is running, the client calls the existing
`ensure_daemon_running` — the same implication the 2026-08-01 spec established for
writes ("Selecting `DaemonKuzu` implies daemon-mode watching is required… Two toggles
that could disagree with each other is a footgun; one implies the other").

If the daemon still cannot start, an explicit environment variable restores the direct
read-only open. This exists so a graph is recoverable when the daemon itself is
broken — disk full, endpoint unavailable, crash loop. It is deliberately explicit
rather than an automatic fallback: a silent fallback would keep both paths permanently
live, which is what allowed `Infigraph::init` and
`GraphStore::open_read_only_or_degrade` to drift apart until one of them quarantined
healthy graphs (5818aa1).

## Error handling

- **Daemon absent** → auto-start; if still absent, a hard error naming the escape hatch.
- **Statement not read-only** → distinct refusal. This is a caller bug, not user error,
  and must not be reported as a query failure.
- **Daemon dies mid-stream** → the client must distinguish a truncated stream from an
  empty result set. Returning zero rows silently is the "0-symbol graph served as
  healthy" failure `Infigraph::init` already shipped once; an incomplete Arrow stream
  is an error, never a successful empty answer.
- **Stale endpoint** → unlink-and-rebind via existing lockfile identity.

## Testing

- A `CREATE` issued through the read path is refused, and the test asserts the refusal
  comes from `is_read_only`, not from a string check.
- N simultaneous reads are served in parallel *while* an index operation holds
  `index.lock` — proving reads neither take it nor queue behind it.
- A result larger than one chunk arrives complete.
- Killing the daemon mid-stream surfaces an error; a test asserts it is not reported as
  an empty result.
- The escape hatch produces a working read when no daemon can start.
- Endpoint naming is exercised at a path length that would overflow `sun_path` if the
  socket lived under `.infigraph/`.

## Sequencing

The read service ships with its own socket transport from the start; writes stay on
file-drop.

The alternative considered first was routing reads over the existing file-drop
transport and swapping the transport afterwards. It was rejected: read requests would
land in the same staging directory the watch loop lists, which is the write pipeline,
so avoiding intermingling would require either a second staging directory with its own
polling loop, orphan cleanup and naming conventions, or discrimination inside
`route_or_serve_request`. Either grows a transport already known to be wrong, to
delete it one phase later — and the 2026-08-01 spec's known "orphaned result files
accumulate in the staging directory" gap scales with request volume, which reads
multiply.

Sequencing the transport first instead (sockets for existing writes, then reads) was
also rejected: it delays the fix for the failure mode that actually destroys indexes,
and it reworks tested write primitives, which the earlier spec declined to do for good
reason.

The accepted risk is that reads are the socket transport's first traffic rather than
its second.

## What this retires

Once no process but the daemon opens the stores:

- **#149** — an idle daemon's uncheckpointed WAL blocking external readers; there are
  no external readers.
- `classify_read_only_open_failure` and its busy / corrupt / WAL-vanished taxonomy.
- `live_graph_writer` and its `pids_holding_file` fallback.
- The dead-holder read path, `unclean_shutdown_wal_holder`'s read-side use, and
  `degrade_or_refuse`.
- `probe::graph_opens` as a load-bearing safety mechanism.
- `READ_ONLY_BUFFER_POOL_BYTES` and multi-process pool arithmetic.
- **#124** — PID-liveness polling; a socket connection *is* client liveness, and MCP
  clients read constantly and write rarely, so read connections track real client
  presence better than write traffic could.

Quarantine, `WalRecovery` and the recovery paths remain, serving the one process
entitled to a verdict and the escape hatch.

## Alternatives Considered

**Extending `serve_one_request`.** Rejected. It matches `WriteRequest` and returns
`WriteResult` — wrong input and wrong output — and is already complexity 82 across 110
statements. Adding read variants would make the single largest match in the codebase
unreviewable, and reads would inherit the watch loop's 200 ms cadence and
`begin_index_op` gating.

**A request variant per read operation.** Maximum type safety, but ~98 MCP tools means
a very large enum and a protocol change per new tool.

**Routing at the `GraphBackend` trait, method per message.** Smaller than per-tool, and
`DaemonKuzuBackend` already exists as the wrapper, but the trait's read surface is not
small and the protocol would grow with it.

**Classifying reads by parsing the query string.** Rejected for the same reason the
2026-08-01 spec rejected it for routing, and now unnecessary: `is_read_only` answers it
from the parser.

**Shared memory now (Plasma or an shm IPC framework).** Rejected. Plasma's only Rust
binding is third-party and unmaintained. `iceoryx2` brings its own discovery and
lifecycle model into a project that already has a daemon, a lock protocol, an instance
registry and a handover mechanism — a second coordination authority is the failure mode
this codebase keeps paying for. A `MAP_SHARED` mmap is already shared memory on all
three CI platforms, and Arrow IPC is designed to be read in place from a mapped buffer,
so the option remains open with zero new dependencies whenever measurement justifies it.

**Arrow Flight.** Purpose-built for streaming result sets, and it would solve Windows
transport too, but it pulls in a full gRPC stack (`tonic` + `prost` + `arrow-flight`)
where `interprocess` plus the already-present `arrow` suffices.

## Open Questions

- Connection pool size, and its behaviour when exhausted: queue or refuse. Refusing is
  safer for a daemon that must not accumulate unbounded work, but a queue is friendlier
  to bursty MCP traffic.
- Whether group/multi-repo reads (`group_search`, `group_query`) fan out to one endpoint
  per member repo or to a single coordinating daemon.
- Whether the escape hatch should additionally require the daemon to be provably
  unstartable, rather than trusting the operator to use it only when it is.
