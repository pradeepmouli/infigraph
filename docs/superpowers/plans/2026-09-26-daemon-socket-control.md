# Daemon Control and Status over the Read Socket Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move `WatchControl` and daemon status off file-drop and onto the daemon's read socket, as typed requests with one synchronous reply each, then delete the file-drop control path.

**Architecture:** Two new `ClientFrame` variants (`Status`, `Control`) share the read socket with `Read` and `Attach`. `Status` is answered on the read service from a coordinator-owned `ControlPort` (atomics plus `Liveness`). `Control` runs on its own thread and reaches the synchronous coordinator over a bounded channel with a reply sender. The coordinator's idle `sleep` becomes a `recv_timeout` on that channel. A new client module, `daemon/control.rs`, is the only client of the new frames. The CLI, MCP, doctor and `ps` all use it.

**Tech Stack:** Rust, `serde` (untagged enums), `std::sync::mpsc`, `interprocess` 2.4.4 local sockets, `libc::shutdown` (unix).

**Spec:** `docs/superpowers/specs/2026-09-26-daemon-socket-control-design.md` (commit `0ff544e`). Read it before starting any task.

## Global Constraints

- Exactly one control path when done: `WriteRequest::WatchControl`, `reply_to_watch_control` and `Infigraph::submit_watch_control_and_await` are deleted in Task 9 (spec D1), and the `watch.stop.docs` sentinel in Task 7 (spec D7).
- Every op declares `const KEEPS_ALIVE: bool` with no default. `ReadRequest` = `true`, `StatusFrame` = `false`, `ControlFrame` = `false` (spec D2).
- `Status` never goes through the coordinator (spec D4). `Control` never runs on a read-pool worker.
- Neither client function takes a lease or calls `ensure_daemon_*`.
- Deadlines: `STATUS_DEADLINE = 500ms` (client), `CONTROL_REPLY_TIMEOUT = 30s` (daemon side, waiting on the coordinator), `CONTROL_DEADLINE = 35s` (client; longer than the daemon's so the daemon's own timeout arrives as a reply, not an EOF).
- Control channel capacity: `CONTROL_QUEUE = 8` (`sync_channel`, `try_send`).
- An EOF before any reply is `ControlError::Incompatible` (spec D6), never "too old".
- The `watch.stop` sentinel stays. `daemon stop`/`daemon-restart` fall back to it on `Incompatible` or `Unresponsive`.
- Data write requests (`Index`, `FullReindex`, `ScipImport`, …) stay on file-drop, and the coordinator's `read_dir` poll is kept unchanged. Moving them is #204, not this plan.
- Run tests as `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test … -- --test-threads=1` (the `.zshrc` leak: an unset backend means daemon since #159).
- Before any `-p infigraph-mcp` test that spawns the CLI: `cargo build -p infigraph-cli`.
- Commit each task with `--no-verify` (concurrent perf gates flake under contention). Task 10 runs the full hook once on the finished branch.
- Commit trailer, on every commit:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV
  ```
- Code search goes through Infigraph MCP tools (`search`, `get_symbols_in_file`, `get_doc_context`); `sed -n 'A,Bp'` for Edit context.

## Review Focus

1. **The Daemon Stop reply racing the process exit.** The reply must reach the client even though the coordinator starts tearing down right after sending it. Pinned by Task 5's `daemon_stop_over_the_socket_replies_ok_then_exits`, and made structural by `ControlPort::wait_idle` after the loop.
2. **A #187 socket rebind while a control request is in flight.** Dropping the old `ReadService` runs on the coordinator's thread, so it must never wait for a control thread that is waiting on that coordinator. Pinned by Task 3's `dropping_the_service_does_not_wait_for_a_pending_control`.
3. **A daemon that is listening but has no graph open yet** (it binds before the registry build). `Status` must still answer, because it reads no store. Pinned by Task 3's `status_answers_before_any_graph_is_open`.
4. **A client whose deadline passes against a wedged daemon** must not leave a thread blocked on the socket forever. That matters for the long-lived MCP process polling `get_watch_status`. Pinned on unix by Task 4's `an_unresponsive_daemon_sees_the_client_hang_up`.
5. **A pre-#155 client dropping a file-drop `WatchControl`** into a post-#155 daemon must get a prompt error reply, not a 30s timeout. Pinned by Task 9's `a_legacy_watch_control_request_file_gets_a_prompt_error`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/infigraph-core/src/daemon/read_protocol.rs` (modify) | Wire types: `WatchRole`/`WatchAction` (moved here), `StatusFrame`, `ControlFrame`, `ControlRequest`, `OpReply<T>`, `StatusReport`, `RoleState`, `DaemonOp`, frame I/O helpers |
| `crates/infigraph-core/src/daemon_protocol.rs` (modify) | Re-exports `WatchRole`/`WatchAction`. Loses `WriteRequest::WatchControl` (Task 9) |
| `crates/infigraph-core/src/daemon/control_port.rs` (create) | Daemon side: `DaemonState`, `ControlPort` (bounded channel, in-flight count), `role_state` |
| `crates/infigraph-core/src/daemon/read_service.rs` (modify) | Dispatches `Status` (pool) and `Control` (own thread) |
| `crates/infigraph-core/src/daemon/control.rs` (create) | Client side: `ControlError`, `query_status`, `send_control`, `query_status_many`, `describe_status` |
| `crates/infigraph-core/src/daemon/mod.rs` (modify) | Coordinator: owns the `ControlPort`, `recv_timeout`, `apply_watch_control`, `DocsHandle`, state refresh |
| `crates/infigraph-core/src/watch/mod.rs` (modify) | `CodeWatch::is_running` |
| `crates/infigraph-cli/src/info_commands.rs` (modify) | `DocsHandle` impl, `cmd_daemon_stop`/`restart`/`watch_control`/`watch_status`/`ps` |
| `crates/infigraph-mcp/src/tools/watch.rs` (modify) | `watch_control`, `tool_get_watch_status` |
| `crates/infigraph-core/src/doctor.rs` (modify) | `watcher_verdict` replaces `project_has_live_mcp_instance` |
| `crates/infigraph-core/tests/daemon_control.rs` (create) | Real-coordinator socket-control integration tests |
| `crates/infigraph-docs/src/watch.rs` (modify) | The doc-watch loop loses the `watch.stop.docs` sentinel, its suppression state and `resume` |
| `crates/infigraph-mcp/src/tools/docs.rs` (modify) | `stop_watch_docs(path)` sends `Control(Docs, Stop)` |

---

### Task 1: Wire protocol types

**Files:**
- Modify: `crates/infigraph-core/src/daemon/read_protocol.rs`
- Modify: `crates/infigraph-core/src/daemon_protocol.rs` (the `WatchRole` enum at L142-151 and `WatchAction` at L154-160 move out)

**Interfaces:**
- Produces (all `pub`, in `infigraph_core::daemon::read_protocol`):
  - `enum WatchRole { Code, Docs, Daemon }` and `enum WatchAction { Start, Stop, Enable, Disable, Restart }`, moved with their existing derives and doc comments; `daemon_protocol` re-exports both (`pub use crate::daemon::read_protocol::{WatchAction, WatchRole};`)
  - `struct StatusQuery {}`, `struct StatusFrame { pub status: StatusQuery }` (`Default`)
  - `struct ControlRequest { pub role: WatchRole, pub action: WatchAction }` (`Copy`), `struct ControlFrame { pub control: ControlRequest }`
  - `enum ClientFrame { Attach(Attach), Read(ReadRequest), Status(StatusFrame), Control(ControlFrame) }`, with `fn keeps_alive(&self) -> Option<bool>`
  - `enum OpReply<T> { Ok(T), Err(String) }`
  - `enum RoleState { Running, Stopped, Disabled, NotOwned }` (`Copy`, `Display`)
  - `struct StatusReport { pid: u32, build: String, leases: usize, idle_secs: Option<u64>, grace_secs: u64, idle_check_secs: u64, work_in_flight: bool, code: RoleState, docs: RoleState }` (`Display`)
  - `trait DaemonOp: Serialize + DeserializeOwned { type Reply: Serialize + DeserializeOwned; const KEEPS_ALIVE: bool; }`
  - `fn write_op<W: Write, O: DaemonOp>(w: &mut W, op: &O) -> Result<()>`
  - `fn write_reply<W: Write, T: Serialize>(w: &mut W, reply: &OpReply<T>) -> Result<()>`
  - `fn read_reply<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<Option<OpReply<T>>>` (`None` = EOF before any frame)

- [ ] **Step 1: Write the failing tests.** Append to `read_protocol.rs`'s `mod tests`:

```rust
    fn parse(bytes: &[u8]) -> ClientFrame {
        let mut framed = Vec::new();
        framed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        framed.extend_from_slice(bytes);
        read_client_frame(&mut framed.as_slice()).unwrap()
    }

    #[test]
    fn a_status_frame_round_trips_and_is_not_a_read_or_attach() {
        let mut buf = Vec::new();
        write_op(&mut buf, &StatusFrame::default()).unwrap();
        assert!(matches!(
            read_client_frame(&mut buf.as_slice()).unwrap(),
            ClientFrame::Status(_)
        ));
    }

    #[test]
    fn a_control_frame_round_trips_with_its_role_and_action() {
        let mut buf = Vec::new();
        let op = ControlFrame {
            control: ControlRequest { role: WatchRole::Docs, action: WatchAction::Restart },
        };
        write_op(&mut buf, &op).unwrap();
        let ClientFrame::Control(got) = read_client_frame(&mut buf.as_slice()).unwrap() else {
            panic!("expected Control");
        };
        assert_eq!(got.control, op.control);
    }

    #[test]
    fn todays_read_and_attach_bytes_still_parse_as_before() {
        assert!(matches!(parse(br#"{"attach_pid":7}"#), ClientFrame::Attach(_)));
        assert!(matches!(
            parse(br#"{"store":"Graph","query":"RETURN 1","params":[],"chunk_size":8}"#),
            ClientFrame::Read(_)
        ));
        assert!(matches!(parse(br#"{"status":{}}"#), ClientFrame::Status(_)));
        assert!(matches!(
            parse(br#"{"control":{"role":"Code","action":"Stop"}}"#),
            ClientFrame::Control(_)
        ));
    }

    #[test]
    fn only_reads_keep_the_daemon_alive() {
        assert_eq!(parse(br#"{"attach_pid":7}"#).keeps_alive(), None);
        assert_eq!(
            parse(br#"{"store":"Graph","query":"RETURN 1","params":[],"chunk_size":8}"#)
                .keeps_alive(),
            Some(true)
        );
        assert_eq!(parse(br#"{"status":{}}"#).keeps_alive(), Some(false));
        assert_eq!(
            parse(br#"{"control":{"role":"Code","action":"Stop"}}"#).keeps_alive(),
            Some(false)
        );
    }

    #[test]
    fn an_op_reply_round_trips_both_ways_and_eof_reads_as_none() {
        let report = StatusReport {
            pid: 1,
            build: "abc".into(),
            leases: 2,
            idle_secs: None,
            grace_secs: 1800,
            idle_check_secs: 60,
            work_in_flight: false,
            code: RoleState::Running,
            docs: RoleState::NotOwned,
        };
        let mut buf = Vec::new();
        write_reply(&mut buf, &OpReply::Ok(report.clone())).unwrap();
        let got: OpReply<StatusReport> = read_reply(&mut buf.as_slice()).unwrap().unwrap();
        assert!(matches!(got, OpReply::Ok(r) if r == report));

        let mut buf = Vec::new();
        write_reply::<_, ()>(&mut buf, &OpReply::Err("busy".into())).unwrap();
        let got: OpReply<()> = read_reply(&mut buf.as_slice()).unwrap().unwrap();
        assert!(matches!(got, OpReply::Err(m) if m == "busy"));

        let empty: &[u8] = &[];
        assert!(read_reply::<_, ()>(&mut &*empty).unwrap().is_none());
    }
```

- [ ] **Step 2: Run them and check they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib daemon::read_protocol -- --test-threads=1`
Expected: compile errors (`write_op`, `StatusFrame`, … not found).

- [ ] **Step 3: Move `WatchRole`/`WatchAction`.** Cut both enums (with derives and doc comments) from `daemon_protocol.rs` L142-160 into `read_protocol.rs` above `ClientFrame`. In their old place in `daemon_protocol.rs` put:

```rust
/// Moved to the read protocol with #155; re-exported so existing paths keep
/// working.
pub use crate::daemon::read_protocol::{WatchAction, WatchRole};
```

- [ ] **Step 4: Add the types and helpers** to `read_protocol.rs` (imports: `use serde::de::DeserializeOwned;`):

```rust
/// Asks the daemon how it is doing (#155, #202). Answered from memory on the
/// read service, never through the coordinator, so it answers even while the
/// coordinator is busy or stuck.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusQuery {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusFrame {
    pub status: StatusQuery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequest {
    pub role: WatchRole,
    pub action: WatchAction,
}

/// Starts, stops, enables, disables or restarts a watch role, or stops the
/// daemon. Replaces the file-drop `WriteRequest::WatchControl` (#155).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlFrame {
    pub control: ControlRequest,
}

