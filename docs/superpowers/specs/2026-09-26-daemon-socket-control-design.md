# Daemon Control and Status over the Read Socket — Design

**Issues:** #155 (control messaging onto the read socket), #202 (doctor judges
liveness by lease count). Also closes Gap B of Task 19 in
`docs/superpowers/plans/2026-08-21-daemon-watch-command-split.md` (no
running/stopped/disabled state per watch role).
**Status:** approved direction 2026-09-26; spec under review.

## Problem

Every project's daemon has a real local socket (`ReadEndpoint`: Unix domain
socket / Windows named pipe). Reads use it, and since #38 so do leases
(`ClientFrame::Attach`). Control does not:

- **`WatchControl`** (start/stop/enable/disable/restart for code, docs and the
  daemon itself) is a `WriteRequest` dropped as a file into
  `.infigraph/requests/`. The synchronous write coordinator finds it on its
  200ms `read_dir` tick (`route_or_serve_request`, `daemon/mod.rs`), and the
  client polls for a `.result` file for up to 30s. Every caller opens a whole
  `Infigraph` (language registry included) just to drop that file.
- **Status** is inferred from outside. `watch-status` and `get_watch_status`
  report only whether someone holds `watch.lock`. Nothing reports whether code
  or doc watching is running, stopped or disabled. `ps` never talks to a
  daemon, so a wedged daemon, or one overdue for its idle exit, still shows as
  "live". `doctor` decides whether a daemon is still needed from the MCP
  instance registry (`project_has_live_mcp_instance`), which misses `path=`
  reads, group tools and CLI use. That is the signal #38 moved the daemon itself
  away from.

Already done, and not repeated here:
- Client registration and heartbeat: #38's leases.
- The restart handshake: #152, fixed by `5d21de6`. `cmd_daemon_restart` waits
  on `confirm_daemon_exited` and refuses to spawn beside a survivor. Only the
  transport of its Stop changes here.

## Goal

Control and status travel over the read socket as typed requests with one
synchronous typed reply each. Neither keeps a daemon alive. `watch.stop` stays
as the out-of-band stop for a daemon wedged beyond answering.

Success criteria:
- A control request on an idle daemon is answered well within one
  `COORDINATOR_TICK`, with no request or result files involved.
- `Status` answers within 500ms even while the coordinator is busy or stuck.
- Polling `Status` (doctor, `ps`, `watch-status`) never extends a daemon's life.
- `watch-status`, `get_watch_status`, doctor and `ps` all read one
  `StatusReport`.
- Exactly one control path exists. The file-drop `WatchControl` and the
  docs-only `watch.stop.docs` sentinel are both deleted.

## Non-goals

- **Data write requests** (`Index`, `FullReindex`, `ScipImport`, …) stay on
  file-drop, and the coordinator's per-tick `read_dir` poll that picks them up
  is kept as it is. Its worst case is one 200ms tick in front of work that runs
  for seconds or minutes. Moving write requests onto the socket, which would
  leave one transport and remove `requests/` entirely, is **#204**. It reuses
  this design's channel, reply sender and in-flight count.
- **Task 19 Gap A** (non-daemon watcher reaping) is unchanged. It remains a
  written task in the 08-21 plan.
- **Mixed-build operation** is not supported beyond getting a daemon stopped: a
  new client against an incompatible daemon can stop or restart it (via the
  sentinel) and does nothing else.

## Decisions

