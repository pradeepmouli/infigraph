# Daemon Write Requests over the Read Socket — Design

**Issue:** #204. Follow-up to #155
(`docs/superpowers/specs/2026-09-26-daemon-socket-control-design.md`), whose
channel, reply sender, per-request thread and in-flight count this reuses.
**Status:** approved direction 2026-09-27; spec under review.

## Problem

Since #155 the daemon has two transports. Reads, leases, status and control
travel over the read socket. Data writes (`Index`, `FullReindex`,
`ScipImport`, `UpsertFilesBulk`, and the other `WriteRequest` variants) still
use file-drop:

- The client writes `<root>/.infigraph/requests/<pid>-<nanos>-<n>.request`
  and polls for the matching `.result`
  (`daemon_protocol::submit_write_request_named_cancellable`).
- The coordinator lists `requests/` on every 200ms tick
  (`run_write_coordinator`) and hands each file to `route_or_serve_request`.
- Every reply is a file path: `Waiter.reply_path`,
  `PendingFullReindex.reply_paths`, `PendingScipImport.reply_path`, written by
  `execute_drain`, `reply_to_all`, `finish_scip_import` and
  `reply_err_to_waiters`.
- A killed client withdraws nothing, so #164 added `request_client_is_gone`,
  which parses the submitter's pid and start time out of the file name.
- "Not yet" means "leave the file for the next tick". That covers a FullReindex
  behind a drain, a ScipImport while busy, and a synchronous write during a
  drain or reopen backoff.
- The daemon talks to itself this way too. Its SCIP enrichment callback
  submits `ScipImport` through file-drop with a 600s cancellable wait, and
  compaction and auto-recovery write `compaction.request` and
  `auto-recovery.request`.

## Goal

One transport. Every data write is a typed request on the read socket with a
typed reply, and `requests/`, `.result` files, the per-tick `read_dir`,
`request_client_is_gone` and `discard_request` are gone.

Success criteria:
- On an idle daemon, a write's median round trip is under `COORDINATOR_TICK`.
- Nothing under `.infigraph/` is listed per tick to find requests.
- A client that disconnects before its work starts costs the daemon nothing.
- A daemon crash during a long write is reported as a crash, not as a version
  mismatch.
- A pre-change client gets a prompt, explicit error instead of a silent
  timeout of up to 600s.

## Non-goals

- **Cancelling running work.** Work already started always finishes. Only
  unstarted work is dropped when its clients go (D3).
- **Inline bulk payloads.** Extraction JSON and edge Arrow files stay as
  sidecar files, and only their paths travel on the socket.
- **Mixed-build operation** beyond failing fast. A new client against an old
  daemon gets `Incompatible`; an old client against a new daemon gets an error
  reply (D5). Neither is served.
- **Windows reader-thread cancellation** remains #206. Write waits share
  `exchange`'s mechanism and inherit its gap.

## Decisions

| # | Decision | Why |
|---|---|---|
| D1 | One coordinator port carries both control and write messages. Control keeps `try_send` fail-fast; a write waits for a slot while its client stays connected. The bound grows from 8 to 64. | `std::mpsc` has no `select`. Two channels would put up to a tick of latency back on one of them, or add a dependency. Admission policy, not the channel, is where control and writes differ. |
| D2 | A client is gone when **its request's own connection** closes. The daemon notices with a non-blocking read every 250ms while the write waits. | More precise than the lease: one process can have several writes outstanding. Cancellation is free, since closing the connection is the withdrawal. Nothing probes pids. |
| D3 | Unstarted work whose waiters have all gone is dropped at pickup. Running work always finishes, and its reply is discarded. | Matches #164, which checked at pickup only. A drain writes the live graph, so cancelling one midway is what the growth and abort guards exist to prevent. |
| D4 | A write gets two reply frames: an admission `OpReply<()>`, once it is on the coordinator's channel, then `OpReply<WriteResult>`. | With one frame, a daemon that dies three minutes into a FullReindex produces an EOF before any reply, which #155's client reads as `Incompatible`. The ack separates `Incompatible` (EOF before it) from `Lost` (EOF after it). |
| D5 | The daemon answers any leftover `requests/*.request` with an `Err` result that names its build and says to restart the client, and removes the file. The sweep runs every 2s and is marked for removal after one release. | The realistic mixed build is an MCP server left running across an install. Without this, each of its writes hangs until its own timeout, up to 600s. #155's `WatchControl` refusal already worked this way, only incidentally. |
| D6 | In-process submitters never use the socket. Compaction and auto-recovery push straight onto the coordinator's deferred queue. The SCIP callback submits through a port handle. | They are the daemon itself. Only then can `requests/` go away. |
| D7 | One `deferred` queue replaces every "leave the file for the next tick" retry, in arrival order. | Three retry behaviors become one. Arrival order becomes explicit instead of whatever `read_dir` returns. |
| D8 | `WriteReply::send(self)` consumes the reply. | A waiter is answered exactly once, by type, rather than by `reply_err_to_waiters` probing whether a reply file already exists. |