/// The one reply frame `Status` and `Control` send. `Read` keeps its
/// streaming `ReadFrame`s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OpReply<T> {
    Ok(T),
    Err(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoleState {
    Running,
    /// Stopped over control, or the loop ended by itself; policy still on.
    Stopped,
    /// The persisted policy is off, and the role is not running.
    Disabled,
    /// This daemon has no loop for the role.
    NotOwned,
}

impl std::fmt::Display for RoleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RoleState::Running => "running",
            RoleState::Stopped => "stopped",
            RoleState::Disabled => "disabled",
            RoleState::NotOwned => "not owned by this daemon",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub pid: u32,
    pub build: String,
    pub leases: usize,
    /// `None` while any lease is held (`Liveness::idle_for`).
    pub idle_secs: Option<u64>,
    /// 0 = idle exit disabled.
    pub grace_secs: u64,
    /// How often the coordinator evaluates the idle exit.
    pub idle_check_secs: u64,
    /// A drain, full reindex or SCIP run is in flight; it defers the idle exit.
    pub work_in_flight: bool,
    pub code: RoleState,
    pub docs: RoleState,
}

impl std::fmt::Display for StatusReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Daemon PID {} (build {})", self.pid, self.build)?;
        writeln!(f, "  clients leasing: {}", self.leases)?;
        match (self.idle_secs, self.grace_secs) {
            (None, _) => writeln!(f, "  idle: no (leased)")?,
            (Some(idle), 0) => writeln!(f, "  idle: {idle}s (idle exit disabled)")?,
            (Some(_), _) if self.work_in_flight => {
                writeln!(f, "  idle: exit deferred by in-flight work")?
            }
            (Some(idle), grace) => writeln!(
                f,
                "  idle: {idle}s, exits after {grace}s without a client"
            )?,
        }
        writeln!(f, "  code watching: {}", self.code)?;
        write!(f, "  doc watching: {}", self.docs)
    }
}

/// An operation a client can send. The dispatcher, not each handler, applies
/// the liveness rule, so a new op cannot forget it (#155).
pub trait DaemonOp: Serialize + DeserializeOwned {
    type Reply: Serialize + DeserializeOwned;
    /// Whether serving this op counts as the daemon being used. No default:
    /// every op must answer.
    const KEEPS_ALIVE: bool;
}

impl DaemonOp for ReadRequest {
    // Streamed as `ReadFrame`s, not one `OpReply`; named for completeness.
    type Reply = Vec<Vec<String>>;
    const KEEPS_ALIVE: bool = true;
}

impl DaemonOp for StatusFrame {
    type Reply = StatusReport;
    const KEEPS_ALIVE: bool = false;
}

impl DaemonOp for ControlFrame {
    type Reply = ();
    const KEEPS_ALIVE: bool = false;
}
```

Extend `ClientFrame` (keep the doc comment; add one line saying each new variant is a single-key object whose key no other frame has) and add `keeps_alive`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientFrame {
    Attach(Attach),
    Read(ReadRequest),
    Status(StatusFrame),
    Control(ControlFrame),
}

impl ClientFrame {
    /// Whether serving this frame counts as activity. `None` for `Attach`,
    /// which is the lease itself and has its own accounting. Exhaustive on
    /// purpose: a new variant does not compile until it answers.
    pub fn keeps_alive(&self) -> Option<bool> {
        match self {
            ClientFrame::Attach(_) => None,
            ClientFrame::Read(_) => Some(ReadRequest::KEEPS_ALIVE),
            ClientFrame::Status(_) => Some(StatusFrame::KEEPS_ALIVE),
            ClientFrame::Control(_) => Some(ControlFrame::KEEPS_ALIVE),
        }
    }
}
```

Add the helpers, and make `write_request` delegate (DRY):

```rust
pub fn write_op<W: Write, O: DaemonOp>(w: &mut W, op: &O) -> Result<()> {
    write_len_prefixed(w, &serde_json::to_vec(op)?)
}

pub fn write_request<W: Write>(w: &mut W, req: &ReadRequest) -> Result<()> {
    write_op(w, req)
}

pub fn write_reply<W: Write, T: Serialize>(w: &mut W, reply: &OpReply<T>) -> Result<()> {
    write_len_prefixed(w, &serde_json::to_vec(reply)?)
}

/// `None` when the stream ends before any frame: the daemon closed without
/// answering, which a client reads as "could not parse what I sent".
pub fn read_reply<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<Option<OpReply<T>>> {
    match read_len_prefixed(r)? {
        None => Ok(None),
        Some(body) => Ok(Some(serde_json::from_slice(&body)?)),
    }
}
```

- [ ] **Step 5: Run the tests and check they pass**

Run: the Step 2 command.
Expected: PASS, including the existing `a_request_round_trips` and `an_attach_round_trips`.

- [ ] **Step 6: Check that the workspace still compiles** (the `serve_one` match is now non-exhaustive)

Run: `cargo check --workspace --all-targets`
Expected: one error in `read_service.rs::serve_one` (non-exhaustive match). Make it compile for now by adding, after the `Attach` arm:

```rust
        ClientFrame::Status(_) | ClientFrame::Control(_) => {
            // Served from Task 3 on.
            return Ok(());
        }
```

Re-run `cargo check --workspace --all-targets`. Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-core/src/daemon/read_protocol.rs crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/src/daemon/read_service.rs
git commit --no-verify -m "feat(core): Status and Control frames on the read protocol (#155)"
```

---

### Task 2: `ControlPort` and `DaemonState`

**Files:**
- Create: `crates/infigraph-core/src/daemon/control_port.rs`
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (add `pub mod control_port;` at the top of the alphabetical `pub mod` block, before `pub mod fault;`)

**Interfaces:**
- Consumes: `read_protocol::{ControlRequest, RoleState, StatusReport, WatchRole}`, `liveness::{Liveness, now_secs}`, `crate::build_hash()`
- Produces:
  - `pub const CONTROL_QUEUE: usize = 8;`, `pub const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(30);`
  - `pub const BUSY: &str`, `pub const SHUTTING_DOWN: &str`, `pub const NO_CONTROL: &str`
  - `pub type ControlReply = std::result::Result<(), String>;`
  - `pub struct ControlMsg { pub request: ControlRequest, pub reply: mpsc::Sender<ControlReply> }`
  - `pub fn role_state(running: bool, policy_on: bool) -> RoleState`
  - `pub struct DaemonState` with `set_role(&self, WatchRole, RoleState)`, `role(&self, WatchRole) -> RoleState`, `set_work_in_flight(&self, bool)`, `report(&self, &Liveness, now_secs: u64) -> StatusReport`
  - `pub struct ControlPort { pub state: DaemonState, .. }` with `new(grace_secs: u64, idle_check_secs: u64) -> (Arc<Self>, mpsc::Receiver<ControlMsg>)`, `submit(&self, ControlRequest) -> Result<mpsc::Receiver<ControlReply>, String>`, `enter(self: &Arc<Self>) -> InFlightGuard`, `in_flight(&self) -> usize`, `wait_idle(&self, Duration) -> bool`

- [ ] **Step 1: Write the failing tests** at the bottom of the new file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::read_protocol::WatchAction;

    fn req() -> ControlRequest {
        ControlRequest { role: WatchRole::Code, action: WatchAction::Stop }
    }

    #[test]
    fn role_state_distinguishes_stopped_from_disabled() {
        assert_eq!(role_state(true, true), RoleState::Running);
        assert_eq!(role_state(true, false), RoleState::Running);
        assert_eq!(role_state(false, true), RoleState::Stopped);
        assert_eq!(role_state(false, false), RoleState::Disabled);
    }

    #[test]
    fn a_fresh_state_reports_code_stopped_and_docs_not_owned() {
        let (port, _rx) = ControlPort::new(1800, 60);
        let r = port.state.report(&Liveness::new(), now_secs());
        assert_eq!(r.code, RoleState::Stopped);
        assert_eq!(r.docs, RoleState::NotOwned);
        assert_eq!((r.grace_secs, r.idle_check_secs), (1800, 60));
        assert_eq!(r.pid, std::process::id());
        assert_eq!(r.build, crate::build_hash());
    }

    #[test]
    fn the_report_carries_leases_idle_and_work_in_flight() {
        let (port, _rx) = ControlPort::new(10, 1);
        let liveness = Liveness::new();
        port.state.set_role(WatchRole::Code, RoleState::Running);
        port.state.set_work_in_flight(true);
        liveness.lease_opened();
        let r = port.state.report(&liveness, now_secs());
        assert_eq!((r.leases, r.idle_secs, r.work_in_flight), (1, None, true));
        assert_eq!(r.code, RoleState::Running);
        liveness.lease_closed();
        liveness.last_activity_for_test(now_secs() - 5);
        assert_eq!(port.state.report(&liveness, now_secs()).idle_secs, Some(5));
    }

    #[test]
    fn a_full_queue_refuses_at_once_and_a_dropped_receiver_reads_as_shutting_down() {
        let (port, rx) = ControlPort::new(0, 1);
        let replies: Vec<_> = (0..CONTROL_QUEUE).map(|_| port.submit(req()).unwrap()).collect();
        assert_eq!(port.submit(req()).unwrap_err(), BUSY);
        drop(replies);
        drop(rx);
        assert_eq!(port.submit(req()).unwrap_err(), SHUTTING_DOWN);
    }

    #[test]
    fn wait_idle_returns_once_every_guard_is_dropped() {
        let (port, _rx) = ControlPort::new(0, 1);
        let guard = port.enter();
        assert_eq!(port.in_flight(), 1);
        assert!(!port.wait_idle(std::time::Duration::from_millis(50)));
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(guard);
        });
        assert!(port.wait_idle(std::time::Duration::from_secs(2)));
        t.join().unwrap();
    }
}
```

- [ ] **Step 2: Run them and check they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib daemon::control_port -- --test-threads=1`
Expected: compile errors (module items missing).

- [ ] **Step 3: Implement** (above the tests):

```rust
//! The daemon's side of socket control (#155): what `Status` reads without
//! the coordinator, and the bounded channel `Control` uses to reach it.
//!
//! Owned by the coordinator for the daemon's whole run, like `Liveness`, and
//! handed to each `ReadService` it binds -- the service is rebuilt on a #187
//! socket rebind, and state living there would reset under a live daemon.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::liveness::Liveness;
use super::read_protocol::{ControlRequest, RoleState, StatusReport, WatchRole};

/// Control requests the coordinator may have queued before new ones are
/// refused. Bounded so a wedged coordinator fails requests fast instead of
/// piling up threads.
pub const CONTROL_QUEUE: usize = 8;

/// How long a control thread waits for the coordinator's outcome. The
/// client's own deadline is longer, so this arrives as a reply, not an EOF.
pub const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

pub const BUSY: &str = "daemon busy: the coordinator has not taken control requests \
    for a while; `infigraph daemon stop` falls back to the watch.stop sentinel";
pub const SHUTTING_DOWN: &str = "the daemon is shutting down";
pub const NO_CONTROL: &str = "this read service has no daemon control attached";

pub type ControlReply = std::result::Result<(), String>;

pub struct ControlMsg {
    pub request: ControlRequest,
    pub reply: mpsc::Sender<ControlReply>,
}

/// `Disabled` only when the persisted policy is off; a role that is merely
/// not running with the policy on is `Stopped`.
pub fn role_state(running: bool, policy_on: bool) -> RoleState {
    match (running, policy_on) {
        (true, _) => RoleState::Running,
        (false, true) => RoleState::Stopped,
        (false, false) => RoleState::Disabled,
    }
}

fn encode(s: RoleState) -> u8 {
    match s {
        RoleState::Running => 0,
        RoleState::Stopped => 1,
        RoleState::Disabled => 2,
        RoleState::NotOwned => 3,
    }
}

fn decode(v: u8) -> RoleState {
    match v {
        0 => RoleState::Running,
        1 => RoleState::Stopped,
        2 => RoleState::Disabled,
        _ => RoleState::NotOwned,
    }
}