| # | Decision | Why |
|---|---|---|
| D1 | Delete the file-drop `WriteRequest::WatchControl` path outright | One control path. A silent fallback keeps both paths alive until they drift (5818aa1). The sentinel already covers the one case where a fallback matters. |
| D2 | Every op declares whether it counts as the daemon being used, through a trait with a required associated const | Doctor or `ps` polling must never keep an idle daemon alive, and a new op cannot forget to answer the question. |
| D3 | Control reaches the synchronous coordinator over a bounded channel with a reply sender. The coordinator's idle `sleep` becomes `recv_timeout` on that channel. | The coordinator stays synchronous and remains the only owner of watch state, so nothing becomes shared-mutable. The tick latency goes away for control without adding a runtime. |
| D4 | `Status` is answered on the read-service thread from shared state, never through the coordinator | It has to answer when the coordinator can't, and a wedged coordinator is exactly when a status check matters. |
| D5 | `ps` asks each live daemon for its `StatusReport`, in parallel, with a 500ms deadline | Shows wedged and overdue daemons, and the total time stays bounded however many daemons are running. |
| D6 | A reply-less EOF is `Incompatible` (not "too old") | The daemon could not parse the frame. That happens whether its build is older or newer. |
| D7 | Delete the `watch.stop.docs` sentinel. MCP `stop_watch_docs` sends `Control(Docs, Stop)`. | It is a second control path, and the only one: it parks the doc-watch loop in a "suppressed" state the daemon cannot see, so `Status` would report a paused loop as `Running`. The daemon-wide `watch.stop` remains the out-of-band stop. |

## Wire protocol (`daemon/read_protocol.rs`)

`ClientFrame` stays `#[serde(untagged)]`, so an existing `ReadRequest` and
`Attach` look the same on the wire. Each new op arrives as a single-key object
whose key no other frame has:

```rust
#[serde(untagged)]
pub enum ClientFrame {
    Attach(Attach),          // {"attach_pid": 123}
    Read(ReadRequest),       // {"store": .., "query": .., ..}   unchanged
    Status(StatusFrame),     // {"status": {}}
    Control(ControlFrame),   // {"control": {"role": "Code", "action": "Stop"}}
}
```

`WatchRole` and `WatchAction` move unchanged from `daemon_protocol.rs` to this
module, which re-exports them from the old path so callers change imports only
where they already touch the file.

**Replies.** `Read` keeps its streaming `Rows…End` frames. `Status` and
`Control` each reply with exactly one frame:

```rust
pub enum OpReply<T> { Ok(T), Err(String) }
// Status  -> OpReply<StatusReport>
// Control -> OpReply<()>    // the coordinator's outcome, same strings as today
```

**`StatusReport`:**

```rust
pub struct StatusReport {
    pub pid: u32,
    pub build: String,           // infigraph_core::build_hash()
    pub leases: usize,
    pub idle_secs: Option<u64>,  // Liveness::idle_for: None while leased
    pub grace_secs: u64,         // daemon_idle.grace_secs; 0 = idle exit disabled
    pub idle_check_secs: u64,    // how often the coordinator evaluates the idle exit
    pub work_in_flight: bool,    // drain / full reindex / SCIP running: defers the idle exit
    pub code: RoleState,
    pub docs: RoleState,
}

pub enum RoleState {
    Running,
    Stopped,   // stopped over control, or the producer ended itself; policy still on
    Disabled,  // persisted policy off and not running
    NotOwned,  // this daemon has no loop for the role (docs handle absent)
}
```

**The `DaemonOp` trait (D2):**

```rust
pub trait DaemonOp: Serialize + DeserializeOwned {
    type Reply: Serialize + DeserializeOwned;
    /// Whether serving this op counts as the daemon being used. No default:
    /// every op must answer.
    const KEEPS_ALIVE: bool;
}
```

| Op | `KEEPS_ALIVE` |
|---|---|
| `ReadRequest` | `true` |
| `StatusFrame` | `false` |
| `ControlFrame` | `false` |

`Attach` is not a `DaemonOp`. It is the lease itself, with its own accounting
(`lease_opened`/`lease_closed`). One generic `dispatch::<O>()` calls
`Liveness::touch()` exactly when `O::KEEPS_ALIVE` is true, and handlers never
touch liveness themselves. The constant is associated rather than per-request
(`&self`) because no op needs different answers for different actions, and a
constant can be checked at compile time.

## Daemon side

### Dispatch (`daemon/read_service.rs`)

| Frame | Runs on | Liveness |
|---|---|---|
| `Attach` | its own thread (`park_lease`), unchanged | lease accounting |
| `Read` | a pool worker, unchanged | `touch()` via `KEEPS_ALIVE` |
| `Status` | a pool worker, answered from memory | none |
| `Control` | its own detached thread | none |