## Wire protocol (`daemon/read_protocol.rs`)

One new frame, following #155's single-key pattern:

```rust
pub struct WriteFrame { pub write: WriteRequest }  // {"write": {"FullReindex": null}}

impl DaemonOp for WriteFrame {
    type Reply = WriteResult;
    const KEEPS_ALIVE: bool = true;  // a write is use of the daemon (#38), as picking up a .request is today
}

#[serde(untagged)]
pub enum ClientFrame { Attach(..), Read(..), Status(..), Control(..), Write(WriteFrame) }
```

`ClientFrame::keeps_alive`'s exhaustive match will not compile until `Write`
answers.

**Replies.** Both frames reuse `OpReply`, so the client always knows the
type of the next frame:
1. **Admission:** `OpReply<()>`. `Ok(())` ("accepted") is written once the
   request is on the coordinator's channel. `Err(msg)` is a refusal before
   admission (the coordinator is shutting down, say), and nothing follows it.
2. **Outcome:** `OpReply<WriteResult>`, sent only after `Ok(())`.

EOF before frame 1 is `Incompatible`. EOF between frames 1 and 2 is `Lost`.

## Daemon side

### `CoordinatorPort` (renamed from `ControlPort`, `daemon/coordinator_port.rs`)

```rust
pub enum PortMsg {
    Control(ControlMsg),                           // unchanged
    Write { request: WriteRequest, reply: WriteReply },
}

pub struct CoordinatorPort { tx: SyncSender<PortMsg>, pub state: DaemonState, in_flight: AtomicUsize }
// sync_channel(PORT_QUEUE = 64)

impl CoordinatorPort {
    pub fn submit_control(&self, r: ControlRequest) -> Result<Receiver<ControlReply>, String>; // try_send: BUSY / SHUTTING_DOWN
    pub fn try_submit_write(&self, r: WriteRequest, reply: WriteReply) -> Result<(), (TrySendError, ..)>;
    pub fn submit_write(&self, r: WriteRequest) -> Result<Receiver<WriteResult>, String>;       // in-process (SCIP callback)
    // enter / in_flight / wait_idle: unchanged, now also counting write threads
}
```

### `WriteReply`

```rust
pub struct WriteReply {
    tx: Option<mpsc::Sender<WriteResult>>,  // None: an in-process submitter that does not wait
    gone: Arc<AtomicBool>,                  // set by the connection thread on client disconnect
}

impl WriteReply {
    pub fn channel() -> (Self, Receiver<WriteResult>);
    pub fn send(self, result: WriteResult);  // consumes (D8); a closed receiver is ignored
    pub fn is_gone(&self) -> bool;           // always false for internal()
    pub fn internal() -> Self;               // compaction, auto-recovery
}

impl Drop for WriteReply { /* unsent: send Err(DROPPED) */ }
```

`Drop` completes D8. An unanswered reply sends
`Err("the daemon dropped this write without answering it")`, so no path can
leave a client waiting: a drain task that panics (its waiters drop during
the unwind), an early return, or teardown. `execute_drain` answers its own
waiters with its specific error before returning one.
`reply_err_to_waiters` and `InFlightDrain.waiter_replies` exist only to
cover those paths, so they are deleted, not ported.

These change from `PathBuf` to `WriteReply`:
- `Waiter.reply_path`
- `PendingFullReindex.reply_paths` (now `Vec<WriteReply>`; `join` pushes one)
- `PendingScipImport.reply_path`
- `reply_to_all`, `reply_err_to_waiters` and `execute_drain`'s reply write