pub struct DaemonState {
    code: AtomicU8,
    docs: AtomicU8,
    work_in_flight: AtomicBool,
    grace_secs: u64,
    idle_check_secs: u64,
}

impl DaemonState {
    fn new(grace_secs: u64, idle_check_secs: u64) -> Self {
        Self {
            code: AtomicU8::new(encode(RoleState::Stopped)),
            docs: AtomicU8::new(encode(RoleState::NotOwned)),
            work_in_flight: AtomicBool::new(false),
            grace_secs,
            idle_check_secs,
        }
    }

    fn slot(&self, role: WatchRole) -> Option<&AtomicU8> {
        match role {
            WatchRole::Code => Some(&self.code),
            WatchRole::Docs => Some(&self.docs),
            WatchRole::Daemon => None,
        }
    }

    /// `WatchRole::Daemon` has no state of its own and is ignored.
    pub fn set_role(&self, role: WatchRole, state: RoleState) {
        if let Some(slot) = self.slot(role) {
            slot.store(encode(state), Ordering::SeqCst);
        }
    }

    pub fn role(&self, role: WatchRole) -> RoleState {
        self.slot(role)
            .map(|s| decode(s.load(Ordering::SeqCst)))
            .unwrap_or(RoleState::NotOwned)
    }

    pub fn set_work_in_flight(&self, on: bool) {
        self.work_in_flight.store(on, Ordering::SeqCst);
    }

    pub fn report(&self, liveness: &Liveness, now_secs: u64) -> StatusReport {
        StatusReport {
            pid: std::process::id(),
            build: crate::build_hash().to_string(),
            leases: liveness.leases(),
            idle_secs: liveness.idle_for(now_secs).map(|d| d.as_secs()),
            grace_secs: self.grace_secs,
            idle_check_secs: self.idle_check_secs,
            work_in_flight: self.work_in_flight.load(Ordering::SeqCst),
            code: self.role(WatchRole::Code),
            docs: self.role(WatchRole::Docs),
        }
    }
}

pub struct ControlPort {
    tx: mpsc::SyncSender<ControlMsg>,
    pub state: DaemonState,
    in_flight: AtomicUsize,
}

/// Counts one control thread. The coordinator waits for the count to reach
/// zero after its loop ends, which is what lets a Daemon Stop reply reach
/// the client before the process exits -- without the service ever joining
/// a control thread (a #187 rebind drops the service on the coordinator's
/// own thread, and joining there would deadlock against it).
pub struct InFlightGuard(Arc<ControlPort>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ControlPort {
    pub fn new(grace_secs: u64, idle_check_secs: u64) -> (Arc<Self>, mpsc::Receiver<ControlMsg>) {
        let (tx, rx) = mpsc::sync_channel(CONTROL_QUEUE);
        let port = Arc::new(Self {
            tx,
            state: DaemonState::new(grace_secs, idle_check_secs),
            in_flight: AtomicUsize::new(0),
        });
        (port, rx)
    }

    /// Queue a request for the coordinator without blocking.
    pub fn submit(&self, request: ControlRequest) -> Result<mpsc::Receiver<ControlReply>, String> {
        let (reply, rx) = mpsc::channel();
        match self.tx.try_send(ControlMsg { request, reply }) {
            Ok(()) => Ok(rx),
            Err(mpsc::TrySendError::Full(_)) => Err(BUSY.to_string()),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(SHUTTING_DOWN.to_string()),
        }
    }

    pub fn enter(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlightGuard(self.clone())
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// `true` once no control thread is in flight, `false` if `timeout`
    /// passed first.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while self.in_flight() > 0 {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }
}
```

- [ ] **Step 4: Run the tests and check they pass** (the Step 2 command). Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon/control_port.rs crates/infigraph-core/src/daemon/mod.rs
git commit --no-verify -m "feat(core): ControlPort and DaemonState for socket control (#155)"
```

---

### Task 3: The read service serves `Status` and `Control`

**Files:**
- Modify: `crates/infigraph-core/src/daemon/read_service.rs` (`start_serving` L165-210, `serve_one` L253-341)
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (the one `start_serving` call, ~L685: pass `None` for now)
- Test: `crates/infigraph-core/tests/read_service.rs`

**Interfaces:**
- Consumes: Task 1's frames and helpers, Task 2's `ControlPort`, `ControlMsg`, `CONTROL_REPLY_TIMEOUT`, `SHUTTING_DOWN`, `NO_CONTROL`
- Produces: `ReadService::start_serving(root, source, docs, workers, liveness, control: Option<Arc<ControlPort>>)`. `start_with_sources` passes `None`.

- [ ] **Step 1: Write the failing tests.** Append to `tests/read_service.rs`:

```rust
use infigraph_core::daemon::control_port::{ControlMsg, ControlPort, BUSY, CONTROL_QUEUE};
use infigraph_core::daemon::liveness::{now_secs, Liveness};
use infigraph_core::daemon::read_protocol::{
    read_reply, write_op, ControlFrame, ControlRequest, OpReply, RoleState, StatusFrame,
    StatusReport, WatchAction, WatchRole,
};
use infigraph_core::daemon::read_service::ReadService;

/// A service with a control port and no graph at all.
fn control_service(
    root: &Path,
) -> (ReadService, Arc<ControlPort>, std::sync::mpsc::Receiver<ControlMsg>, Arc<Liveness>) {
    let liveness = Arc::new(Liveness::new());
    let (port, rx) = ControlPort::new(1800, 60);
    let svc = ReadService::start_serving(
        root,
        Arc::new(|| None),
        None,
        4,
        liveness.clone(),
        Some(port.clone()),
    )
    .unwrap();
    (svc, port, rx, liveness)
}

fn status(root: &Path) -> OpReply<StatusReport> {
    let mut s = ReadEndpoint::for_root(root).connect().unwrap();
    write_op(&mut s, &StatusFrame::default()).unwrap();
    read_reply(&mut s).unwrap().expect("a reply frame")
}

fn control(root: &Path, role: WatchRole, action: WatchAction) -> OpReply<()> {
    let mut s = ReadEndpoint::for_root(root).connect().unwrap();
    write_op(&mut s, &ControlFrame { control: ControlRequest { role, action } }).unwrap();
    read_reply(&mut s).unwrap().expect("a reply frame")
}

#[test]
fn status_answers_before_any_graph_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, port, _rx, _l) = control_service(dir.path());
    port.state.set_role(WatchRole::Code, RoleState::Running);
    let OpReply::Ok(r) = status(dir.path()) else { panic!("status must answer") };
    assert_eq!(r.code, RoleState::Running);
    assert_eq!(r.pid, std::process::id());
    svc.shutdown();
}

#[test]
fn status_and_control_do_not_count_as_activity_but_a_read_does() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _port, rx, liveness) = control_service(dir.path());
    let answer = std::thread::spawn(move || {
        let msg = rx.recv().unwrap();
        msg.reply.send(Ok(())).unwrap();
    });
    let then = now_secs() - 100;
    liveness.last_activity_for_test(then);
    let _ = status(dir.path());
    let _ = control(dir.path(), WatchRole::Code, WatchAction::Start);
    answer.join().unwrap();
    assert!(liveness.idle_for(now_secs()).unwrap().as_secs() >= 100);
    // A read with no graph is refused, but it still counts: it was a use.
    let _ = client_query(dir.path(), "RETURN 1");
    assert!(liveness.idle_for(now_secs()).unwrap().as_secs() < 100);
    svc.shutdown();
}

#[test]
fn a_control_reply_carries_the_coordinators_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _port, rx, _l) = control_service(dir.path());
    let answer = std::thread::spawn(move || {
        let ok = rx.recv().unwrap();
        assert_eq!(ok.request.action, WatchAction::Stop);
        ok.reply.send(Ok(())).unwrap();
        let err = rx.recv().unwrap();
        err.reply.send(Err("no doc-watch loop".into())).unwrap();
    });
    assert!(matches!(control(dir.path(), WatchRole::Code, WatchAction::Stop), OpReply::Ok(())));
    assert!(matches!(
        control(dir.path(), WatchRole::Docs, WatchAction::Stop),
        OpReply::Err(m) if m == "no doc-watch loop"
    ));
    answer.join().unwrap();
    svc.shutdown();
}

#[test]
fn control_beyond_the_queue_is_refused_at_once_and_status_still_answers() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, port, rx, _l) = control_service(dir.path());
    // Nobody drains `rx`: a wedged coordinator. Fill the queue.
    let root = dir.path().to_path_buf();
    let waiters: Vec<_> = (0..CONTROL_QUEUE)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || {
                let mut s = ReadEndpoint::for_root(&root).connect().unwrap();
                write_op(
                    &mut s,
                    &ControlFrame {
                        control: ControlRequest { role: WatchRole::Code, action: WatchAction::Stop },
                    },
                )
                .unwrap();
                s // keep the connection open
            })
        })
        .collect();
    let streams: Vec<_> = waiters.into_iter().map(|t| t.join().unwrap()).collect();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while port.in_flight() < CONTROL_QUEUE && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let started = std::time::Instant::now();
    assert!(matches!(
        control(dir.path(), WatchRole::Code, WatchAction::Stop),
        OpReply::Err(m) if m == BUSY
    ));
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    let started = std::time::Instant::now();
    assert!(matches!(status(dir.path()), OpReply::Ok(_)));
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    drop(rx); // pending control threads now see "shutting down" and finish
    drop(streams);
    svc.shutdown();
}

#[test]
fn dropping_the_service_does_not_wait_for_a_pending_control() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, port, _rx, _l) = control_service(dir.path());
    let mut s = ReadEndpoint::for_root(dir.path()).connect().unwrap();
    write_op(
        &mut s,
        &ControlFrame { control: ControlRequest { role: WatchRole::Code, action: WatchAction::Stop } },
    )
    .unwrap();
    while port.in_flight() == 0 {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let started = std::time::Instant::now();
    drop(svc);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "a #187 rebind drops the service on the coordinator's thread; it must not wait for control"
    );
}

#[test]
fn a_service_without_a_port_refuses_status_and_control() {
    let dir = tempfile::tempdir().unwrap();
    let svc = ReadService::start_with_sources(dir.path(), Arc::new(|| None), None, 2).unwrap();
    assert!(matches!(status(dir.path()), OpReply::Err(_)));
    assert!(matches!(control(dir.path(), WatchRole::Code, WatchAction::Stop), OpReply::Err(_)));
    svc.shutdown();
}
```

(`client_query` already exists in this file and is used above as `client_query(root, query)`; the test ignores its result.)

- [ ] **Step 2: Run them and check they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test read_service -- --test-threads=1`
Expected: compile error (`start_serving` takes 5 arguments).

- [ ] **Step 3: Thread the port through `start_serving`.** Add the parameter `control: Option<Arc<super::control_port::ControlPort>>` after `liveness`. `start_with_sources` passes `None`. In the accept loop, clone it per connection like `docs`, and pass `control.as_ref()` to `serve_one`. In `daemon/mod.rs`'s `start_serving` call (~L685), pass `None` for now.

- [ ] **Step 4: Dispatch in `serve_one`.** Change its signature to take `control: Option<&Arc<ControlPort>>` and replace its head (the `let req = match read_client_frame(...)` block, including the `Status | Control` placeholder from Task 1, and the `leases.liveness.touch();` line) with:

```rust
    let frame = read_client_frame(&mut stream)?;
    // One place applies the liveness rule for every op (#155): handlers
    // never touch it themselves.
    if frame.keeps_alive() == Some(true) {
        leases.liveness.touch();
    }
    let req = match frame {
        ClientFrame::Read(req) => req,
        ClientFrame::Attach(Attach { attach_pid }) => {
            park_lease(leases.clone(), attach_pid, stream);
            return Ok(());
        }
        ClientFrame::Status(_) => {
            let reply = match control {
                Some(port) => OpReply::Ok(port.state.report(&leases.liveness, now_secs())),
                None => OpReply::Err(NO_CONTROL.to_string()),
            };
            write_reply(&mut stream, &reply)?;
            return Ok(());
        }
        ClientFrame::Control(ControlFrame { control: request }) => {
            spawn_control(control.cloned(), request, stream);
            return Ok(());
        }
    };
```

Add below `park_lease`:

```rust
/// Runs one control request on its own thread, never a pool worker: it may
/// wait up to `CONTROL_REPLY_TIMEOUT` on a busy coordinator, and a few of
/// those on the pool would stop every read.
fn spawn_control(port: Option<Arc<ControlPort>>, request: ControlRequest, mut stream: ReadStream) {
    let Some(port) = port else {
        let _ = write_reply::<_, ()>(&mut stream, &OpReply::Err(NO_CONTROL.to_string()));
        return;
    };
    let guard = port.enter();
    let spawned = std::thread::Builder::new()
        .name("infigraph-control".into())
        .spawn(move || {
            let _guard = guard;
            let reply = match port.submit(request) {
                Err(msg) => OpReply::Err(msg),
                Ok(rx) => match rx.recv_timeout(CONTROL_REPLY_TIMEOUT) {
                    Ok(Ok(())) => OpReply::Ok(()),
                    Ok(Err(msg)) => OpReply::Err(msg),
                    Err(mpsc::RecvTimeoutError::Timeout) => OpReply::Err(format!(
                        "the coordinator did not answer within {}s",
                        CONTROL_REPLY_TIMEOUT.as_secs()
                    )),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        OpReply::Err(SHUTTING_DOWN.to_string())
                    }
                },
            };
            let _ = write_reply(&mut stream, &reply);
        });
    if let Err(e) = spawned {
        eprintln!("[control] could not start a control thread: {e}");
    }
}
```

Imports to add: `super::control_port::{ControlPort, CONTROL_REPLY_TIMEOUT, NO_CONTROL, SHUTTING_DOWN}`, `super::liveness::now_secs`, and from `read_protocol`: `write_reply, ControlFrame, ControlRequest, OpReply`.

- [ ] **Step 5: Run the tests and check they pass** (the Step 2 command). Expected: all pass, including the existing read-service tests.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/daemon/read_service.rs crates/infigraph-core/src/daemon/mod.rs crates/infigraph-core/tests/read_service.rs
git commit --no-verify -m "feat(core): the read service answers Status and hands Control to its port (#155)"
```

---

### Task 4: Client helper `daemon/control.rs`

**Files:**
- Create: `crates/infigraph-core/src/daemon/control.rs` (`pub mod control;` in `daemon/mod.rs`, first in the block)
- Modify: `crates/infigraph-core/src/daemon/read_endpoint.rs` (widen `LeaseShutdown` and `lease_shutdown` from `pub(crate)` only if the compiler requires it; `control.rs` is in the same crate, so it should not)
- Test: `crates/infigraph-core/tests/daemon_control_client.rs` (create)

**Interfaces:**
- Consumes: Task 1's frames and helpers, `ReadEndpoint::{for_root, connect, bind}`, `connect_allowing_for_startup`, `lifecycle::daemon_is_alive`, `lockfile::read_holder`
- Produces:
  - `pub const STATUS_DEADLINE: Duration = Duration::from_millis(500);`, `pub const CONTROL_DEADLINE: Duration = Duration::from_secs(35);`
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum ControlError { NoDaemon, Incompatible, Unresponsive, Refused(String) }` (`Display`, `std::error::Error`)
  - `pub fn query_status(root: &Path) -> Result<StatusReport, ControlError>`
  - `pub fn send_control(root: &Path, role: WatchRole, action: WatchAction) -> Result<(), ControlError>`
  - `pub fn query_status_many(roots: &[PathBuf]) -> Vec<Result<StatusReport, ControlError>>` (same order as `roots`)
  - `pub fn describe_status(root: &Path, result: &Result<StatusReport, ControlError>) -> String`

- [ ] **Step 1: Write the failing tests** in `tests/daemon_control_client.rs`:

```rust
//! The client half of #155's socket control, against fake listeners and a
//! real `ReadService` with a control port.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use infigraph_core::daemon::control::{
    describe_status, query_status, query_status_many, send_control, ControlError,
    STATUS_DEADLINE,
};
use infigraph_core::daemon::control_port::ControlPort;
use infigraph_core::daemon::liveness::Liveness;
use infigraph_core::daemon::read_endpoint::ReadEndpoint;
use infigraph_core::daemon::read_protocol::{WatchAction, WatchRole};
use infigraph_core::daemon::read_service::ReadService;