`Control` never occupies a pool worker, for the same reason leases don't: a
control op can wait up to 30s on a busy coordinator, and a few of those parked
on pool workers would stop every read.

### `ControlPort` (coordinator-owned)

```rust
pub struct ControlPort {
    tx: mpsc::SyncSender<ControlMsg>,   // sync_channel(8)
    state: Arc<DaemonState>,
}
type ControlMsg = (WatchRole, WatchAction, mpsc::Sender<Result<(), String>>);
```

The coordinator creates the port and passes it to
`ReadService::start_with_source` alongside `Liveness`. The service is rebuilt
on a #187 socket rebind, so the port has to live outside it. That is the same
reason `Liveness` does.

The control thread sends with `try_send`. If all 8 slots are full, which means
the coordinator is wedged, it replies straight away with
`Err("daemon busy: the coordinator has not taken control requests …; \`infigraph
daemon stop\` falls back to the watch.stop sentinel")`. It then waits on the
reply receiver for at most the client's 30s deadline. If that passes, it drops
the connection, and the client sees `Unresponsive`.

### The coordinator's wait

`std::thread::sleep(COORDINATOR_TICK)` at the bottom of
`run_write_coordinator`'s loop becomes `control_rx.recv_timeout(COORDINATOR_TICK)`.
When a message arrives, the coordinator runs:

```rust
fn apply_watch_control(
    role: WatchRole,
    action: WatchAction,
    code_watch: &mut CodeWatch,
    docs: Option<&Arc<dyn DocsHandle>>,
) -> Result<(), String>
```

It is today's `WatchControl` arm in `route_or_serve_request`, moved out without
behavior changes: Enable/Disable act like Start/Stop, Docs without a handle is
an `Err`, and Daemon accepts only Stop/Restart. The coordinator sends the
outcome on the reply sender, then updates `DaemonState`, then, for a
successful Daemon Stop/Restart, sets `shutdown_requested` and cancels
`daemon_token`. The reply is sent before teardown starts, as it is today.
Messages that arrive together are drained with `try_recv` before the loop
continues.

The file-drop `WatchControl` arm, `reply_to_watch_control`, the `WatchControl`
variant, and `serve_one_request`'s rejection arm for it are deleted.

### `DaemonState`: what `Status` reads without the coordinator

```rust
pub struct DaemonState {
    code: AtomicU8,            // encoded RoleState
    docs: AtomicU8,
    work_in_flight: AtomicBool,
}
```

The coordinator writes it after every `apply_watch_control` and once per loop
iteration, so a producer that ends by itself shows `Stopped` within one tick.
`work_in_flight` is the same disjunction the idle-exit check already computes
(drain, full reindex, SCIP enrichment, SCIP import), stored rather than
recomputed.
`Disabled` is decided when the state is written: a role that is not running
whose persisted policy (`watch::config`) is off is `Disabled`, otherwise
`Stopped`. That keeps config reads off the `Status` path.

The `Status` handler builds the report from `DaemonState`, `Liveness`
(`leases()`, `idle_for(now)`), the resolved grace and idle-check settings,
`std::process::id()` and `build_hash()`.

### Small API changes

- `CodeWatch::is_running(&self) -> bool`: `task.is_some_and(|t| !t.is_finished())`,
  the check `start()` already makes.
- `DocsControl` changes from a bare closure to a trait:
  ```rust
  pub trait DocsHandle: Send + Sync {
      fn control(&self, action: WatchAction) -> Result<(), String>;
      fn is_running(&self) -> bool;
  }
  ```
  `cmd_daemon` implements it over its existing `Arc<Mutex<DocWatchThread>>`.
  `None` still means the daemon owns no doc-watch loop (`RoleState::NotOwned`).

### Doc-watch stop without a sentinel (D7)