### Connection thread (`read_service::spawn_write`, next to `spawn_control`)

A write occupies its own thread, never a pool worker, for the same reason
control does: it can wait minutes. Counted by `CoordinatorPort::enter`.

1. **Admission.** Call `try_send` in a loop, sleeping 10ms on `Full`, and stop
   if the client disconnects meanwhile. A disconnected channel gets the
   admission frame `OpReply::<()>::Err(SHUTTING_DOWN)`.
2. **Ack.** Write `OpReply::<()>::Ok(())`.
3. **Wait.** `recv_timeout(250ms)` on the reply receiver. Between waits, do a
   non-blocking read of the stream. `Ok(0)` or any byte (a protocol violation:
   the client sends nothing after its frame) sets `gone` and ends the thread.
   `WouldBlock` means the client is still there.
4. **Reply.** Write `OpReply<WriteResult>` and close. If the reply sender is
   dropped without sending, which only happens during shutdown teardown, write
   `OpReply::Err(SHUTTING_DOWN)`.

If spawning the thread fails, the client still gets an `Err` reply, as in
`spawn_control`.

### Routing (`daemon/mod.rs`)

`route_or_serve_request(path, …)` becomes `route_write(request, reply, …)`.
It keeps the same arms and coalescing. The queue-lock-plus-waiter pairing, the
FullReindex start-or-join and the ScipImport start are unchanged. These go
away:
- the #164 check and `discard_request`;
- the malformed-JSON fallback (a frame that does not parse never reaches the
  coordinator; the connection closes, and the client sees `Incompatible`);
- `std::fs::remove_file` of request files.

`serve_one_request(&Infigraph, &Path)` becomes
`serve_write(&Infigraph, &WriteRequest) -> WriteResult`. A missing or corrupt
sidecar becomes an `Err` result. `serve_request_locked` takes the reply and
returns it (`Err(reply)`) when it could not run yet.

`IngestSource::Inline` located its data file next to the request file, which
no longer exists, so it becomes `Inline(PathBuf)`, naming its sidecar.

### Deferred work (D7)

```rust
deferred: VecDeque<(WriteRequest, WriteReply)>
```

Each coordinator iteration:
1. Drop deferred entries whose reply `is_gone()`.
2. Retry the rest in order, through `route_write`.
3. Anything that still cannot start goes back in order.

Retrying happens where the `read_dir` block is today. New messages from the
port are routed as they arrive. One that cannot start (FullReindex behind a
drain, ScipImport while busy, a synchronous write during a drain or reopen
backoff) is appended to `deferred`.

**Dropping at pickup (D3).** A deferred entry whose reply `is_gone()` is
removed before it is retried, so a FullReindex, ScipImport or synchronous
write whose client has left never starts. Each such drop is logged as
`[daemon] dropped N write(s): their clients disconnected`.

Queue-shaped writes (`Index`, `UpsertFilesBulk`, `RemoveFiles`,
`ResolveCalls`) are enqueued as soon as they arrive, so they never sit in
`deferred`. A gone client's waiter just goes unanswered, and the drain still
runs. It cannot be skipped safely: queue items do not record which request
added them, so a path a gone client named may also be a real watcher change,
and skipping it would lose that change. The cost is only the files that
actually changed.

**The daemon's own rebuilds collapse.** Compaction may ask for a rebuild on
every tick until one lands, and a fixed file name (`compaction.request`)
used to collapse those requests silently. `request_internal_rebuild` does
it explicitly instead: it queues a FullReindex only if none is deferred or
running.

### In-process submitters (D6)

- `submit_compaction_rebuild` is deleted. Its call site calls
  `request_internal_rebuild`, which cannot fail.