/// A listener on `root`'s real endpoint name that reads one frame and then
/// does whatever `then` says with the connection.
fn fake_daemon(
    root: &Path,
    then: impl Fn(infigraph_core::daemon::read_endpoint::ReadStream) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let listener = ReadEndpoint::for_root(root).bind().unwrap();
    std::thread::spawn(move || {
        while let Ok(Some(mut s)) = listener.accept_timeout(Duration::from_secs(5)) {
            let mut len = [0u8; 4];
            use std::io::Read as _;
            if s.read_exact(&mut len).is_ok() {
                let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
                let _ = s.read_exact(&mut body);
            }
            then(s);
        }
    })
}

#[test]
fn no_listener_and_no_watch_lock_is_no_daemon() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
    assert_eq!(query_status(dir.path()), Err(ControlError::NoDaemon));
    assert_eq!(
        send_control(dir.path(), WatchRole::Code, WatchAction::Stop),
        Err(ControlError::NoDaemon)
    );
}

#[test]
fn a_daemon_that_closes_without_replying_is_incompatible() {
    let dir = tempfile::tempdir().unwrap();
    let _fake = fake_daemon(dir.path(), drop);
    assert_eq!(query_status(dir.path()), Err(ControlError::Incompatible));
}

#[test]
fn a_daemon_that_never_replies_is_unresponsive_within_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let _fake = fake_daemon(dir.path(), |s| {
        std::thread::sleep(Duration::from_secs(3));
        drop(s);
    });
    let started = Instant::now();
    assert_eq!(query_status(dir.path()), Err(ControlError::Unresponsive));
    assert!(started.elapsed() < STATUS_DEADLINE + Duration::from_millis(500));
}

#[cfg(unix)]
#[test]
fn an_unresponsive_daemon_sees_the_client_hang_up() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let _fake = fake_daemon(dir.path(), move |mut s| {
        use std::io::Read as _;
        let mut buf = [0u8; 1];
        let started = Instant::now();
        let _ = s.read(&mut buf); // blocks until the client shuts the socket down
        tx.send(started.elapsed()).unwrap();
    });
    assert_eq!(query_status(dir.path()), Err(ControlError::Unresponsive));
    let waited = rx.recv_timeout(Duration::from_secs(2)).expect("the client must hang up");
    assert!(waited < Duration::from_secs(2));
}

#[test]
fn status_and_control_round_trip_against_a_real_service() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = ControlPort::new(1800, 60);
    let svc = ReadService::start_serving(
        dir.path(),
        Arc::new(|| None),
        None,
        2,
        Arc::new(Liveness::new()),
        Some(port),
    )
    .unwrap();
    let answer = std::thread::spawn(move || {
        rx.recv().unwrap().reply.send(Err("no doc-watch loop".into())).unwrap();
    });
    let report = query_status(dir.path()).unwrap();
    assert_eq!(report.pid, std::process::id());
    assert!(describe_status(dir.path(), &Ok(report)).contains("clients leasing: 0"));
    assert_eq!(
        send_control(dir.path(), WatchRole::Docs, WatchAction::Stop),
        Err(ControlError::Refused("no doc-watch loop".into()))
    );
    answer.join().unwrap();
    svc.shutdown();
}

#[test]
fn many_unresponsive_daemons_cost_about_one_deadline_not_one_each() {
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let _fakes: Vec<_> = dirs
        .iter()
        .map(|d| {
            fake_daemon(d.path(), |s| {
                std::thread::sleep(Duration::from_secs(3));
                drop(s);
            })
        })
        .collect();
    let roots: Vec<_> = dirs.iter().map(|d| d.path().to_path_buf()).collect();
    let started = Instant::now();
    let results = query_status_many(&roots);
    assert!(results.iter().all(|r| *r == Err(ControlError::Unresponsive)));
    assert!(started.elapsed() < STATUS_DEADLINE * 2);
}

#[test]
fn describe_status_names_the_holder_of_an_unresponsive_daemons_lock() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
    let lock = dir.path().join(".infigraph").join("watch.lock");
    let _held = infigraph_core::lockfile::try_acquire(&lock, "test-daemon").unwrap().unwrap();
    let text = describe_status(dir.path(), &Err(ControlError::Unresponsive));
    assert!(text.contains("role: test-daemon"), "{text}");
    assert!(describe_status(dir.path(), &Err(ControlError::NoDaemon))
        .starts_with("No watcher running for"));
}
```

- [ ] **Step 2: Run them and check they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_control_client -- --test-threads=1`
Expected: compile error (module `control` missing).

- [ ] **Step 3: Implement `daemon/control.rs`:**

```rust
//! The client side of socket control and status (#155, #202). The only code
//! that sends `Status` or `Control` frames: the CLI, MCP, doctor and `ps` all
//! come through here.
//!
//! Neither call takes a lease or starts a daemon. Checking on a daemon must
//! never keep it alive.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use super::read_endpoint::{connect_allowing_for_startup, ReadEndpoint, ReadStream};
use super::read_protocol::{
    read_reply, write_op, ControlFrame, ControlRequest, DaemonOp, OpReply, StatusFrame,
    StatusReport, WatchAction, WatchRole,
};

pub const STATUS_DEADLINE: Duration = Duration::from_millis(500);
/// Longer than the daemon's own `CONTROL_REPLY_TIMEOUT`, so a slow
/// coordinator arrives as that timeout's reply, not as a hang-up.
pub const CONTROL_DEADLINE: Duration = Duration::from_secs(35);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// Nothing is listening and nobody holds `watch.lock`.
    NoDaemon,
    /// The daemon closed without replying: it could not parse the frame, so
    /// its build is older or newer than this one.
    Incompatible,
    /// No reply within the deadline, or `watch.lock` is held with no
    /// listener (starting, or wedged).
    Unresponsive,
    /// The daemon answered with an error.
    Refused(String),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::NoDaemon => f.write_str("no daemon is running"),
            ControlError::Incompatible => f.write_str(
                "the daemon is an incompatible build; run `infigraph daemon-restart`",
            ),
            ControlError::Unresponsive => f.write_str(
                "the daemon is not responding; run `infigraph daemon-restart`",
            ),
            ControlError::Refused(msg) => write!(f, "the daemon refused: {msg}"),
        }
    }
}

impl std::error::Error for ControlError {}

fn watch_lock_held(root: &Path) -> bool {
    super::lifecycle::daemon_is_alive(&root.join(".infigraph").join("watch.lock"))
}

fn not_connected(root: &Path) -> ControlError {
    if watch_lock_held(root) {
        ControlError::Unresponsive
    } else {
        ControlError::NoDaemon
    }
}

pub fn query_status(root: &Path) -> Result<StatusReport, ControlError> {
    let stream = ReadEndpoint::for_root(root)
        .connect()
        .map_err(|_| not_connected(root))?;
    exchange(stream, &StatusFrame::default(), STATUS_DEADLINE)
}

pub fn send_control(root: &Path, role: WatchRole, action: WatchAction) -> Result<(), ControlError> {
    let stream = connect_allowing_for_startup(root).map_err(|_| not_connected(root))?;
    exchange(
        stream,
        &ControlFrame { control: ControlRequest { role, action } },
        CONTROL_DEADLINE,
    )
}

/// One status query per root, all at once, so a `ps` over many daemons
/// costs about one deadline.
pub fn query_status_many(roots: &[PathBuf]) -> Vec<Result<StatusReport, ControlError>> {
    let handles: Vec<_> = roots
        .iter()
        .cloned()
        .map(|root| std::thread::spawn(move || query_status(&root)))
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().unwrap_or(Err(ControlError::Unresponsive)))
        .collect()
}

/// Send `op` and wait at most `deadline` for its one reply frame.
///
/// The read runs on a helper thread so the deadline holds on every
/// transport. On unix a timed-out read is also woken with `shutdown(2)`, so
/// a wedged daemon never pins a thread in a long-lived client (MCP). The
/// handle sits behind a mutex the reader clears before dropping the stream:
/// never shut down an fd number that may since have been reused.
fn exchange<O: DaemonOp + 'static>(
    mut stream: ReadStream,
    op: &O,
    deadline: Duration,
) -> Result<O::Reply, ControlError>
where
    O::Reply: Send + 'static,
{
    write_op(&mut stream, op).map_err(|_| ControlError::Unresponsive)?;
    #[cfg(unix)]
    let hangup = Arc::new(Mutex::new(stream.lease_shutdown()));
    #[cfg(not(unix))]
    let hangup = Arc::new(Mutex::new(None::<()>));
    let reader_hangup = hangup.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let got = read_reply::<_, O::Reply>(&mut stream);
        reader_hangup.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(stream);
        let _ = tx.send(got);
    });
    match rx.recv_timeout(deadline) {
        Ok(Ok(Some(OpReply::Ok(v)))) => Ok(v),
        Ok(Ok(Some(OpReply::Err(msg)))) => Err(ControlError::Refused(msg)),
        Ok(Ok(None)) | Ok(Err(_)) => Err(ControlError::Incompatible),
        Err(_) => {
            #[cfg(unix)]
            if let Some(h) = hangup.lock().unwrap_or_else(|e| e.into_inner()).take() {
                h.shutdown();
            }
            #[cfg(not(unix))]
            let _ = hangup;
            Err(ControlError::Unresponsive)
        }
    }
}

/// What `watch-status` (CLI) and `get_watch_status` (MCP) print: one text,
/// so the two never drift.
pub fn describe_status(root: &Path, result: &Result<StatusReport, ControlError>) -> String {
    let shown = root.display();
    match result {
        Ok(report) => format!("Watcher active for {shown}\n{report}"),
        Err(ControlError::NoDaemon) => format!("No watcher running for {shown}."),
        Err(ControlError::Unresponsive) => {
            let lock = root.join(".infigraph").join("watch.lock");
            let holder = crate::lockfile::read_holder(&lock)
                .map(|h| format!("PID {} (role: {})", h.pid, h.role))
                .unwrap_or_else(|| "a process".to_string());
            format!(
                "Watcher for {shown}: {holder} holds watch.lock but is not answering on its \
                 socket (starting, or wedged). `infigraph daemon stop` falls back to the stop \
                 sentinel."
            )
        }
        Err(e) => format!("Watcher for {shown}: {e}"),
    }
}
```