Today the doc-watch loop (`infigraph-docs::watch::watch_docs_daemon_loop`)
polls for two files: `docs.kuzu` (attach once it exists, detach if it goes)
and `watch.stop.docs` (detach and stay suppressed until `docs.kuzu` cycles or
a `resume` flag is set). The second is written only by MCP `stop_watch_docs`,
and the suppression it causes is invisible outside the loop.

After this change:
- The loop takes only `shutdown`. `run_attached_cycle` no longer takes a stop
  sentinel or returns a "sticky" flag. `suppressed_until_absent` and the
  `resume` flag are deleted.
- A docs Stop is `DocWatchThread::stop()`, which sets `shutdown` and joins the
  thread. A docs Start respawns it. There is no paused-but-alive state left,
  so `DocsHandle::is_running` is exact.
- The `docs.kuzu` existence poll stays. It watches an index-lifecycle
  condition, not a control signal.

### Shutdown ordering

A Daemon Stop reply reaches the control thread before the coordinator starts
tearing down, and that thread writes it to the socket at once.
`ReadService::stop_and_join` ends leases and the accept loop and does not
join control threads, so it cannot cut off a reply already being written.

## Client side

### `daemon/control.rs` (new): the only client of the new frames

```rust
pub enum ControlError {
    NoDaemon,          // nothing listening and watch.lock not held
    Incompatible,      // EOF before any reply: the daemon could not parse the frame
    Unresponsive,      // deadline passed, or listening socket missing while watch.lock is held
    Refused(String),   // the daemon's own Err
}

pub fn query_status(root: &Path) -> Result<StatusReport, ControlError>;    // 500ms
pub fn send_control(root: &Path, role: WatchRole, action: WatchAction)
    -> Result<(), ControlError>;                                            // 30s
```

- `send_control` connects with the existing `connect_allowing_for_startup`, so
  a daemon still starting is waited for, as today. `query_status` uses a plain
  `ReadEndpoint::connect()`. If that fails while `watch.lock` is held, the
  result is `Unresponsive`, meaning starting or wedged.
- Neither function takes a lease or calls `ensure_daemon_*`. Checking on a
  daemon does not keep it alive, on the client side as well as the daemon side.
- **Deadlines** are a read timeout on the stream if `interprocess` supports one
  on both platforms. If not, the call runs on a helper thread and the caller
  waits with `recv_timeout`. The plan's first task settles which.
- `Infigraph::submit_watch_control_and_await` is deleted, and no control
  caller opens an `Infigraph` any more.

### Callers

| Caller | Change | On `Incompatible` / `Unresponsive` |
|---|---|---|
| `cmd_daemon_stop` | `send_control(Daemon, Stop)` | write the `watch.stop` sentinel |
| `cmd_daemon_restart` | the same, then the existing `confirm_daemon_exited` | the sentinel, then the same confirmation; still refuses to spawn beside a survivor |
| `cmd_watch_control`, MCP `watch_control` | `send_control(role, action)` | error: "daemon is an incompatible build / not responding — run `infigraph daemon-restart`" |
| `cmd_watch_status`, MCP `get_watch_status(path)` | `query_status`, printed through one `impl Display for StatusReport` shared by both | "not responding (pid N holds watch.lock)" / "incompatible build" |
| MCP `stop_watch_docs(path)` | `send_control(Docs, Stop)` instead of writing `watch.stop.docs` | error, as for `watch_control` |
| doctor `check_one_watcher` | uses `query_status` instead of `project_has_live_mcp_instance` (deleted) | warn with the sentinel stop hint |
| `infigraph ps` | queries each live daemon row in parallel, and adds `LEASES IDLE CODE DOCS` | `no reply` / `incompatible` |

The pre-checks callers make today (`daemon_is_alive` for the "no daemon
running" message, and Enable/Disable persisting policy before any daemon is
contacted) keep their current behavior.

### Doctor's verdicts (#202)

`check_one_watcher` keeps its existing checks for a missing, empty, dead-PID and
stale-heartbeat lock. The final registry check becomes a pure
`watcher_verdict(&StatusReport, now) -> CheckResult`:

| Report | Verdict |
|---|---|
| `leases > 0` | pass: "N clients leasing" |
| `grace_secs == 0` | pass: "idle exit disabled" |
| no leases, `work_in_flight` | pass: "idle, exit deferred by in-flight work" |
| no leases, idle < grace + `idle_check_secs` | pass: "idle Xs, exits in ~Ys" |
| no leases, idle ≥ grace + `idle_check_secs` | **warn**: "should have exited Zs ago" |

The slack is one idle-check interval, because the exit is evaluated only that
often. Rows are checked top to bottom. `Unresponsive` and `Incompatible` warn,
naming `infigraph daemon stop` (sentinel fallback) and `infigraph kill <pid>`.

## Error handling summary

- **No daemon:** callers keep today's "No daemon running" messages.
- **Coordinator wedged:** `Status` still answers. Control fails fast with
  `Refused("daemon busy …")` once the channel is full, or `Unresponsive` after
  30s. `daemon stop` then uses the sentinel.
- **Whole daemon wedged (read service too):** every socket op is
  `Unresponsive`. `daemon stop` and `restart` use the sentinel, and restart's
  existing exit confirmation names `infigraph kill <pid>` if that fails too.
- **Incompatible build:** stop and restart use the sentinel. Everything else
  says to run `infigraph daemon-restart`.

## Testing

**Protocol units (`read_protocol.rs`):**
- `Status`, `Control` and `OpReply` round-trip.
- None of the four frame kinds parses as another.
- Today's `ReadRequest` and `Attach` bytes parse unchanged.

**Liveness, one test per op** (backdating with `last_activity_for_test`):
- `Read` advances `last_activity`.
- `Status` doesn't.
- `Control` doesn't.

**Coordinator dispatch.** These tests move from file-drop to `send_control`
with their assertions unchanged:
- `watch_daemon.rs`: Daemon Stop ends the loop; Daemon Start is rejected
  without stopping; the Docs role dispatches; Docs without a handle is
  `Refused`.
- `watch_control.rs`: the daemon survives a code Stop and keeps serving writes.

`watch_control_helper.rs` is rewritten against `daemon/control.rs`. Test
helpers that stop a daemon through file-drop `WatchControl`
(`daemon_kuzu_e2e.rs` and others) switch to `send_control`. Those already on
the sentinel stay.

**Real-daemon integration:**
- Daemon Stop over the socket: the client receives `Ok`, and the process exits.
- Latency: the median of five control round trips on an idle daemon is under
  `COORDINATOR_TICK`.
- Status reports:
  - `leases == 1` and `idle_secs == None` while a lease is held;
  - `code` goes `Running` → `Stopped` → `Disabled` through a control Stop and
    then a Disable;
  - `docs == NotOwned` with no docs handle.
- A busy coordinator, held by a test-only stall hook set from an env var (the
  `INFIGRAPH_TEST_BUILD_HASH_CHECK_SECS` pattern):
  - `Status` answers within 500ms;
  - control beyond 8 queued requests is `Refused("daemon busy …")` at once.

**Fake listeners on the real endpoint name:**
- Accept, read one frame and close → `Incompatible`.
- Accept and never reply → `Unresponsive` within the deadline.
- `cmd_daemon_stop` against the incompatible fake writes the `watch.stop`
  sentinel.

**Doc watching (D7):**
- A real daemon: docs Stop over control gives `docs == Stopped` in `Status`,
  and docs Start gives `Running` again, with a doc edit indexed afterwards.
- The docs loop ignores a stray `watch.stop.docs` file (the sentinel is gone).
- MCP `stop_watch_docs(path)` with no daemon still answers "No watcher running."
  and writes no file.

**Doctor and `ps`:**
- `watcher_verdict` unit tests for all five verdicts, including in-flight work
  deferring the "should have exited" warning.
- A `ps` formatting unit test.
- N unresponsive fakes answer in about one deadline in total, not N.

**Environment:** tests that open the graph themselves pin
`INFIGRAPH_BACKEND=kuzu`, and the CLI is rebuilt before MCP tests that spawn
it. The pre-commit perf gates don't touch this path.