- `recovery::drain_recovery_sentinel` returns `Result<bool>` ("a rebuild is
  wanted") instead of writing `auto-recovery.request`, and the coordinator
  calls `request_internal_rebuild`. The attempts log, the crash-loop breaker
  and the audit lines are unchanged.
- The SCIP enrichment callback gets a fourth argument, `WriteSubmitter` (a
  cloneable handle on the port):
  `FullReindexCallback = dyn Fn(PathBuf, ScipEnrichJob, CancellationToken,
  WriteSubmitter)`. `WriteSubmitter::submit(request, &token)` waits in 100ms
  slices, as #138 requires. On cancellation it marks its reply gone, so the
  import is dropped at pickup if it has not started, and returns
  `WriteRequestCancelled`.

### Legacy refusal (D5)

`refuse_legacy_requests(infigraph_dir)` runs every 2s. If `requests/` exists,
each `*.request` gets a `.result` with
`WriteResult::Err("this daemon (build <hash>) no longer accepts file-drop
requests; restart the client (MCP: /mcp reconnect)")`, and the request file
is removed. Its doc comment marks it for removal one release after this
lands. The empty directory is left alone. `.result` files left there are
read and removed by old clients, or are harmless.

### Idle exit and status

- `has_pending_request` (#203 M2) is deleted. The idle exit is also deferred
  while `port.in_flight() > 0` or `!deferred.is_empty()`.
- Both conditions join the `work_in_flight!()` disjunction, so
  `StatusReport.work_in_flight`, doctor's verdicts and `ps` cover them with no
  new fields.

### Shutdown

When the loop exits:
1. `drop(port_rx)`: write threads still waiting for admission reply
   `SHUTTING_DOWN`.
2. `deferred` and any queued drain waiters get
   `Err("daemon shutting down")`, as `reply_err_to_waiters` does today.
3. A running drain, FullReindex or ScipImport is joined and replied to as it
   is now.
4. The existing `wait_idle(2s)` covers write threads writing their final
   frame.

## Client side (`daemon/writes.rs`, new)

The only sender of `Write` frames.

```rust
pub struct WriteOpts<'a> { pub timeout: Duration, pub cancel: Option<&'a CancellationToken> }

pub fn submit(root: &Path, request: &WriteRequest, opts: WriteOpts) -> anyhow::Result<WriteResult>;
pub fn sidecar_path(root: &Path, ext: &str) -> PathBuf;  // .infigraph/write-tmp/<pid>-<nanos>-<n>.<ext>
```

1. **Admission, before connecting:**
   - `blocking_fault(root, request)` → `DaemonFaulted` (#165, the growth
     refusal's `admits`), unchanged apart from taking `root`;
   - a `lease::in_use(root)` guard for the whole call, taken from `root`
     directly instead of derived from the staging directory.
2. **Connect** with `connect_allowing_for_startup(root)`. Failure maps
   through `not_connected` (`NoDaemon` / `Unresponsive`), as control does.
3. **Exchange.** #155's `exchange` becomes generic over its error type,
   `E: From<ControlError>`, and takes a stop check,
   `FnMut() -> Option<E>`, run every 50ms while waiting. Control passes a
   deadline check that returns `ControlError::Unresponsive`. A write passes
   one returning `anyhow::Error` for:
   - the cancel token fired → `WriteRequestCancelled`;
   - `blocking_fault` fired → `DaemonFaulted`;
   - the deadline passed → a timeout error.

   Every abort closes the connection (unix `shutdown(2)`, as today), and
   that close is the withdrawal the daemon sees (D2). Control keeps calling
   `exchange` with no abort check and a single reply frame. The ack is read
   only for `WriteFrame`, via a second required `DaemonOp` associated const,
   `const ACKED: bool`. It has no default, like `KEEPS_ALIVE`, so every op
   answers it.
4. **Errors.** `ControlError` gains `Lost` ("the daemon exited while serving
   this write") and serves both control and writes. `WriteRequestCancelled`
   and `DaemonFaulted` stay the downcast markers callers already match on.
   `WriteRequestCancelled` loses its `request_path` field.

**Sidecars.** Writers of extraction JSON, edge Arrow files and inline ingest
data call `sidecar_path` instead of `generate_request_name` plus the
`requests/` directory:
- the client removes its sidecar on every error return;
- the daemon removes it after reading, as today;
- the daemon's startup removes `write-tmp/` files older than 6h. The age sweep
  moves from the CLI's `sweep_stale_scip_scratch` into a shared
  `infigraph_core::scratch::sweep_older_than(dir, age, extensions)`, which the
  SCIP sweep now calls as well.

**Callers.** All move to `writes::submit`:
- the 18 `DaemonKuzuBackend` write methods;
- `cmd_index`'s FullReindex;
- `Infigraph::index_via_daemon`.

Deleted: `submit_write_request`, `submit_write_request_named`,
`submit_write_request_cancellable`, `submit_write_request_named_cancellable`,
`generate_request_name`, `request_client_is_gone`, `discard_request` and
`DaemonKuzuBackend::staging_dir`. `write_atomic` stays; it has other users.

## Error handling summary

| Situation | Client sees |
|---|---|
| No daemon, `watch.lock` free | `NoDaemon` (today's "no daemon running" callers keep their messages) |
| Fault latched before or during the wait | `DaemonFaulted` with the daemon's own record |
| Old daemon (cannot parse `Write`) | `Incompatible`: run `infigraph daemon-restart` |
| Daemon dies after admission | `Lost` |
| Coordinator shutting down before admission | `Refused("the daemon is shutting down")` |
| Client cancels or times out | `WriteRequestCancelled` / timeout; the daemon drops the waiter if its work has not started |
| Old client, new daemon | its `.result` says to restart the client, within 2s |

## Testing

**Moved to direct calls, assertions unchanged:**
- `tests/daemon_protocol_serve.rs` (15 tests):
  `serve_one_request(&infigraph, &path)` becomes `serve_write(&infigraph,
  request)`, returning the result instead of writing a file.
- `daemon/mod.rs` and `daemon/drain.rs` unit tests that build reply paths use
  `WriteReply::channel()`. `finish_drain_does_not_overwrite…` becomes
  `a_panicking_drain_still_answers_every_waiter`, pinning the `Drop`
  default.
- `tests/watch_daemon.rs`: `out_of_scope…contends_with_a_held_index_lock`
  asserts "deferred, then answered once the lock frees". The FullReindex and
  SCIP cancellation tests call `route_write`.

**Moved onto the real socket:**
- `daemon_protocol_watcher_wiring.rs`, `daemon_protocol_e2e.rs`,
  `watch_control.rs`;
- `daemon_kuzu_backend.rs`, whose one-request file server becomes a
  `ReadService` with a stub port;
- `daemon_kuzu_e2e.rs`: "nothing orphaned in `requests/`" becomes
  "`requests/` never exists". The crash-loop breaker test observes deferred
  rebuilds through a test hook, not files.

**New:**
- Protocol: `WriteFrame` round-trips and parses as no other frame; its
  `KEEPS_ALIVE` is true; today's frames still parse.
- Ack (fake listener on the real endpoint): EOF before the admission frame
  → `Incompatible`; EOF after `Ok(())` → `Lost`; an admission `Err` →
  `Refused`.
- Disconnect before start: a client killed while its FullReindex is deferred
  behind a stalled drain. The rebuild never runs, and "dropped 1 write(s)" is
  logged.
- Disconnect after start: the running work completes.
- Cancel: `submit` returns `WriteRequestCancelled` within about 100ms, and the
  daemon marks the waiter gone within 250ms.
- Faults: fast-fail before connecting; a fault latched mid-wait ends the wait.
  These are today's three tests, moved.
- Coalescing: two concurrent socket FullReindexes give one rebuild and two
  replies. N concurrent `Index` requests give one drain.
- In-process submitters:
  - compaction and auto-recovery reach `deferred` with no files;
  - the SCIP callback's import goes through the port;
  - a shutdown during its wait gives `WriteRequestCancelled` (#138).
- Legacy: any `.request`, including the existing
  `a_legacy_watch_control_request_file_gets_a_prompt_error`, gets an `Err`
  naming the build within one sweep, and is removed.
- Idle exit: none while a write connection is in flight or `deferred` is
  non-empty. This replaces the #203 M2 `.request` test.
- Sidecars: the client removes its sidecar on every error path; the startup
  sweep removes only files older than 6h.
- Latency: on an idle real daemon, the median of five `UpsertRepo` round trips
  is under `COORDINATOR_TICK`.

**Environment.**
- Tests that open the graph themselves pin `INFIGRAPH_BACKEND=kuzu`.
- The CLI is rebuilt before MCP tests that spawn it.
- Tests run per crate, not one `--all`, on this disk-constrained machine.
- None of the four pre-commit perf gates touches the transport.