Two notes for the implementer:
- `lease_shutdown` is `#[cfg(unix)] pub(crate)` on `ReadStream` and returns `Option<LeaseShutdown>`, so `hangup` holds an `Option<LeaseShutdown>` on unix. `LeaseShutdown` wraps a `RawFd` (`i32`), so it is already `Send`.
- Check that `lockfile::read_holder` returns a struct with `pid` and `role` fields (it does in `tool_get_watch_status`, `crates/infigraph-mcp/src/tools/watch.rs:543`).

- [ ] **Step 4: Run the tests and check they pass** (the Step 2 command). Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon/control.rs crates/infigraph-core/src/daemon/mod.rs crates/infigraph-core/src/daemon/read_endpoint.rs crates/infigraph-core/tests/daemon_control_client.rs
git commit --no-verify -m "feat(core): query_status and send_control, the only socket-control client (#155)"
```

---

### Task 5: The coordinator serves socket control

**Files:**
- Modify: `crates/infigraph-core/src/daemon/mod.rs`:
  - `DocsControl` (L288) becomes the `DocsHandle` trait
  - `run_write_coordinator` (L585-1783): creates the port, moves `daemon_idle_settings` up, `recv_timeout`, state refresh, post-loop drain
  - `route_or_serve_request` (L3178; `WatchControl` arm ~L3390): now calls `apply_watch_control`
- Modify: `crates/infigraph-core/src/watch/mod.rs` (`CodeWatch`, L88-138): add `is_running`
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`cmd_daemon` ~L603-625, `DocWatchThread` L811-862): implement `DocsHandle`
- Modify: `crates/infigraph-core/tests/watch_daemon.rs` (`watch_control_docs_role_dispatches_to_the_registered_docs_control`, ~L1446): build a `DocsHandle` instead of a closure
- Create: `crates/infigraph-core/tests/daemon_control.rs`

**Interfaces:**
- Consumes: Tasks 1–4
- Produces:
  - `pub trait DocsHandle: Send + Sync { fn control(&self, action: WatchAction) -> std::result::Result<(), String>; fn is_running(&self) -> bool; }`
  - `run_write_coordinator`'s `docs_control` parameter becomes `Option<Arc<dyn DocsHandle>>`. Callers passing `None` are unchanged.
  - `pub(crate) fn CodeWatch::is_running(&self) -> bool`
  - `fn apply_watch_control(role: WatchRole, action: WatchAction, code_watch: &mut CodeWatch, docs: Option<&Arc<dyn DocsHandle>>) -> std::result::Result<(), String>` (private)
  - Test-only env var `INFIGRAPH_TEST_COORDINATOR_STALL_FILE`: while the named file exists, the coordinator loop sleeps at the top of each iteration and takes no control requests.

- [ ] **Step 1: Write the failing tests** in `tests/daemon_control.rs`:

```rust
//! #155: a real write coordinator served over its socket.

use std::path::Path;
use std::time::{Duration, Instant};

use infigraph_core::daemon::control::{query_status, send_control, ControlError};
use infigraph_core::daemon::read_protocol::{RoleState, WatchAction, WatchRole};

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Daemon {
    handle: std::thread::JoinHandle<anyhow::Result<()>>,
    token: tokio_util::sync::CancellationToken,
    _stop_tx: std::sync::mpsc::Sender<()>,
}

fn start(root: &Path) -> Daemon {
    std::fs::write(root.join("main.py"), "def main():\n    pass\n").unwrap();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let token = tokio_util::sync::CancellationToken::new();
    let t = token.clone();
    let r = root.to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &r,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50,
            stop_rx,
            |_| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true,
            None,
            &t,
            None,
            None,
        )
    });
    // The endpoint binds before the registry build; control waits until the
    // loop is taking requests, which is after that build.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match send_control(root, WatchRole::Code, WatchAction::Start) {
            Ok(()) => break,
            Err(e) if Instant::now() > deadline => panic!("daemon never took control: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    Daemon { handle, token, _stop_tx: stop_tx }
}

fn stop(d: Daemon) {
    d.token.cancel();
    let _ = d._stop_tx.send(());
    let _ = d.handle.join();
}

#[test]
fn daemon_stop_over_the_socket_replies_ok_then_exits() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    assert_eq!(send_control(dir.path(), WatchRole::Daemon, WatchAction::Stop), Ok(()));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !d.handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(d.handle.is_finished(), "Daemon Stop must end the coordinator loop");
    assert!(d.token.is_cancelled(), "a daemon stop must cancel background work");
    d.handle.join().unwrap().unwrap();
}

#[test]
fn daemon_start_is_refused_without_stopping_the_loop() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    assert!(matches!(
        send_control(dir.path(), WatchRole::Daemon, WatchAction::Start),
        Err(ControlError::Refused(m)) if m.contains("only supports Stop/Restart")
    ));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!d.handle.is_finished());
    stop(d);
}

#[test]
fn docs_without_a_handle_is_refused_and_reported_not_owned() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    assert!(matches!(
        send_control(dir.path(), WatchRole::Docs, WatchAction::Stop),
        Err(ControlError::Refused(_))
    ));
    assert_eq!(query_status(dir.path()).unwrap().docs, RoleState::NotOwned);
    stop(d);
}

#[test]
fn code_state_goes_running_stopped_disabled() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    assert_eq!(query_status(dir.path()).unwrap().code, RoleState::Running);
    send_control(dir.path(), WatchRole::Code, WatchAction::Stop).unwrap();
    assert_eq!(query_status(dir.path()).unwrap().code, RoleState::Stopped);
    // Disable's contract: the caller persists the policy first, then tells the daemon.
    infigraph_core::watch::config::write_watch_policy(dir.path(), WatchRole::Code, false).unwrap();
    send_control(dir.path(), WatchRole::Code, WatchAction::Disable).unwrap();
    assert_eq!(query_status(dir.path()).unwrap().code, RoleState::Disabled);
    stop(d);
}

#[test]
fn a_control_round_trip_on_an_idle_daemon_beats_one_tick() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    let mut samples: Vec<Duration> = (0..5)
        .map(|_| {
            let t = Instant::now();
            send_control(dir.path(), WatchRole::Code, WatchAction::Start).unwrap();
            t.elapsed()
        })
        .collect();
    samples.sort();
    let median = samples[2];
    stop(d);
    // COORDINATOR_TICK is 200ms. Before #155 a control waited out the tick.
    assert!(median < Duration::from_millis(200), "median {median:?} of {samples:?}");
}

#[test]
fn a_stalled_coordinator_still_answers_status_and_refuses_control_when_full() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let stall = dir.path().join("stall");
    std::env::set_var("INFIGRAPH_TEST_COORDINATOR_STALL_FILE", &stall);
    let d = start(dir.path());
    std::fs::write(&stall, b"").unwrap();
    std::thread::sleep(Duration::from_millis(400)); // let the loop reach the stall
    let root = dir.path().to_path_buf();
    let queued: Vec<_> = (0..infigraph_core::daemon::control_port::CONTROL_QUEUE)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || send_control(&root, WatchRole::Code, WatchAction::Start))
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    assert!(query_status(dir.path()).is_ok());
    assert!(t.elapsed() < Duration::from_millis(500));
    let t = Instant::now();
    assert!(matches!(
        send_control(dir.path(), WatchRole::Code, WatchAction::Start),
        Err(ControlError::Refused(m)) if m.contains("busy")
    ));
    assert!(t.elapsed() < Duration::from_millis(500));
    std::fs::remove_file(&stall).unwrap();
    for q in queued {
        assert_eq!(q.join().unwrap(), Ok(()));
    }
    std::env::remove_var("INFIGRAPH_TEST_COORDINATOR_STALL_FILE");
    stop(d);
}
```

- [ ] **Step 2: Run them and check they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_control -- --test-threads=1`
Expected: every test fails. `start` never gets `Ok` (no port attached, so Control is `Err(NO_CONTROL)`), and it panics after 90s. To avoid waiting, run a single test first and confirm the panic message names `this read service has no daemon control attached`.

- [ ] **Step 3: `CodeWatch::is_running`.** In `watch/mod.rs`, after `stop`:

```rust
    /// Whether a producer is live. The same check `start()` makes, so a
    /// producer that ended by itself reads as not running.
    pub(crate) fn is_running(&self) -> bool {
        self.task.as_ref().is_some_and(|t| !t.is_finished())
    }
```

- [ ] **Step 4: `DocsHandle`.** Replace the `DocsControl` type alias (keep and adapt its doc comment) with:

```rust
pub trait DocsHandle: Send + Sync {
    /// Act on a `Control { role: Docs, .. }` request. `Err(msg)` is the
    /// reply the client gets.
    fn control(&self, action: WatchAction) -> std::result::Result<(), String>;
    /// Whether the doc-watch loop is live, for `Status`.
    fn is_running(&self) -> bool;
}
```

Change `run_write_coordinator`'s parameter to `docs_control: Option<Arc<dyn DocsHandle>>` and `route_or_serve_request`'s to `docs_control: Option<&Arc<dyn DocsHandle>>`.

In `crates/infigraph-cli/src/info_commands.rs`, replace the `docs_control` closure (~L603-625) with a newtype implementing the trait, and add an `is_running` to `DocWatchThread`:

```rust
/// Lets `Control { role: Docs, .. }` reach this thread from the coordinator,
/// which lives in infigraph-core and knows nothing about doc-watching.
struct DocWatchHandle(std::sync::Arc<std::sync::Mutex<DocWatchThread>>);

impl infigraph_core::daemon::DocsHandle for DocWatchHandle {
    fn control(
        &self,
        action: infigraph_core::daemon_protocol::WatchAction,
    ) -> std::result::Result<(), String> {
        use infigraph_core::daemon_protocol::WatchAction;
        let mut doc_watch = self.0.lock().unwrap();
        match action {
            WatchAction::Stop | WatchAction::Disable => doc_watch.stop(),
            WatchAction::Start | WatchAction::Enable => doc_watch.start(),
            WatchAction::Restart => {
                doc_watch.stop();
                doc_watch.start();
            }
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.0.lock().unwrap().is_running()
    }
}
```

Build it at the old site as `let docs_control: std::sync::Arc<dyn infigraph_core::daemon::DocsHandle> = std::sync::Arc::new(DocWatchHandle(std::sync::Arc::clone(&doc_watch)));`. In `impl DocWatchThread`:

```rust
    fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
```

Update `watch_daemon.rs`'s `watch_control_docs_role_dispatches_to_the_registered_docs_control` (~L1465) in the same way: a test struct that records actions in its `Mutex<Vec<WatchAction>>` and returns `false` from `is_running`.

- [ ] **Step 5: Extract `apply_watch_control`.** Move the body of the `WatchControl` arm's `let outcome = match role { … };` (~L3391-3425) into a free function, unchanged:

```rust
/// What a `Control` request does to this daemon's watch roles. `Daemon`'s
/// Stop/Restart only answer `Ok` here; the caller ends the loop.
fn apply_watch_control(
    role: WatchRole,
    action: WatchAction,
    code_watch: &mut CodeWatch,
    docs: Option<&Arc<dyn DocsHandle>>,
) -> std::result::Result<(), String> {
    match role {
        // (the existing three arms, verbatim, with `docs_control` renamed to
        // `docs` and `control(action)` becoming `handle.control(action)`)
    }
}

fn is_daemon_stop(role: WatchRole, action: WatchAction, outcome: &std::result::Result<(), String>) -> bool {
    matches!(
        (role, action, outcome),
        (WatchRole::Daemon, WatchAction::Stop | WatchAction::Restart, Ok(()))
    )
}
```

The file-drop arm becomes (it is deleted in Task 9):

```rust
        WriteRequest::WatchControl { role, action } => {
            let outcome = apply_watch_control(role, action, code_watch, docs_control);
            let daemon_stop = is_daemon_stop(role, action, &outcome);
            reply_to_watch_control(&reply_path, outcome);
            std::fs::remove_file(path).ok();
            if daemon_stop {
                *shutdown_requested = true;
                daemon_token.cancel();
            }
            None
        }
```

Keep the existing comment explaining the two separate stop signals, at the socket call site in Step 6.

- [ ] **Step 6: Wire the port into `run_write_coordinator`.**

(a) Move the `let idle = daemon_idle_settings(root);` line (~L888) up to just before `let liveness = Arc::new(…)` (~L656), and create the port there:

```rust
    let idle = daemon_idle_settings(root);
    // #155: what `Status` reads, and the channel `Control` reaches this loop
    // through. Owned here, like `liveness`, so a #187 rebind keeps it.
    let (control_port, control_rx) =
        control_port::ControlPort::new(idle.grace_secs, idle.check_secs.max(1));
```

Leave the `idle_grace`/`idle_check` lines where they are; they now read the moved `idle`. In `bind_read_service`, clone `control_port` next to `liveness` and pass `Some(control_port.clone())` as `start_serving`'s new last argument.

(b) After `code_watch` is constructed and first started, and wherever `docs_control` is in scope, add a closure-free helper at file scope, and call it once before the loop:

```rust
/// Publish the watch roles' state for `Status`. Cheap: two atomics, plus a
/// config read only when asked.
fn publish_roles(
    state: &control_port::DaemonState,
    code_watch: &CodeWatch,
    docs: Option<&Arc<dyn DocsHandle>>,
    policy: [bool; 2],
) {
    state.set_role(WatchRole::Code, control_port::role_state(code_watch.is_running(), policy[0]));
    state.set_role(
        WatchRole::Docs,
        match docs {
            Some(d) => control_port::role_state(d.is_running(), policy[1]),
            None => RoleState::NotOwned,
        },
    );
}

fn read_policy(root: &Path) -> [bool; 2] {
    [
        crate::watch::config::watch_enabled_at(root, "watch"),
        crate::watch::config::watch_enabled_at(root, "watch_docs"),
    ]
}
```

Before the loop: `let mut policy = read_policy(root); publish_roles(&control_port.state, &code_watch, docs_control.as_ref(), policy);`.

(c) At the top of the loop body, add the test stall hook:

```rust
        // Test-only: park the loop so tests can observe a busy coordinator.
        if let Ok(stall) = std::env::var("INFIGRAPH_TEST_COORDINATOR_STALL_FILE") {
            while Path::new(&stall).exists() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
```

(d) Replace `std::thread::sleep(COORDINATOR_TICK);` (~L1723) with:

```rust
        control_port.state.set_work_in_flight(
            drain_in_flight.is_some()
                || full_reindex_in_flight.is_some()
                || scip_in_flight.is_some()
                || scip_import_in_flight.is_some(),
        );
        publish_roles(&control_port.state, &code_watch, docs_control.as_ref(), policy);

        // #155: wait on the control channel instead of sleeping, so a control
        // request wakes the loop at once. Everything queued is served now.
        let mut next = control_rx.recv_timeout(COORDINATOR_TICK).ok();
        while let Some(msg) = next.take() {
            let control_port::ControlMsg { request, reply } = msg;
            let outcome = apply_watch_control(
                request.role,
                request.action,
                &mut code_watch,
                docs_control.as_ref(),
            );
            let daemon_stop = is_daemon_stop(request.role, request.action, &outcome);
            if matches!(request.action, WatchAction::Enable | WatchAction::Disable) {
                policy = read_policy(root);
            }
            publish_roles(&control_port.state, &code_watch, docs_control.as_ref(), policy);
            // Replied before any teardown starts, so the client learns the
            // stop was accepted.
            let _ = reply.send(outcome);
            if daemon_stop {
                // Two separate signals, deliberately: the token tears down
                // background work, the flag ends this loop. See
                // `shutdown_requested`'s declaration.
                shutdown_requested = true;
                daemon_token.cancel();
                break;
            }
            next = control_rx.try_recv().ok();
        }
        // The loop's own `if shutdown_requested` check (~L1590) sits mid-body,
        // after work that could start a drain; leave now instead.
        if shutdown_requested {
            break;
        }
```

`shutdown_requested` is the loop's local `let mut shutdown_requested = false;` (~L865), so it is assigned directly here. The file-drop arm's `Enable`/`Disable` does not refresh `policy`; it does not need to, because Task 9 deletes that arm.

(e) Directly after the loop ends (before `code_watch.stop();`, ~L1728):

```rust
    // Refuse new control requests, and let the ones in flight finish writing
    // their replies (a Daemon Stop's above all) before teardown and exit.
    drop(control_rx);
    if !control_port.wait_idle(Duration::from_secs(2)) {
        eprintln!("[control] {} control reply(s) still in flight at shutdown", control_port.in_flight());
    }
```

- [ ] **Step 7: Run the new tests and check they pass**

Run: the Step 2 command.
Expected: 6 passed.

- [ ] **Step 8: Run the existing coordinator suites** (the file-drop path still works until Task 9)

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test watch_daemon --test watch_control --test watch_control_helper --test read_service --test daemon_protocol_watcher_wiring -- --test-threads=1`
Then: `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test watch_daemon_docs -- --test-threads=1`
Expected: all pass.

- [ ] **Step 9: Commit**

```bash
git add crates/infigraph-core/src/daemon/mod.rs crates/infigraph-core/src/watch/mod.rs crates/infigraph-cli/src/info_commands.rs crates/infigraph-core/tests/watch_daemon.rs crates/infigraph-core/tests/daemon_control.rs
git commit --no-verify -m "feat(core): the coordinator wakes on socket control and publishes role state (#155)"
```

---

### Task 6: CLI and MCP send control over the socket

**Files:**
- Modify: `crates/infigraph-cli/src/info_commands.rs`:
  - `cmd_daemon_stop` (~L901-921)
  - `cmd_daemon_restart` (~L923-986)
  - `cmd_watch_control` (~L998-1034)
  - remove `WATCH_CONTROL_TIMEOUT` (L899) when it is no longer used
- Modify: `crates/infigraph-mcp/src/tools/watch.rs` (`watch_control` L462-541; remove `WATCH_CONTROL_TIMEOUT` L436 when unused)
- Test: `crates/infigraph-cli/tests/daemon_control_cli.rs` (create)

**Interfaces:**
- Consumes: `infigraph_core::daemon::control::{send_control, ControlError}`
- Produces: `pub(crate) fn stop_via_sentinel(root: &Path) -> Result<()>` in `info_commands.rs` (writes `.infigraph/watch.stop`; the same write `cmd_watch_stop` does, which is refactored to call it)

- [ ] **Step 1: Write the failing test** in `tests/daemon_control_cli.rs`. It drives the real CLI binary against a fake incompatible daemon: a listener that closes without replying while `watch.lock` is held.

```rust
//! #155: `infigraph daemon stop` falls back to the watch.stop sentinel when
//! the daemon cannot parse a Control frame.

use std::time::Duration;

#[test]
fn daemon_stop_against_an_incompatible_daemon_writes_the_sentinel() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let lock = root.join(".infigraph").join("watch.lock");
    let _held = infigraph_core::lockfile::try_acquire(&lock, "old-daemon").unwrap().unwrap();
    let listener =
        infigraph_core::daemon::read_endpoint::ReadEndpoint::for_root(&root).bind().unwrap();
    let fake = std::thread::spawn(move || {
        // Accept one connection and close it unanswered: an incompatible build.
        if let Ok(Some(s)) = listener.accept_timeout(Duration::from_secs(20)) {
            drop(s);
        }
    });

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["daemon", "stop"])
        .current_dir(&root)
        .env("INFIGRAPH_BACKEND", "kuzu")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .unwrap();
    fake.join().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(root.join(".infigraph").join("watch.stop").exists());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("incompatible"), "{stdout}");
}
```

Check the real subcommand spelling for daemon stop in `crates/infigraph-cli/src/main.rs` (`Commands::Daemon…`) and adjust `args` if it differs. `watch.lock` is held by the test process, and the CLI runs as a separate process, so its `daemon_is_alive` sees the lock as held.

- [ ] **Step 2: Run it and check it fails**

Run: `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test daemon_control_cli -- --test-threads=1`
Expected: FAIL. Today's CLI drops a request file and waits 30s for a result, then errors with no sentinel written.

- [ ] **Step 3: Implement the CLI side.**

```rust
/// The out-of-band stop (#155): works when the daemon cannot take a control
/// frame, because the coordinator checks for this file on its own.
pub(crate) fn stop_via_sentinel(root: &Path) -> Result<()> {
    std::fs::write(root.join(".infigraph").join("watch.stop"), b"")?;
    Ok(())
}
```

`cmd_watch_stop` calls `stop_via_sentinel(root)?` in place of its own `std::fs::write`.

`cmd_daemon_stop`, after the existing `daemon_is_alive` early return. Delete the `bundled_registry`/`Infigraph::open` lines, since control no longer needs a graph:

```rust
    use infigraph_core::daemon::control::{send_control, ControlError};
    match send_control(root, WatchRole::Daemon, WatchAction::Stop) {
        Ok(()) => println!("Daemon stopped."),
        Err(ControlError::NoDaemon) => println!("No daemon running."),
        Err(e @ (ControlError::Incompatible | ControlError::Unresponsive)) => {
            stop_via_sentinel(root)?;
            println!("Daemon did not take the stop request ({e}); wrote the stop sentinel instead.");
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
```

`cmd_daemon_restart`: replace its `submit_watch_control_and_await` call (and the two lines opening an `Infigraph`) with the same `match`, except that `Ok(())` and the sentinel arm print nothing and fall through to the existing `confirm_daemon_exited` block. `NoDaemon` also falls through (nothing to wait for). `Refused` returns the error.

`cmd_watch_control`, after the existing `daemon_is_alive` early return. Delete the `Infigraph::open` lines:

```rust
    infigraph_core::daemon::control::send_control(root, role, watch_action)?;
    println!("{role:?}: {watch_action:?} done.");
    Ok(())
```

Its `ControlError` messages (Task 4's `Display`) already say to run `daemon-restart` for `Incompatible`/`Unresponsive`.

- [ ] **Step 4: Implement the MCP side.** In `watch_control`'s daemon-mode branch, replace the three lines from `let registry = bundled_registry()?;` to `prism.submit_watch_control_and_await(...)?;` with:

```rust
        infigraph_core::daemon::control::send_control(&root, role, action)?;
```

and change the returned text from `"… sent for …"` to `format!("{role:?}: {action:?} done for {root_str}.")`. Check `crates/infigraph-mcp/tests/watcher_daemon_mode.rs` for assertions on the old `"sent for"` text and update them to `"done for"`.

- [ ] **Step 5: Run the tests and check they pass**

Run: `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test daemon_control_cli --test watch_daemon_docs -- --test-threads=1`
Then: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-mcp --test watcher_daemon_mode -- --test-threads=1`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-cli/src/info_commands.rs crates/infigraph-cli/tests/daemon_control_cli.rs crates/infigraph-mcp/src/tools/watch.rs crates/infigraph-mcp/tests/watcher_daemon_mode.rs
git commit --no-verify -m "feat(cli,mcp): control goes over the socket, stop falls back to the sentinel (#155)"
```

---

### Task 7: Doc-watch stop goes over control; delete `watch.stop.docs`

Spec D7. The sentinel is a second control path that parks the doc-watch loop in a suppressed state `Status` cannot see.

**Files:**
- Modify: `crates/infigraph-docs/src/watch.rs`:
  - `watch_docs_daemon_loop` and its doc comment, L162-236
  - `run_attached_cycle`, L237-301
  - tests: `returns_immediately_when_shutdown_already_set` L428, `does_not_attach_without_docs_kuzu` L439, `attaches_and_indexes_once_docs_kuzu_appears` L462, and L551-783
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`DocWatchThread` L811-862: drop `resume`)
- Modify: `crates/infigraph-mcp/src/tools/docs.rs` (`tool_stop_watch_docs`'s `path` branch, L660-679)
- Modify: `crates/infigraph-mcp/tests/watcher_daemon_mode.rs` (delete `stop_watch_docs_by_path_writes_sentinel_when_daemon_alive` L595-622; keep `…_reports_no_watcher_when_lock_free` L745-758)
- Modify: `crates/infigraph-cli/tests/watch_daemon_docs.rs` (`watch_docs_start_resumes_after_a_sentinel_triggered_stop` L521-688)

**Interfaces:**
- Consumes: `control::{send_control, query_status, ControlError}`, `RoleState`, Task 5's `DocsHandle` impl (`DocWatchHandle`)
- Produces: `pub fn watch_docs_daemon_loop(root: &Path, debounce_ms: u64, shutdown: Arc<AtomicBool>) -> Result<()>` (no `resume`), and `fn run_attached_cycle<F>(docs_kuzu: &Path, shutdown: &Arc<AtomicBool>, poll: Duration, watch_fn: F)` (no sentinel, returns `()`)

- [ ] **Step 1: Write the failing tests.**

In `crates/infigraph-docs/src/watch.rs`'s tests, delete these three tests: they pin behaviour this task removes.
- `detaches_on_stop_sentinel_and_does_not_immediately_reattach`
- `resume_signal_reattaches_a_suppressed_loop_without_docs_kuzu_cycling`
- `a_resume_armed_while_attached_does_not_cancel_the_next_explicit_stop`

Then add, reusing that module's `FastPoll` guard and the setup lines the deleted tests used:

```rust
    #[test]
    fn a_stray_stop_docs_file_no_longer_detaches_the_loop() {
        let _poll = FastPoll::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        crate::DocIndex::open(&root).unwrap().init().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let root_clone = root.clone();
        let handle = std::thread::spawn(move || {
            watch_docs_daemon_loop(&root_clone, 50, shutdown_clone)
        });
        std::thread::sleep(Duration::from_millis(300));
        // What a pre-#155 MCP server would have written.
        std::fs::write(root.join(".infigraph").join("watch.stop.docs"), b"").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            root.join(".infigraph").join("watch.stop.docs").exists(),
            "the loop must not consume (or act on) the retired sentinel"
        );
        shutdown.store(true, Ordering::Relaxed);
        handle.join().unwrap().unwrap();
    }
```

Change `run_attached_cycle_reports_non_sticky_when_watch_fn_exits_unrequested` (L750) to `run_attached_cycle_returns_when_watch_fn_exits_unrequested`: drop the `stop_sentinel` argument and the `sticky` assertion, and keep its "returns rather than blocking forever" assertion. Update the other three calls at L433, L448 and L473 to drop their `resume` argument.

In `crates/infigraph-cli/tests/watch_daemon_docs.rs`, rename `watch_docs_start_resumes_after_a_sentinel_triggered_stop` to `watch_docs_stop_and_start_over_control_are_visible_in_status`. Keep its daemon harness and its "a doc edit after restart gets indexed" assertion. Replace the sentinel write and whatever it used to trigger the resume (around L619-640) with:

```rust
    infigraph_core::daemon::control::send_control(
        &root,
        infigraph_core::daemon::read_protocol::WatchRole::Docs,
        infigraph_core::daemon::read_protocol::WatchAction::Stop,
    )
    .unwrap();
    assert_eq!(
        infigraph_core::daemon::control::query_status(&root).unwrap().docs,
        infigraph_core::daemon::read_protocol::RoleState::Stopped
    );
    infigraph_core::daemon::control::send_control(
        &root,
        infigraph_core::daemon::read_protocol::WatchRole::Docs,
        infigraph_core::daemon::read_protocol::WatchAction::Start,
    )
    .unwrap();
    assert_eq!(
        infigraph_core::daemon::control::query_status(&root).unwrap().docs,
        infigraph_core::daemon::read_protocol::RoleState::Running
    );
```

Update the test's doc comment to say what it now pins.

In `crates/infigraph-mcp/tests/watcher_daemon_mode.rs`:
- delete `stop_watch_docs_by_path_writes_sentinel_when_daemon_alive`;
- in `stop_watch_docs_by_path_reports_no_watcher_when_lock_free`, keep its assertions (`"No watcher running."`, and no `watch.stop.docs` file).

- [ ] **Step 2: Run them and check they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --lib watch -- --test-threads=1`
Expected: compile errors (`watch_docs_daemon_loop` still takes `resume`).

- [ ] **Step 3: Simplify the loop.** In `crates/infigraph-docs/src/watch.rs`:

```rust
/// Drive doc-watching for `root` as part of the merged code+doc daemon (see
/// `infigraph_core::daemon::lifecycle`). Attaches a `watch_docs` session once
/// `.infigraph/docs.kuzu` exists, detaches if that file disappears (e.g.
/// after `clean_docs`) and re-attaches when it comes back. Exits once
/// `shutdown` is set. Blocks until then.
///
/// Stopping and starting doc-watching is the daemon's `Control(Docs, …)`
/// (#155): stop sets `shutdown` and joins this thread, start spawns a new
/// one. There is deliberately no stop file: a loop paused by a file is a
/// state the daemon cannot report.
pub fn watch_docs_daemon_loop(
    root: &Path,
    debounce_ms: u64,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let docs_kuzu = root.join(".infigraph").join("docs.kuzu");
    let poll = attach_poll_interval(root);
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        if !docs_kuzu.exists() {
            std::thread::sleep(poll);
            continue;
        }
        let root_owned = root.to_path_buf();
        eprintln!(
            "[doc-watch-daemon] attaching doc watcher for {}",
            root.display()
        );
        run_attached_cycle(&docs_kuzu, &shutdown, poll, move |stop_rx| {
            watch_docs(&root_owned, debounce_ms, stop_rx, "doc-watch-daemon")
        });
    }
}
```

In `run_attached_cycle`, remove the `stop_sentinel` parameter, the `if stop_sentinel.exists() { … }` block and the `bool` return (every `return false;` becomes `return;`). Rewrite its doc comment's last paragraph: it returns when `watch_fn` exits on its own, on `shutdown`, or when `docs_kuzu` disappears.

- [ ] **Step 4: `DocWatchThread` loses `resume`.** In `info_commands.rs`, delete the `resume` field and its initialisations. `start()` becomes:

```rust
    fn start(&mut self) {
        // Running already: nothing to do -- there is no paused-but-alive
        // state any more (#155 removed the stop file that created one).
        if self.is_running() {
            return;
        }
        // A self-terminated loop (panic) is respawned, mirroring
        // `CodeWatch::start()`'s `is_finished()` guard (c9dae4b).
        self.handle.take();
        self.shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let root = self.root.clone();
        let debounce = self.debounce;
        let shutdown = std::sync::Arc::clone(&self.shutdown);
        self.handle = Some(std::thread::spawn(move || {
            if let Err(e) = infigraph_docs::watch::watch_docs_daemon_loop(&root, debounce, shutdown) {
                eprintln!("[doc-watch-daemon] error: {e}");
            }
        }));
    }
```

Update the struct's doc comment: it bundles the thread and its shutdown flag, and each start gets a fresh flag.

- [ ] **Step 5: MCP `stop_watch_docs(path)` goes over control.** Replace the `path` branch body after `let root_str = …;` (the lock probe and sentinel write, L662-679) with:

```rust
        use infigraph_core::daemon::control::{send_control, ControlError};
        return match send_control(&root, WatchRole::Docs, WatchAction::Stop) {
            Ok(()) => Ok(format!(
                "Doc watcher on {root_str} stopped (the code watcher, if any, is unaffected)."
            )),
            Err(ControlError::NoDaemon) => Ok("No watcher running.".to_string()),
            Err(e) => Err(e.into()),
        };
```

(`WatchRole`/`WatchAction` are already imported in this file for `enable_watch_docs`.)

- [ ] **Step 6: Check nothing else references the sentinel**

Run `mcp__infigraph__search` with `regex=true` for `watch\.stop\.docs|suppressed_until_absent`.
Expected: matches only in `docs/`, plus the new test's string literal.

- [ ] **Step 7: Run the tests and check they pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs -- --test-threads=1`
Then: `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test watch_daemon_docs -- --test-threads=1`
Then: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-mcp --test watcher_daemon_mode -- --test-threads=1`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add crates/infigraph-docs/src/watch.rs crates/infigraph-cli/src/info_commands.rs crates/infigraph-cli/tests/watch_daemon_docs.rs crates/infigraph-mcp/src/tools/docs.rs crates/infigraph-mcp/tests/watcher_daemon_mode.rs
git commit --no-verify -m "refactor: doc-watch stop goes over control; delete the watch.stop.docs sentinel (#155)"
```

---

### Task 8: Status consumers: `watch-status`, `get_watch_status`, doctor, `ps`

**Files:**
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`cmd_watch_status` L884-893, `cmd_ps` L1545-1588)
- Modify: `crates/infigraph-mcp/src/tools/watch.rs` (`tool_get_watch_status`'s `path` branch, L544-573)
- Modify: `crates/infigraph-core/src/doctor.rs` (`check_one_watcher` L667-772; delete `project_has_live_mcp_instance` L781-795)

**Interfaces:**
- Consumes: `control::{query_status, query_status_many, describe_status, ControlError}`, `StatusReport`
- Produces: `fn watcher_verdict(label: String, report: &StatusReport, log: &Path) -> CheckResult` (private, in `doctor.rs`)

- [ ] **Step 1: Write the failing doctor tests** in `doctor.rs`'s test module (use its existing `CheckResult` status accessor; check how neighbouring tests assert pass/warn, e.g. `docs_sidecar_older_than_the_doc_store_warns_with_the_doc_reindex_hint`, and match that style):

```rust
    fn report(leases: usize, idle: Option<u64>, grace: u64, busy: bool) -> StatusReport {
        StatusReport {
            pid: 42,
            build: "b".into(),
            leases,
            idle_secs: idle,
            grace_secs: grace,
            idle_check_secs: 60,
            work_in_flight: busy,
            code: RoleState::Running,
            docs: RoleState::NotOwned,
        }
    }

    #[test]
    fn watcher_verdicts_follow_202() {
        let log = Path::new("/p/.infigraph/daemon.log");
        let v = |r| watcher_verdict("w".into(), &r, log);
        let leased = v(report(2, None, 1800, false));
        assert!(leased.is_pass() && leased.message.contains("2 clients leasing"));
        let never = v(report(0, Some(99_999), 0, false));
        assert!(never.is_pass() && never.message.contains("idle exit disabled"));
        let deferred = v(report(0, Some(99_999), 1800, true));
        assert!(deferred.is_pass() && deferred.message.contains("deferred by in-flight work"));
        let waiting = v(report(0, Some(100), 1800, false));
        assert!(waiting.is_pass() && waiting.message.contains("exits in ~1700s"));
        let overdue = v(report(0, Some(1800 + 60 + 5), 1800, false));
        assert!(!overdue.is_pass() && overdue.message.contains("should have exited"));
    }
```

`CheckResult` (`doctor.rs:19`) has `pub status: CheckStatus` (`Pass`/`Warn`/`Fail`) and `pub message: String`. Add this helper to the test module so the assertions above read as written:

```rust
    trait IsPass {
        fn is_pass(&self) -> bool;
    }
    impl IsPass for CheckResult {
        fn is_pass(&self) -> bool {
            matches!(self.status, CheckStatus::Pass)
        }
    }
```

- [ ] **Step 2: Run it and check it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib doctor::tests::watcher_verdicts_follow_202 -- --test-threads=1`
Expected: compile error (`watcher_verdict` not found).

- [ ] **Step 3: Implement the doctor side.** Add:

```rust
/// #202: judge a live daemon by what it says about its own clients, not by
/// the MCP instance registry (which misses `path=` reads, group tools and
/// the CLI). Rows are checked top to bottom.
fn watcher_verdict(label: String, r: &StatusReport, log: &Path) -> CheckResult {
    let log = log.display();
    let done = |msg: String| CheckResult::pass(WATCHER_CATEGORY, label.clone(), format!("{msg} -- log: {log}"));
    if r.leases > 0 {
        return done(format!("daemon (PID {}) has {} clients leasing", r.pid, r.leases));
    }
    if r.grace_secs == 0 {
        return done(format!("daemon (PID {}) has no clients; idle exit disabled", r.pid));
    }
    if r.work_in_flight {
        return done(format!("daemon (PID {}) is idle, exit deferred by in-flight work", r.pid));
    }
    let idle = r.idle_secs.unwrap_or(0);
    let due = r.grace_secs + r.idle_check_secs;
    if idle < due {
        return done(format!(
            "daemon (PID {}) idle {idle}s, exits in ~{}s",
            r.pid,
            r.grace_secs.saturating_sub(idle)
        ));
    }
    CheckResult::warn(
        WATCHER_CATEGORY,
        label,
        format!(
            "daemon (PID {}) has had no clients for {idle}s and should have exited {}s ago",
            r.pid,
            idle - due
        ),
        format!("check {log} for why the idle exit did not run; `infigraph daemon stop` stops it"),
    )
}
```

In `check_one_watcher`, replace the `if !project_has_live_mcp_instance(project_path) { … }` block and the final `CheckResult::pass(…)` with:

```rust
    let log = project_path.join(".infigraph").join("daemon.log");
    match crate::daemon::control::query_status(project_path) {
        Ok(report) => watcher_verdict(label, &report, &log),
        Err(e) => CheckResult::warn(
            WATCHER_CATEGORY,
            label,
            format!("watcher (PID {}) is alive but did not answer a status query: {e}", holder.pid),
            format!(
                "`infigraph daemon stop` from {} (falls back to the stop sentinel), or \
                 `infigraph kill {}` if that doesn't clear it",
                project_path.display(),
                holder.pid
            ),
        ),
    }
```

Delete `project_has_live_mcp_instance`. If `instances` is now unused in `doctor.rs`, remove the import. First run `find_all_references` on `project_has_live_mcp_instance` to confirm it has no other callers.

- [ ] **Step 4: Run the doctor tests and check they pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib doctor -- --test-threads=1 && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test doctor -- --test-threads=1`
Expected: PASS. If an existing test asserted the registry-based "no MCP server instance" warning, it now tests removed behavior: rewrite it to assert the `query_status` error warning (a held `watch.lock` with no listener gives `Unresponsive`).

- [ ] **Step 5: `watch-status` and `get_watch_status`.** `cmd_watch_status` becomes:

```rust
pub(crate) fn cmd_watch_status(root: &Path) -> Result<()> {
    let result = infigraph_core::daemon::control::query_status(root);
    println!("{}", infigraph_core::daemon::control::describe_status(root, &result));
    Ok(())
}
```

In `tool_get_watch_status`, the whole `if let Some(path) = …` branch body after `let root = …canonicalize()…;` becomes:

```rust
        let result = infigraph_core::daemon::control::query_status(&root);
        return Ok(infigraph_core::daemon::control::describe_status(&root, &result));
```

Existing MCP tests: `…_reports_no_watcher_when_none_running` and `…_ignores_stale_payload_without_live_flock` expect `"No watcher running for"`, which `NoDaemon` gives. `…_reports_holder_identity_when_lock_held` expects `"role: test-daemon"`, which `Unresponsive` gives. Search the CLI tests for `"Watcher is running."` / `"No watcher running."` from `watch-status` and update them to the `describe_status` text.

- [ ] **Step 6: `ps`.** In `cmd_ps`, after `rows` is computed:

```rust
    // #155: ask every live daemon how it is doing, all at once.
    let daemon_rows: Vec<(usize, std::path::PathBuf)> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.alive && r.evidence.iter().any(|e| e.contains("watch.lock")))
        .filter_map(|(i, r)| r.projects.first().map(|p| (i, std::path::PathBuf::from(p))))
        .collect();
    let roots: Vec<_> = daemon_rows.iter().map(|(_, p)| p.clone()).collect();
    let statuses: std::collections::HashMap<usize, _> = daemon_rows
        .iter()
        .map(|(i, _)| *i)
        .zip(infigraph_core::daemon::control::query_status_many(&roots))
        .collect();
```

Add `LEASES IDLE CODE DOCS` to the header (`{:<7} {:<7} {:<9} {:<9}` after `ROLE`), and for each row render with a small helper:

```rust
fn ps_status_cells(
    s: Option<&Result<infigraph_core::daemon::read_protocol::StatusReport, infigraph_core::daemon::control::ControlError>>,
) -> [String; 4] {
    use infigraph_core::daemon::control::ControlError;
    match s {
        None => ["-".into(), "-".into(), "-".into(), "-".into()],
        Some(Ok(r)) => [
            r.leases.to_string(),
            r.idle_secs.map(|s| format!("{s}s")).unwrap_or_else(|| "-".into()),
            r.code.to_string(),
            r.docs.to_string(),
        ],
        Some(Err(ControlError::Incompatible)) => ["incompatible".into(), "".into(), "".into(), "".into()],
        Some(Err(_)) => ["no reply".into(), "".into(), "".into(), "".into()],
    }
}
```

Add a unit test in `info_commands.rs`'s tests for `ps_status_cells` covering `None`, `Ok`, `Incompatible` and `Unresponsive`. `RoleState::NotOwned`'s `Display` ("not owned by this daemon") is too wide for a column: in this helper map `RoleState::NotOwned` to `"-"`.

- [ ] **Step 7: Run everything touched**

Run: `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli -- --test-threads=1 && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-mcp --test watcher_daemon_mode -- --test-threads=1`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add crates/infigraph-core/src/doctor.rs crates/infigraph-cli/src/info_commands.rs crates/infigraph-mcp/src/tools/watch.rs crates/infigraph-mcp/tests/watcher_daemon_mode.rs crates/infigraph-core/tests/doctor.rs
git commit --no-verify -m "feat: watch-status, get_watch_status, doctor and ps read the daemon's StatusReport (#155, #202)"
```

---

### Task 9: Delete the file-drop control path

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs` (the `WatchControl` variant at L135-137, its `serve_one_request` arm ~L1177, and the tests `watch_control_request_round_trips_through_json` / `watch_control_covers_all_role_action_combinations_without_panicking_on_serialize` L703-728)
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (the file-drop arm; `reply_to_watch_control`; `route_or_serve_request` loses its `code_watch`, `docs_control` and `shutdown_requested` parameters if nothing else uses them)
- Modify: `crates/infigraph-core/src/lib.rs` (delete `submit_watch_control_and_await`, L850-870)
- Modify tests that drop `WatchControl` files:
  - `crates/infigraph-core/tests/watch_daemon.rs`: L371, L463, and the four `watch_control_*` tests at L1238-1640, which are superseded by `tests/daemon_control.rs`
  - `crates/infigraph-core/tests/watch_control.rs`
  - `crates/infigraph-core/tests/watch_control_helper.rs`
  - `crates/infigraph-core/tests/daemon_kuzu_e2e.rs`: L200, L1381, L1454

**Interfaces:**
- Consumes: `control::send_control`
- Produces: none new

- [ ] **Step 1: Write the failing legacy-client test** in `tests/daemon_control.rs`:

```rust
#[test]
fn a_legacy_watch_control_request_file_gets_a_prompt_error() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    let requests = dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&requests).unwrap();
    // Exactly what a pre-#155 client wrote.
    infigraph_core::daemon_protocol::write_atomic(
        &requests.join("legacy.request"),
        r#"{"WatchControl":{"role":"Daemon","action":"Stop"}}"#,
    )
    .unwrap();
    let result = requests.join("legacy.result");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !result.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let reply = std::fs::read_to_string(&result).expect("a prompt reply, not a 30s client timeout");
    assert!(reply.contains("Err"), "{reply}");
    assert!(!d.handle.is_finished(), "a legacy stop must not be honoured");
    stop(d);
}
```

Check the exact JSON shape of the old request first. Run `watch_control_request_round_trips_through_json` (before deleting it) with `--nocapture`, printing `serde_json::to_string(&req)`, and paste that shape into the test.

- [ ] **Step 2: Run it and check it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_control a_legacy -- --test-threads=1`
Expected: FAIL on the last assertion. The file-drop arm still honours the stop, so the loop finishes.

- [ ] **Step 3: Delete the path.**
  - Remove `WriteRequest::WatchControl`, its `serve_one_request` arm and its two JSON tests.
  - Remove the `WriteRequest::WatchControl { .. }` arm in `route_or_serve_request` and the `reply_to_watch_control` function.
  - A legacy request now fails `serde_json::from_str::<WriteRequest>`, which hands it to `serve_request_locked`, which replies `WriteResult::Err` for corrupt JSON. That is the prompt error Step 1 pins.
  - Drop `route_or_serve_request` parameters the compiler reports as unused, and update its one caller.
  - Remove `Infigraph::submit_watch_control_and_await`.

- [ ] **Step 4: Migrate the tests.**
  - The helpers in `daemon_kuzu_e2e.rs` (L200 `stop_daemon`, L1381, L1454) and `watch_daemon.rs` (L371, L463) that drop a `WatchControl { Daemon, Stop }` file: replace the `write_atomic(... WatchControl ...)` block with `let _ = infigraph_core::daemon::control::send_control(&root, WatchRole::Daemon, WatchAction::Stop);`, keeping each helper's existing wait-for-exit and kill fallback.
  - Delete `watch_daemon.rs`'s four `watch_control_*` tests (L1238-1640). `tests/daemon_control.rs` (Task 5) covers the same assertions over the socket: Stop ends the loop and cancels the token; Start is refused without stopping; docs without a handle is refused. Move the docs-dispatch test there, using a recording `DocsHandle` (the struct written in Task 5 Step 4), and assert the recorded actions equal `[WatchAction::Start]` after `send_control(root, WatchRole::Docs, WatchAction::Start)`.
  - `watch_control.rs` (`daemon_survives_watch_control_stop_and_keeps_serving_writes`): replace its two file drops (L145, L188) with `send_control(root, WatchRole::Code, WatchAction::Stop)` / `(…, Start)` and assert `Ok(())`. Keep the write-serving assertions.
  - `watch_control_helper.rs`: delete the file. The helper it tested is gone, and `tests/daemon_control_client.rs` covers the replacement.

- [ ] **Step 5: Confirm nothing references the old path**

Run `mcp__infigraph__search` with `regex=true` for `WriteRequest::WatchControl|submit_watch_control_and_await|reply_to_watch_control|DocsControl`.
Expected: matches only in `docs/` (plans and specs), none in `crates/`.

- [ ] **Step 6: Run the affected suites**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_control --test watch_daemon --test watch_control --test daemon_kuzu_e2e --test read_service -- --test-threads=1 && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib -- --test-threads=1`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add -u crates/
git add crates/infigraph-core/tests/daemon_control.rs
git status --short   # confirm watch_control_helper.rs shows as deleted and nothing is left unstaged
git commit --no-verify -m "refactor(core): delete the file-drop WatchControl path (#155)"
```

---

### Task 10: Whole-workspace gate and docs

**Files:**
- Modify: `CLAUDE.md` (the "Cross-cutting invariants" bullet on daemon leases: add one sentence)
- Modify: `docs/superpowers/plans/2026-08-21-daemon-watch-command-split.md` (Task 19: mark Gap B done by #155)

- [ ] **Step 1: Document the invariant.** Append to CLAUDE.md's "A daemon lives as long as someone leases it" bullet:

```
Control and status travel on the same socket (#155): `daemon::control::{send_control, query_status}` are the only clients, neither leases, and every op declares `DaemonOp::KEEPS_ALIVE` -- a new op that should not extend a daemon's life says `false`, and `ClientFrame::keeps_alive`'s exhaustive match will not compile until it answers. `watch.stop` stays as the out-of-band stop for a daemon that cannot answer.
```

- [ ] **Step 2: Mark Task 19 Gap B done** in the 08-21 plan: tick Step 3 and Step 4, each followed by `(done by #155: StatusReport.code/docs, RoleState)`.

- [ ] **Step 3: Format, lint and test the workspace**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo build -p infigraph-cli
env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core -- --test-threads=1
env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli -- --test-threads=1
env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-mcp -- --test-threads=1
env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs -- --test-threads=1
```

This is per crate, not one `--all`, because of the disk-constrained machine. Expected: clean. A failure under load: rerun that one test with `--test-threads=1` before treating it as real.

- [ ] **Step 4: Commit through the real hook** (no `--no-verify`: this runs fmt, clippy and the four perf gates once on the finished branch)

```bash
git add CLAUDE.md docs/superpowers/plans/2026-08-21-daemon-watch-command-split.md
git commit -m "docs: control and status share the read socket; Task 19 Gap B done (#155)"
```

- [ ] **Step 5: Manual end-to-end check** (install the build first; see the codesign memory: `codesign --force --sign -` after copying into `~/.local/bin`):

```bash
infigraph daemon-restart
infigraph watch-status        # shows PID, build, leases, idle, code/doc state
infigraph watch stop && infigraph watch-status   # code watching: stopped
infigraph watch start
infigraph ps                  # LEASES IDLE CODE DOCS columns filled for this project
infigraph doctor              # watcher line reads "N clients leasing" / "idle Xs, exits in ~Ys"
```
