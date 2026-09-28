# Daemon Write Requests over the Read Socket — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every daemon data write (`Index`, `FullReindex`, `ScipImport`, …) travels over the read socket as a typed request with a typed reply, and the `.infigraph/requests/` file-drop protocol is deleted.

**Architecture:** #155's `ControlPort` becomes `CoordinatorPort`, whose one channel carries `PortMsg::{Control, Write}`. Each write connection gets its own thread (like control). That thread acks admission, waits on a `WriteReply` channel, and marks the reply *gone* if its client disconnects. On the coordinator, every file reply path becomes a consuming `WriteReply`, and one `deferred` queue replaces every "retry the file next tick". The client (`daemon/writes.rs`) reuses #155's `exchange` with an abort check. The daemon's own writes (compaction, auto-recovery, SCIP import) go in-process.

**Tech Stack:** Rust, `std::sync::mpsc`, `interprocess` 2.4 local sockets, `libc::poll` (unix), `tokio_util::sync::CancellationToken`, serde_json framing from `daemon/read_protocol.rs`.

**Spec:** `docs/superpowers/specs/2026-09-27-daemon-socket-writes-design.md`. Read it before any task. It carries the decisions (D1–D8) and the reasons this plan does not repeat.

## Global Constraints

- Port channel bound: `PORT_QUEUE = 64`. Control admission stays `try_send` fail-fast (`BUSY`); write admission waits while its client is connected.
- Disconnect poll: the daemon's write thread checks `peer_closed` every **250ms** (`GONE_POLL`).
- Client wait poll: `exchange` runs its stop check every **50ms** (`EXCHANGE_POLL`).
- Legacy refusal sweep: every **2s** (`LEGACY_SWEEP_INTERVAL`). Message: `this daemon (build <hash>) no longer accepts file-drop requests; restart the client (MCP: /mcp reconnect)`.
- Sidecars: `.infigraph/write-tmp/<pid>-<nanos>-<n>.<ext>`, swept at daemon start when older than **6h**.
- Two reply frames for a write: admission `OpReply<()>`, then `OpReply<WriteResult>`. EOF before the first is `Incompatible`; EOF between them is `Lost`.
- `DaemonOp` gains a required `const ACKED: bool` (no default, like `KEEPS_ALIVE`).
- `WriteFrame::KEEPS_ALIVE = true`.
- Existing per-call write timeouts are unchanged (30s / 60s / 120s / 600s as today).
- Work that has started is never cancelled (D3).
- Tests that open the graph pin `INFIGRAPH_BACKEND=kuzu`, with `INFIGRAPH_WATCH_DAEMON` unset: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test …`.
- This machine is disk-constrained: run per crate, never one `cargo test --all` until Task 10.
- Intermediate task commits use `git commit --no-verify`. The hook's perf gates don't touch this path, and #155's plan did the same. Task 10 runs the full hook.
- Every commit message ends with:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV
  ```

## Review Focus

1. **A client killed while its write waits for admission** (the port is full). The write thread must notice the closed peer while it retries `try_send`, and drop the message instead of spinning. Pinned in Task 6: `a_write_waiting_for_a_slot_gives_up_when_its_client_leaves`.
2. **A drain task that panics** while waiters are folded into it. Every client must still get an `Err` rather than hanging to its timeout. `Drop for WriteReply` does this. Pinned in Task 4: `a_panicking_drain_still_answers_every_waiter`.
3. **Compaction asking for a rebuild on every tick** while one is deferred behind a drain must not queue dozens of rebuilds. Pinned in Task 5: `internal_rebuilds_collapse_while_one_is_waiting_or_running`.
4. **A daemon that crashes mid-write** must read as `Lost`, not as a version mismatch. Pinned in Task 7: `eof_after_admission_is_lost_not_incompatible`.
5. **The SCIP callback cancelled during shutdown** must not leave its import to run later. Its reply is marked gone and dropped at pickup. Pinned in Task 5: `a_cancelled_in_process_write_is_marked_gone`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/infigraph-core/src/daemon/read_protocol.rs` (modify) | `WriteFrame`, `DaemonOp::ACKED`, `ClientFrame::Write` |
| `crates/infigraph-core/src/daemon/coordinator_port.rs` (rename from `control_port.rs`) | `CoordinatorPort`, `PortMsg`, `WriteReply`, `WriteSubmitter`, `DaemonState` |
| `crates/infigraph-core/src/daemon_protocol.rs` (modify) | `WriteRequest`/`WriteResult` types, `serve_write`, sidecar codecs. File-drop client and server functions deleted (Task 9). |
| `crates/infigraph-core/src/daemon/mod.rs` (modify) | Coordinator: `route_write`, the `deferred` queue, reply plumbing, in-process submitters, legacy sweep |
| `crates/infigraph-core/src/daemon/drain.rs` (modify) | `execute_drain` answers through `WriteReply` |
| `crates/infigraph-core/src/daemon/queue.rs` (modify) | `Waiter.reply: WriteReply` |
| `crates/infigraph-core/src/daemon/read_service.rs` (modify) | `spawn_write`, shared `spawn_on_port` |
| `crates/infigraph-core/src/daemon/read_endpoint.rs` (modify) | `ReadStream::peer_closed` |
| `crates/infigraph-core/src/daemon/control.rs` (modify) | Generic `exchange`, `ControlError::Lost` |
| `crates/infigraph-core/src/daemon/writes.rs` (create) | The write client: `submit`, `WriteOpts`, `sidecar_path` |
| `crates/infigraph-core/src/scratch.rs` (create) | `sweep_older_than`, shared with the CLI's SCIP sweep |
| `crates/infigraph-core/src/recovery.rs` (modify) | `drain_recovery_sentinel -> Result<bool>` |
| `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs` (modify) | 18 write methods through two helpers |
| `crates/infigraph-core/src/lib.rs` (modify) | `index_via_daemon`, `pub mod scratch` |
| `crates/infigraph-cli/src/index.rs` (modify) | `cmd_index` FullReindex; the SCIP sweep delegates to `scratch` |
| `crates/infigraph-cli/src/info_commands.rs` (modify) | The SCIP callback uses `WriteSubmitter` |

---

### Task 1: Wire protocol: `WriteFrame` and `ACKED`

**Files:**
- Modify: `crates/infigraph-core/src/daemon/read_protocol.rs` (`DaemonOp` L170-190, `ClientFrame` L200-218, tests module)
- Modify: `crates/infigraph-core/src/daemon/read_service.rs` (`serve_one`, the `match frame` block ~L273-297)

**Interfaces:**
- Consumes: `crate::daemon_protocol::{WriteRequest, WriteResult}` (unchanged).
- Produces:
  - `pub struct WriteFrame { pub write: WriteRequest }`
  - `impl DaemonOp for WriteFrame { type Reply = WriteResult; const KEEPS_ALIVE: bool = true; const ACKED: bool = true; }`
  - `DaemonOp::ACKED: bool` (required, no default)
  - `ClientFrame::Write(WriteFrame)`
  - `pub const WRITES_NOT_SERVED: &str` in `read_service.rs` (removed in Task 6)

- [ ] **Step 1: Write the failing tests** (append to `read_protocol.rs`'s `mod tests`)

```rust
    #[test]
    fn a_write_frame_round_trips_and_is_no_other_frame() {
        let frame = WriteFrame {
            write: crate::daemon_protocol::WriteRequest::FullReindex,
        };
        let bytes = serde_json::to_vec(&frame).unwrap();
        assert_eq!(bytes, br#"{"write":"FullReindex"}"#);
        match parse(&bytes) {
            ClientFrame::Write(w) => {
                assert_eq!(w.write, crate::daemon_protocol::WriteRequest::FullReindex)
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn only_writes_are_acked_and_writes_keep_the_daemon_alive() {
        assert!(WriteFrame::ACKED);
        assert!(WriteFrame::KEEPS_ALIVE);
        assert!(!ReadRequest::ACKED);
        assert!(!StatusFrame::ACKED);
        assert!(!ControlFrame::ACKED);
        let w = ClientFrame::Write(WriteFrame {
            write: crate::daemon_protocol::WriteRequest::FullReindex,
        });
        assert_eq!(w.keeps_alive(), Some(true));
    }
```

`parse` is the existing test helper at L383. `todays_read_and_attach_bytes_still_parse_as_before` already pins that older frames still parse. Leave it unchanged.

- [ ] **Step 2: Run to see it fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib daemon::read_protocol`
Expected: FAIL to compile (`WriteFrame` and `ACKED` not found).

- [ ] **Step 3: Implement.** In `read_protocol.rs`:

```rust
/// A data write (#204). Answered with two frames: an admission
/// `OpReply<()>` once the coordinator has the request, then the
/// `OpReply<WriteResult>` outcome -- so a client can tell a daemon that could
/// not parse the frame (EOF before admission) from one that died serving it
/// (EOF after).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFrame {
    pub write: crate::daemon_protocol::WriteRequest,
}
```

In `trait DaemonOp`, after `KEEPS_ALIVE`:

```rust
    /// Whether the daemon sends an admission frame before the reply. No
    /// default, for the same reason as `KEEPS_ALIVE`.
    const ACKED: bool;
```

Add `const ACKED: bool = false;` to the `ReadRequest`, `StatusFrame` and `ControlFrame` impls. Then add:

```rust
impl DaemonOp for WriteFrame {
    type Reply = crate::daemon_protocol::WriteResult;
    const KEEPS_ALIVE: bool = true;
    const ACKED: bool = true;
}
```

Add `Write(WriteFrame),` as the last `ClientFrame` variant. Add `ClientFrame::Write(_) => Some(WriteFrame::KEEPS_ALIVE),` to `keeps_alive`. Extend the enum's doc comment: "`Write` (#204) is the same single-key shape."

In `read_service.rs`, add next to the other consts:

```rust
/// Until the coordinator serves socket writes (#204, Task 6).
pub const WRITES_NOT_SERVED: &str = "this daemon does not serve writes over the socket yet";
```

In `serve_one`'s match, add before the `};` that closes it:

```rust
        ClientFrame::Write(_) => {
            write_reply::<_, ()>(&mut stream, &OpReply::Err(WRITES_NOT_SERVED.to_string()))?;
            return Ok(());
        }
```

- [ ] **Step 4: Run to see it pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib daemon::read_protocol`
Expected: PASS, including the existing frame tests.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon/read_protocol.rs crates/infigraph-core/src/daemon/read_service.rs
git commit --no-verify -m "feat(protocol): WriteFrame and the ACKED op constant (#204)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```

---

### Task 2: `serve_write`, a pure request-to-result function

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs` (`IngestSource` L133-145, `serve_one_request` L836-1145, `handle_ingest_structured` L1147-1180, `write_ingest_inline_sibling` L1182-1202)
- Modify: `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs` (`ingest_structured_data` L469-508)
- Test: `crates/infigraph-core/tests/daemon_protocol_serve.rs` (all 15 tests)

**Interfaces:**
- Produces:
  - `pub fn serve_write(infigraph: &Infigraph, request: &WriteRequest) -> WriteResult`
  - `IngestSource::Inline(PathBuf)` (the data sidecar's path)
  - `pub fn write_ingest_data(path: &Path, data: &[serde_json::Value]) -> anyhow::Result<()>` (replaces `write_ingest_inline_sibling`)
  - `serve_one_request(&Infigraph, &Path) -> anyhow::Result<()>` stays, as a thin file wrapper, until Task 9.

- [ ] **Step 1: Rewrite the serve tests against `serve_write`.** Each of the 15 tests in `tests/daemon_protocol_serve.rs` follows one shape: write a `.request` file, call `serve_one_request`, read `.result`. Change each as the first test shows below, keeping its setup and assertions. Delete `serve_one_request_writes_err_result_on_corrupt_request_json`: corrupt JSON can no longer reach `serve_write`, and the socket equivalent is tested in Task 6. Before:

```rust
    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let request_path = staging_dir.join("test-1.request");
    write_atomic(&request_path, &serde_json::to_string(&request).unwrap()).unwrap();
    serve_one_request(&infigraph, &request_path).unwrap();
    let result: WriteResult =
        serde_json::from_str(&std::fs::read_to_string(request_path.with_extension("result")).unwrap()).unwrap();
    assert!(!request_path.exists());
```

After:

```rust
    let result = serve_write(&infigraph, &request);
```

Drop any assertion about the request or result *file* (`!request_path.exists()`). Keep every assertion about `result` and the graph. For the two ingest tests:
- `serve_one_request_handles_ingest_structured_inline`: write the data with `write_ingest_data(&data_path, &data)` to `project_dir.path().join("data.json")`, and send `IngestSource::Inline(data_path.clone())`. Also assert `!data_path.exists()` afterwards: the daemon consumes the sidecar.
- `serve_one_request_handles_ingest_structured_file`: unchanged apart from the call.

Rename each test from `serve_one_request_handles_*` to `serve_write_handles_*`. Update the `use` line to import `serve_write, write_ingest_data` in place of `serve_one_request, write_atomic` (keep `write_atomic` if another test still uses it).

- [ ] **Step 2: Run to see it fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_protocol_serve`
Expected: FAIL to compile (`serve_write`, `write_ingest_data` and `Inline(..)` not found).

- [ ] **Step 3: Implement.**

`IngestSource`:

```rust
/// Where IngestStructured's data comes from. `Inline`'s array rides in a
/// sidecar JSON file (`writes::sidecar_path`) rather than in the frame,
/// following the reference-not-payload convention paths already use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IngestSource {
    File(PathBuf),
    Directory(PathBuf),
    Inline(PathBuf),
}
```

Split `serve_one_request`. Move the whole `match &request { … }` body (the arm list from `WriteRequest::Index { paths: None }` through `WriteRequest::FullReindex`) into:

```rust
/// Runs one write against `infigraph` and says what happened. Never panics
/// on a failed operation: every failure is a `WriteResult::Err`, so the
/// client always gets an answer.
pub fn serve_write(infigraph: &Infigraph, request: &WriteRequest) -> WriteResult {
    match request {
        // ... the existing arms, unchanged except the IngestStructured one:
        WriteRequest::IngestStructured { schema_id, source } => {
            match handle_ingest_structured(infigraph, schema_id, source) {
                Ok(r) => WriteResult::Ok {
                    total_files: r.nodes_created + r.edges_created,
                    indexed_files: r.nodes_created,
                },
                Err(e) => WriteResult::Err {
                    message: e.to_string(),
                },
            }
        }
        // ...
    }
}
```

`serve_one_request` becomes (kept until Task 9, which deletes it):

```rust
pub fn serve_one_request(infigraph: &Infigraph, request_path: &Path) -> anyhow::Result<()> {
    let result_path = request_path.with_extension("result");
    let result = match std::fs::read_to_string(request_path)
        .map_err(anyhow::Error::from)
        .and_then(|contents| Ok(serde_json::from_str::<WriteRequest>(&contents)?))
    {
        Ok(request) => serve_write(infigraph, &request),
        Err(e) => WriteResult::Err {
            message: format!("failed to read/parse request: {e}"),
        },
    };
    write_atomic(&result_path, &serde_json::to_string(&result)?)?;
    // (keep the existing tolerant request-file removal and its comment)
    Ok(())
}
```

`handle_ingest_structured` loses its `request_path` parameter. Its `Inline` arm becomes:

```rust
        IngestSource::Inline(data_path) => {
            let data: Vec<serde_json::Value> =
                serde_json::from_str(&std::fs::read_to_string(data_path)?)?;
            let result = backend.ingest_structured_data(&schema.schema, &data);
            std::fs::remove_file(data_path).ok();
            result
        }
```

Keep the other two arms. Replace `write_ingest_inline_sibling` with:

```rust
/// Writes an `IngestStructured::Inline` payload to its sidecar `path`.
pub fn write_ingest_data(path: &Path, data: &[serde_json::Value]) -> anyhow::Result<()> {
    write_atomic(path, &serde_json::to_string(data)?)
}
```

In `DaemonKuzuBackend::ingest_structured_data`, replace the `request_path` and `write_ingest_inline_sibling` lines with:

```rust
        let data_path = staging_dir.join(format!("{name}.data.json"));
        crate::daemon_protocol::write_ingest_data(&data_path, data)?;
```

and send `IngestSource::Inline(data_path.clone())`. Task 8 moves this to `sidecar_path`.

- [ ] **Step 4: Run to see it pass**

Run:
- `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_protocol_serve`
- `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_kuzu_backend`

Expected: PASS. The backend's inline-ingest test still passes, because the file-drop wrapper forwards to `serve_write`.

- [ ] **Step 5: Commit**

```bash
git add -A crates/infigraph-core
git commit --no-verify -m "refactor(daemon): serve_write, a pure request-to-result function (#204)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```

---

### Task 3: `CoordinatorPort`, `PortMsg` and `WriteReply`

**Files:**
- Rename: `crates/infigraph-core/src/daemon/control_port.rs` → `crates/infigraph-core/src/daemon/coordinator_port.rs` (`git mv`)
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (`pub mod control_port;` L3, the port creation ~L670, the `recv_timeout` loop ~L1765-1797, `publish_roles` ~L3548-3560)
- Modify: `crates/infigraph-core/src/daemon/read_service.rs` (import L22, `start_serving`, `serve_one`, `spawn_control`)
- Modify imports and uses in:
  - `crates/infigraph-cli/src/info_commands.rs:905`
  - `crates/infigraph-cli/tests/daemon_control_cli.rs:73`
  - `crates/infigraph-core/tests/{doctor.rs:1176,1828, daemon_control_client.rs:12,88,149, read_service.rs:730-746, daemon_control.rs:167}`

**Interfaces:**
- Consumes: `WriteRequest`, `WriteResult`.
- Produces (all in `daemon::coordinator_port`):
  - `pub const PORT_QUEUE: usize = 64;` (replaces `CONTROL_QUEUE`)
  - `pub enum PortMsg { Control(ControlMsg), Write { request: WriteRequest, reply: WriteReply } }`
  - `pub struct CoordinatorPort` (was `ControlPort`) with:
    - `new(grace_secs, idle_check_secs) -> (Arc<Self>, mpsc::Receiver<PortMsg>)`
    - `submit_control(&self, ControlRequest) -> Result<Receiver<ControlReply>, String>` (was `submit`)
    - `admit_write(&self, WriteRequest, WriteReply, give_up: impl FnMut() -> bool) -> Result<(), String>`
    - `enter`, `in_flight`, `wait_idle` and `pub state` unchanged
  - `pub struct WriteReply` with `channel() -> (Self, Receiver<WriteResult>)`, `internal() -> Self`, `send(self, WriteResult)`, `is_gone(&self) -> bool`, `gone_flag(&self) -> Arc<AtomicBool>`, and `impl Drop`
  - `pub const DROPPED: &str`
  - `#[derive(Clone)] pub struct WriteSubmitter(Arc<CoordinatorPort>)` with `new(Arc<CoordinatorPort>)` and `submit(&self, WriteRequest, &CancellationToken) -> anyhow::Result<WriteResult>`

- [ ] **Step 1: Rename the module and type.** Run `git mv crates/infigraph-core/src/daemon/control_port.rs crates/infigraph-core/src/daemon/coordinator_port.rs`. Change `pub mod control_port;` to `pub mod coordinator_port;` (keep the `pub mod` block alphabetical). Then, across the files listed above, replace:
  - `control_port::` → `coordinator_port::`
  - `ControlPort` → `CoordinatorPort`
  - `CONTROL_QUEUE` → `PORT_QUEUE`
  - `port.submit(` → `port.submit_control(` (in `read_service.rs::spawn_control` and the module's own tests)

  In `mod.rs`, keep the local variable name `control_port` for now; Task 4 renames it to `port`.

- [ ] **Step 2: Write the failing tests** (append to `coordinator_port.rs`'s `mod tests`)

```rust
    use crate::daemon_protocol::{WriteRequest, WriteResult};

    #[test]
    fn a_reply_is_sent_once_and_an_unsent_one_answers_dropped() {
        let (reply, rx) = WriteReply::channel();
        reply.send(WriteResult::Ok { total_files: 1, indexed_files: 1 });
        assert!(matches!(rx.recv().unwrap(), WriteResult::Ok { .. }));
        assert!(rx.recv().is_err(), "exactly one answer");

        let (reply, rx) = WriteReply::channel();
        drop(reply);
        assert!(matches!(rx.recv().unwrap(), WriteResult::Err { message } if message == DROPPED));
    }

    #[test]
    fn gone_is_shared_with_the_flag_and_internal_is_never_gone() {
        let (reply, _rx) = WriteReply::channel();
        reply.gone_flag().store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(reply.is_gone());
        assert!(!WriteReply::internal().is_gone());
        WriteReply::internal().send(WriteResult::Err { message: "x".into() }); // no receiver: no panic
    }

    #[test]
    fn control_and_writes_share_one_channel_in_arrival_order() {
        let (port, rx) = CoordinatorPort::new(0, 1);
        let _c = port.submit_control(req()).unwrap();
        let (reply, _r) = WriteReply::channel();
        port.admit_write(WriteRequest::FullReindex, reply, || false).unwrap();
        assert!(matches!(rx.recv().unwrap(), PortMsg::Control(_)));
        assert!(matches!(rx.recv().unwrap(), PortMsg::Write { request: WriteRequest::FullReindex, .. }));
    }

    #[test]
    fn a_write_waits_for_a_slot_and_can_give_up() {
        let (port, rx) = CoordinatorPort::new(0, 1);
        let held: Vec<_> = (0..PORT_QUEUE).map(|_| port.submit_control(req()).unwrap()).collect();
        let (reply, _r) = WriteReply::channel();
        let mut tries = 0;
        assert_eq!(
            port.admit_write(WriteRequest::FullReindex, reply, || { tries += 1; tries > 3 }),
            Err(GAVE_UP.to_string())
        );
        drop(held);
        drop(rx);
        let (reply, _r) = WriteReply::channel();
        assert_eq!(
            port.admit_write(WriteRequest::FullReindex, reply, || false),
            Err(SHUTTING_DOWN.to_string())
        );
    }

    #[test]
    fn a_cancelled_in_process_write_is_marked_gone() {
        let (port, rx) = CoordinatorPort::new(0, 1);
        let token = tokio_util::sync::CancellationToken::new();
        let submitter = WriteSubmitter::new(port);
        let t = std::thread::spawn({
            let token = token.clone();
            move || submitter.submit(WriteRequest::FullReindex, &token)
        });
        let PortMsg::Write { reply, .. } = rx.recv().unwrap() else { panic!() };
        token.cancel();
        let err = t.join().unwrap().unwrap_err();
        assert!(err.downcast_ref::<crate::daemon_protocol::WriteRequestCancelled>().is_some());
        assert!(reply.is_gone(), "the coordinator must drop it at pickup");
    }
```

Update `a_full_queue_refuses_at_once_and_a_dropped_receiver_reads_as_shutting_down` to use `PORT_QUEUE` and `submit_control`.

- [ ] **Step 3: Run to see it fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib daemon::coordinator_port`
Expected: FAIL to compile (`WriteReply`, `PortMsg`, `admit_write` and `WriteSubmitter` missing).

- [ ] **Step 4: Implement** in `coordinator_port.rs`. Update the module doc's first line to: "The daemon's side of socket control and writes (#155, #204): what `Status` reads without the coordinator, and the one bounded channel control and writes reach it through." Then:

```rust
use crate::daemon_protocol::{WriteRequest, WriteResult};
use std::sync::atomic::AtomicBool;

/// Requests the coordinator may have queued. Control fails fast when it is
/// full; a write waits for a slot while its client is still connected (D1).
pub const PORT_QUEUE: usize = 64;

pub const DROPPED: &str = "the daemon dropped this write without answering it";
pub const GAVE_UP: &str = "the client left before the coordinator had room for its write";

pub enum PortMsg {
    Control(ControlMsg),
    Write { request: WriteRequest, reply: WriteReply },
}

/// Where one write's answer goes. Consumed by `send`, so a waiter is answered
/// at most once by type; answered `DROPPED` by `Drop` if it never was, so a
/// panic, an early return or a teardown cannot strand a client (D8).
pub struct WriteReply {
    tx: Option<mpsc::Sender<WriteResult>>,
    gone: Arc<AtomicBool>,
}

impl WriteReply {
    pub fn channel() -> (Self, mpsc::Receiver<WriteResult>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx: Some(tx), gone: Arc::default() }, rx)
    }

    /// For the daemon's own writes (compaction, auto-recovery): nobody waits.
    pub fn internal() -> Self {
        Self { tx: None, gone: Arc::default() }
    }

    pub fn send(mut self, result: WriteResult) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(result);
        }
    }

    /// Set by the connection thread when its client disconnects (D2).
    pub fn gone_flag(&self) -> Arc<AtomicBool> {
        self.gone.clone()
    }

    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::SeqCst)
    }
}

impl Drop for WriteReply {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(WriteResult::Err { message: DROPPED.to_string() });
        }
    }
}

impl std::fmt::Debug for WriteReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteReply")
            .field("waits", &self.tx.is_some())
            .field("gone", &self.is_gone())
            .finish()
    }
}
```

In `CoordinatorPort`, the field becomes `tx: mpsc::SyncSender<PortMsg>`, `new` uses `mpsc::sync_channel(PORT_QUEUE)`, and `new`'s return type is `(Arc<Self>, mpsc::Receiver<PortMsg>)`. `submit_control` is the old `submit`, sending `PortMsg::Control(ControlMsg { request, reply })`. Add:

```rust
    /// Queue a write, waiting for a slot rather than refusing (D1). Stops
    /// waiting when `give_up` says so -- its client left, or cancelled.
    pub fn admit_write(
        &self,
        request: WriteRequest,
        reply: WriteReply,
        mut give_up: impl FnMut() -> bool,
    ) -> Result<(), String> {
        let mut msg = PortMsg::Write { request, reply };
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Disconnected(_)) => return Err(SHUTTING_DOWN.to_string()),
                Err(mpsc::TrySendError::Full(back)) => {
                    if give_up() {
                        return Err(GAVE_UP.to_string());
                    }
                    msg = back;
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
```

Then `WriteSubmitter`:

```rust
/// The daemon's own writes, from inside its process (D6): the SCIP
/// enrichment callback's import. No socket, no files.
#[derive(Clone)]
pub struct WriteSubmitter(Arc<CoordinatorPort>);

impl WriteSubmitter {
    pub fn new(port: Arc<CoordinatorPort>) -> Self {
        Self(port)
    }

    /// Queue `request` and wait for its answer. `cancel` ends the wait
    /// promptly (#138) and marks the write gone, so the coordinator drops it
    /// at pickup if it has not started.
    pub fn submit(
        &self,
        request: WriteRequest,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<WriteResult> {
        let (reply, rx) = WriteReply::channel();
        let gone = reply.gone_flag();
        let cancelled = || {
            gone.store(true, Ordering::SeqCst);
            anyhow::Error::new(crate::daemon_protocol::WriteRequestCancelled)
        };
        self.0
            .admit_write(request, reply, || cancel.is_cancelled())
            .map_err(|msg| if cancel.is_cancelled() { cancelled() } else { anyhow::anyhow!(msg) })?;
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => return Ok(result),
                Err(mpsc::RecvTimeoutError::Timeout) if cancel.is_cancelled() => return Err(cancelled()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!(SHUTTING_DOWN),
            }
        }
    }
}
```

`WriteRequestCancelled` becomes a unit struct in `daemon_protocol.rs`. The existing file-drop code constructs it with `request_path`, so change those two sites to `WriteRequestCancelled`:

```rust
#[derive(Debug)]
pub struct WriteRequestCancelled;

impl std::fmt::Display for WriteRequestCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("write request cancelled before the daemon answered")
    }
}
```

Every `PortMsg` consumer needs updating:
- **Coordinator (`mod.rs`, the `recv_timeout` loop).** Destructure `PortMsg::Control(control_port::ControlMsg { request, reply })` in place of `ControlMsg { request, reply }`. Add a `PortMsg::Write { request, reply } =>` arm that pushes onto a new `let mut deferred: std::collections::VecDeque<(WriteRequest, WriteReply)> = VecDeque::new();`, declared before the loop. Task 4 processes it. The loop's shape:

  ```rust
          while let Some(msg) = next.take() {
              match msg {
                  coordinator_port::PortMsg::Write { request, reply } => {
                      deferred.push_back((request, reply));
                  }
                  coordinator_port::PortMsg::Control(coordinator_port::ControlMsg { request, reply }) => {
                      // ... the existing control body, unchanged, including its `break` on daemon stop
                  }
              }
              next = control_rx.try_recv().ok();
          }
  ```

- **Tests reading the receiver** (`tests/read_service.rs`'s answer threads, `tests/daemon_control_client.rs:88-100`). Change `rx.recv().unwrap().reply` to:

  ```rust
          let coordinator_port::PortMsg::Control(msg) = rx.recv().unwrap() else { panic!("a control message") };
          msg.reply.send(Ok(())).unwrap();
  ```

  Change the `control_service` helper's return type to `Receiver<PortMsg>`.

- [ ] **Step 5: Run to see it pass**

Run:
- `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib daemon::coordinator_port`
- `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test read_service --test daemon_control_client --test daemon_control --test doctor`
- `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test daemon_control_cli`

Expected: all PASS. #155's control behavior is unchanged.

- [ ] **Step 6: Commit**

```bash
git add -A crates/
git commit --no-verify -m "refactor(daemon): CoordinatorPort carries control and writes; WriteReply (#204)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```

---

### Task 4: The coordinator answers through `WriteReply`, with one `deferred` queue

This is the core daemon change. File-drop keeps working during it: a small transitional *file bridge* turns each `.request` into a deferred write and writes its answer back as the `.result`. Task 9 deletes the bridge.

**Files:**
- Modify: `crates/infigraph-core/src/daemon/queue.rs` (`Waiter` L33-50: `reply_path` → `reply`, drop the `Clone` derive; tests ~L310-325)
- Modify: `crates/infigraph-core/src/daemon/drain.rs` (`execute_drain` L26-161, test helper `drive_full_reindex_sync` L223-257, tests using `reply_path`)
- Modify: `crates/infigraph-core/src/daemon/mod.rs`:
  - `InFlightDrain` ~L1879
  - `finish_drain` ~L1928
  - delete `reply_err_to_waiters` ~L1966-1994
  - `PendingFullReindex` + `join` ~L2299-2321
  - `PendingScipImport` ~L2338
  - `try_start_full_reindex` ~L2453
  - `try_start_scip_import` ~L2627
  - `finish_scip_import` ~L2726
  - delete `reply_to_all` ~L2799
  - `finish_full_reindex` ~L2823
  - `serve_request_locked` ~L2226
  - `route_or_serve_request` → `route_write` ~L3267
  - the loop's pickup block ~L1572-1598, drain scheduling ~L1630-1670, reap blocks ~L1109-1216 and ~L1386-1404
  - `work_in_flight!` ~L924
  - post-loop teardown ~L1823-1870
- Test: `mod.rs` tests ~L3990-4420, `drain.rs` tests, `crates/infigraph-core/tests/watch_daemon.rs` (L814-923, L1004-1070, L1148-1214)

**Interfaces:**
- Consumes: Task 3's `WriteReply`, `PortMsg`, `deferred`. Task 2's `serve_write`.
- Produces (crate-private, in `daemon/mod.rs`):
  - `enum Routed { Done, Started(PendingWork), NotYet(WriteRequest, WriteReply) }`
  - `fn route_write(root, request: WriteRequest, reply: WriteReply, queue, registry, make_registry, held, reopen_backoff, drain_in_flight: bool, full_reindex_in_flight: &mut Option<PendingFullReindex>, drain_rt, daemon_token, scip_import_in_flight: bool) -> Routed`
  - `try_start_full_reindex(root, reply: WriteReply, queue, make_registry, drain_in_flight, drain_rt, daemon_token) -> Result<Option<PendingFullReindex>, WriteReply>`
  - `try_start_scip_import(root, reply: WriteReply, scip_path: &Path, enriched_ast_generation: Option<i64>, registry, held, reopen_backoff, drain_in_flight, full_reindex_in_flight, scip_import_in_flight, drain_rt, daemon_token) -> Result<Option<PendingScipImport>, WriteReply>`
    - `Ok(Some(_))`: started. `Ok(None)`: answered already (an early failure). `Err(reply)`: not yet.
  - `serve_request_locked(root, request: &WriteRequest, reply: WriteReply, registry, held, reopen_backoff, drain_in_flight) -> Result<(), WriteReply>`
  - `finish_full_reindex(root, replies: Vec<WriteReply>, …)` and `finish_scip_import(root, reply: WriteReply, …)`
  - `PendingFullReindex { task, replies: Vec<WriteReply> }` with `join(&mut self, reply: WriteReply)`; `PendingScipImport { task, reply, indexer_label }`

- [ ] **Step 1: Write the failing tests.** In `mod.rs`, replace `a_request_whose_client_is_gone_is_discarded_without_being_served` (~L4265) with the two tests below. Adapt `a_full_reindex_requested_while_one_runs_joins_it_instead_of_queuing_another` (~L4205) to `route_write` with `WriteReply::channel()` pairs:

```rust
    #[test]
    fn a_full_reindex_requested_while_one_runs_joins_it_instead_of_queuing_another() {
        // Setup as before (project, queue, registry, held, runtime, token),
        // minus the requests directory.
        let (first, first_rx) = WriteReply::channel();
        let (second, second_rx) = WriteReply::channel();
        let mut running = None;
        let Routed::Started(PendingWork::FullReindex(p)) = route_write(
            root, WriteRequest::FullReindex, first, &queue, &registry, &make_registry,
            &mut held, &mut backoff, false, &mut running, &rt, &token, false,
        ) else { panic!("the first request starts a rebuild") };
        running = Some(p);
        assert!(matches!(
            route_write(root, WriteRequest::FullReindex, second, &queue, &registry,
                &make_registry, &mut held, &mut backoff, false, &mut running, &rt, &token, false),
            Routed::Done
        ));
        let running = running.unwrap();
        assert_eq!(running.replies.len(), 2, "one rebuild, two clients owed its answer");
        let (_guard, _) = finish_full_reindex(root, running.replies, &registry, &mut held,
            rt.block_on(running.task.join()));
        assert!(matches!(first_rx.recv().unwrap(), WriteResult::FullReindexOk { .. }));
        assert!(matches!(second_rx.recv().unwrap(), WriteResult::FullReindexOk { .. }));
    }

    #[test]
    fn a_deferred_write_whose_client_left_is_dropped_not_started() {
        // A drain in flight makes a FullReindex `NotYet`; its client then leaves.
        let (reply, _rx) = WriteReply::channel();
        let gone = reply.gone_flag();
        let Routed::NotYet(request, reply) = route_write(
            root, WriteRequest::FullReindex, reply, &queue, &registry, &make_registry,
            &mut held, &mut backoff, /* drain_in_flight */ true, &mut None, &rt, &token, false,
        ) else { panic!("a drain in flight defers a rebuild") };
        gone.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut deferred = std::collections::VecDeque::from([(request, reply)]);
        assert_eq!(drop_gone(&mut deferred), 1);
        assert!(deferred.is_empty());
    }
```

`drop_gone` is a helper this task adds, below. Rewrite `finish_drain_does_not_overwrite_a_reply_execute_drain_already_wrote` (~L4040) as:

```rust
    /// A drain task that panics drops its waiters during the unwind; each
    /// is still answered, with `DROPPED`, instead of hanging (Review Focus 2).
    #[test]
    fn a_panicking_drain_still_answers_every_waiter() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (a, a_rx) = WriteReply::channel();
        let (b, b_rx) = WriteReply::channel();
        let handle = rt.spawn_blocking(move || {
            let _held = (a, b);
            panic!("drain blew up");
        });
        let _ = rt.block_on(handle);
        for rx in [a_rx, b_rx] {
            assert!(matches!(rx.recv().unwrap(), WriteResult::Err { message } if message == DROPPED));
        }
    }
```

Migrate `drain_task_panic_surfaces_as_write_result_err_not_a_hang` (~L3990) the same way: build waiters with `WriteReply::channel()`, and assert on the receivers, not on files. The `route_or_serve_*` tests (~L4160-4420) become `route_write` tests:
- replace the `request_path` / `write_request` fixture with the request value plus a `WriteReply::channel()`;
- replace "the `.result` file exists / contains" with `rx.try_recv()` / `rx.recv_timeout(..)`;
- replace "the `.request` file remains" with `matches!(routed, Routed::NotYet(..))`.

In `drain.rs`, the tests build `Waiter { reply: WriteReply::channel().0, .. }`, keep the receiver, and assert on it. `drive_full_reindex_sync` takes a `WriteReply` and passes `in_flight.replies` to `finish_full_reindex`.

In `tests/watch_daemon.rs`:
- `out_of_scope_write_request_contends_with_a_held_index_lock`: keep the file-drop fixture. The bridge keeps file-drop working in this task. Replace its assertion ".request file must remain in place while contended" with: "no `.result` appears while the lock is held, and one appears within 10s of releasing it".
- `full_reindex_build_task_can_be_cancelled_before_it_starts_the_swap` and `scip_enrichment_task_is_cancellable_via_daemon_token` keep their file fixtures, unchanged.

- [ ] **Step 2: Run to see it fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib daemon::`
Expected: FAIL to compile (`route_write`, `Routed`, `drop_gone`, `Waiter.reply` missing).

- [ ] **Step 3: Implement the reply plumbing.**

`queue.rs`: `Waiter` derives `Debug` only, and `pub reply: crate::daemon::coordinator_port::WriteReply,` replaces `reply_path`. Remove the `PathBuf` import if it becomes unused.

`drain.rs`: `execute_drain` answers its own waiters on every outcome:

```rust
pub(crate) fn execute_drain(infigraph: &Infigraph, mut drained: DrainedQueue) -> Result<DrainOutcome> {
    let waiters = std::mem::take(&mut drained.waiters);
    let use_learned = waiters
        .iter()
        .any(|w| w.kind == WaiterKind::ResolveCalls && w.use_learned);
    match run_drain(infigraph, drained, use_learned) {
        Ok(outcome) => {
            for waiter in waiters {
                let result = reply_for(&waiter, &outcome);
                waiter.reply.send(result);
            }
            Ok(outcome)
        }
        Err(e) => {
            let message = format!("daemon drain failed: {e}");
            for waiter in waiters {
                waiter.reply.send(WriteResult::Err { message: message.clone() });
            }
            Err(e)
        }
    }
}
```

`run_drain` is today's body, from `let backend = …` through the `extractions.truncate(extractions_count);` line, returning `Ok(DrainOutcome { extractions, resolve_stats, removals })`. `DrainOutcome` gains `pub removals: Vec<String>`, which today's per-waiter `RemoveFiles` count needs. `reply_for` is today's per-waiter `match waiter.kind { … }` block, lifted into:

```rust
/// Scoped to the waiter's own requested paths when it named any -- see the
/// comment this block carried inside `execute_drain`.
fn reply_for(waiter: &crate::daemon::queue::Waiter, outcome: &DrainOutcome) -> WriteResult {
    // the existing match, reading `outcome.extractions`, `outcome.removals`
    // and `outcome.resolve_stats`
}
```

`use crate::daemon_protocol::write_atomic` goes, and the `?` on the reply write goes with it. The existing `a_targeted_waiter_reports_a_count_scoped_to_its_own_request_not_the_whole_batch` test pins that the extraction preserves behavior.

`mod.rs`:
- `InFlightDrain` loses `waiter_replies`, and its doc comment loses the "reply paths … retained" clause.
- `finish_drain` loses its third parameter and both `reply_err_to_waiters` calls. `execute_drain` answered on error, and `Drop` answers on panic. Update its doc comment accordingly.
- Delete `reply_err_to_waiters` and `reply_to_all`.
- In the scheduling block, delete the `waiter_replies` computation and its comment. The `drained` value moves into the task whole.

`PendingFullReindex`:

```rust
struct PendingFullReindex {
    task: Task<FullReindexTaskOutput>,
    /// Every client owed this rebuild's result: the request that started it,
    /// then each `FullReindex` that arrived while it ran (#164).
    replies: Vec<WriteReply>,
}

impl PendingFullReindex {
    fn join(&mut self, reply: WriteReply) {
        self.replies.push(reply);
    }
}
```

`PendingScipImport`: replace `request_path` and `reply_path` with `reply: WriteReply`.

`try_start_full_reindex`:
- The parameter `path: &Path` becomes `reply: WriteReply`, and the return type is `Result<Option<PendingFullReindex>, WriteReply>`.
- Each busy return (`drain_in_flight`, `AlreadyRunning`, the `Err` from `begin_index_op`) becomes `return Err(reply);`.
- The superseded-waiters loop becomes:

  ```rust
      for waiter in queue.lock().unwrap().drain().waiters {
          waiter.reply.send(WriteResult::Err {
              message: "superseded by a full reindex; resubmit if still needed".to_string(),
          });
      }
  ```

- The registry-failure branch becomes `reply.send(WriteResult::Err { message: format!(…) }); drop(guard); return Ok(None);`.
- The success return is `Ok(Some(PendingFullReindex { task, replies: vec![reply] }))`.

`try_start_scip_import`, with the same pattern:
- `path` becomes `reply: WriteReply`, and `scip_path: PathBuf` becomes `scip_path: &Path`.
- Every `return None` on busy or backoff becomes `return Err(reply)`.
- The task closure captures `let scip_path = scip_path.to_path_buf();`.
- The success return is `Ok(Some(PendingScipImport { task, reply, indexer_label }))`.

`finish_scip_import` takes `reply: WriteReply` and calls `reply.send(write_result)` on each of its three paths, instead of the `serde_json::to_string` + `write_atomic` pairs.

`finish_full_reindex` takes `replies: Vec<WriteReply>` by value. Add a local helper and replace every `if let Ok(json) = serde_json::to_string(&result) { reply_to_all(reply_paths, &json); }` with `answer_all(replies, result)`:

```rust
/// Give one rebuild's result to every client waiting on it (#164).
fn answer_all(replies: Vec<WriteReply>, result: WriteResult) {
    for reply in replies {
        reply.send(result.clone());
    }
}
```

Each early-return path uses `replies` once, so the move checks. If a path compiles to "use of moved value", that path answered twice before, and it's a bug to fix, not a clone to add.

`serve_request_locked`:

```rust
fn serve_request_locked(
    root: &Path,
    request: &WriteRequest,
    reply: WriteReply,
    registry: &Arc<crate::lang::LanguageRegistry>,
    held: &mut HeldPrism,
    reopen_backoff: &mut ReopenBackoff,
    drain_in_flight: bool,
) -> Result<(), WriteReply> {
    // While backing off or contended the write stays deferred, served on a
    // later tick.
    if drain_in_flight || !reopen_backoff.should_attempt() {
        return Err(reply);
    }
    match begin_index_op(root, "infigraph daemon", Duration::from_secs(30)) {
        Ok(IndexOpOutcome::Acquired(_guard)) => match watch_db(root, registry, held) {
            Ok(prism) => {
                reopen_backoff.record_success();
                reply.send(crate::daemon_protocol::serve_write(&prism, request));
                Ok(())
            }
            Err(e) => {
                log_reopen_failure("daemon", reopen_backoff, &e);
                Err(reply)
            }
        },
        Ok(o @ IndexOpOutcome::AlreadyRunning(_)) => {
            eprintln!(
                "[daemon] request-serving busy ({}), retrying next tick",
                o.skip_note().unwrap_or_default()
            );
            Err(reply)
        }
        Err(e) => {
            eprintln!("[daemon] request-serving busy ({e}), retrying next tick");
            fault::record_if_fault(&root.join(".infigraph"), &e, fault::FaultClass::of);
            Err(reply)
        }
    }
}
```

Keep its doc comment, replacing "leaves the `.request` file in place" with "hands the reply back, so the write stays deferred".

- [ ] **Step 4: Implement `route_write`** in place of `route_or_serve_request`:

```rust
/// What became of one write the coordinator tried to route.
enum Routed {
    /// Queued, joined, served, or already answered.
    Done,
    /// Background work the loop must track and reap.
    Started(PendingWork),
    /// Cannot start yet (busy, backing off): deferred again, in order.
    NotYet(WriteRequest, WriteReply),
}

/// Routes one write: the four index-shaped variants join the shared queue
/// with their reply as a waiter; `FullReindex` and `ScipImport` start
/// background work (or join the running rebuild, #164); everything else is
/// served under `index.lock` on this thread.
#[allow(clippy::too_many_arguments)]
fn route_write<MR>(
    root: &Path,
    request: WriteRequest,
    reply: WriteReply,
    queue: &Arc<Mutex<crate::daemon::queue::IndexWorkQueue>>,
    registry: &Arc<crate::lang::LanguageRegistry>,
    make_registry: &MR,
    held: &mut HeldPrism,
    reopen_backoff: &mut ReopenBackoff,
    drain_in_flight: bool,
    full_reindex_in_flight: &mut Option<PendingFullReindex>,
    drain_rt: &tokio::runtime::Runtime,
    daemon_token: &CancellationToken,
    scip_import_in_flight: bool,
) -> Routed
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    use crate::daemon::queue::{Waiter, WaiterKind};

    // Each queue arm holds the queue lock across its items-plus-waiter pair,
    // so a drain scheduled concurrently can never take the items without the
    // waiter blocked on them.
    match request {
        WriteRequest::Index { paths: None } => {
            let mut q = queue.lock().unwrap();
            q.mark_whole_project();
            q.add_waiter(Waiter { kind: WaiterKind::Index, use_learned: false, reply, paths: None });
            Routed::Done
        }
        WriteRequest::Index { paths: Some(paths) } => {
            let mut q = queue.lock().unwrap();
            let mut rel_paths = Vec::with_capacity(paths.len());
            for p in paths {
                // (keep the existing absolute-path comment)
                let rel = p
                    .strip_prefix(root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| p.to_string_lossy().replace('\\', "/"));
                q.add_raw(rel.clone());
                rel_paths.push(rel);
            }
            q.add_waiter(Waiter { kind: WaiterKind::Index, use_learned: false, reply, paths: Some(rel_paths) });
            Routed::Done
        }
        // (keep the existing comment on why `existing_hashes_empty` is dropped)
        WriteRequest::UpsertFilesBulk { extractions_path, .. } => {
            match crate::daemon_protocol::read_extractions_json(&extractions_path) {
                Ok(extractions) => {
                    let mut q = queue.lock().unwrap();
                    let rel_paths: Vec<String> = extractions.iter().map(|e| e.file.clone()).collect();
                    for extraction in extractions {
                        q.add_structured(extraction);
                    }
                    q.add_waiter(Waiter { kind: WaiterKind::UpsertFilesBulk, use_learned: false, reply, paths: Some(rel_paths) });
                    drop(q);
                    std::fs::remove_file(&extractions_path).ok();
                }
                Err(e) => reply.send(unreadable_sidecar(&extractions_path, &e)),
            }
            Routed::Done
        }
        WriteRequest::RemoveFiles { files } => {
            let mut q = queue.lock().unwrap();
            let rel_paths = files.clone();
            for f in files {
                q.add_removal(f);
            }
            q.add_waiter(Waiter { kind: WaiterKind::RemoveFiles, use_learned: false, reply, paths: Some(rel_paths) });
            Routed::Done
        }
        WriteRequest::ResolveCalls { extractions_path, use_learned } => {
            match crate::daemon_protocol::read_extractions_json(&extractions_path) {
                Ok(extractions) => {
                    let mut q = queue.lock().unwrap();
                    for extraction in extractions {
                        q.add_resolve_only(extraction);
                    }
                    // (keep the existing comment on why `paths` is None)
                    q.add_waiter(Waiter { kind: WaiterKind::ResolveCalls, use_learned, reply, paths: None });
                    drop(q);
                    std::fs::remove_file(&extractions_path).ok();
                }
                Err(e) => reply.send(unreadable_sidecar(&extractions_path, &e)),
            }
            Routed::Done
        }
        // #164: only one rebuild, ever. One already running answers this too.
        WriteRequest::FullReindex => match full_reindex_in_flight.as_mut() {
            Some(running) => {
                running.join(reply);
                Routed::Done
            }
            None => match try_start_full_reindex(
                root, reply, queue, make_registry, drain_in_flight, drain_rt, daemon_token,
            ) {
                Ok(Some(p)) => Routed::Started(PendingWork::FullReindex(p)),
                Ok(None) => Routed::Done,
                Err(reply) => Routed::NotYet(WriteRequest::FullReindex, reply),
            },
        },
        WriteRequest::ScipImport { scip_path, enriched_ast_generation } => match try_start_scip_import(
            root, reply, &scip_path, enriched_ast_generation, registry, held, reopen_backoff,
            drain_in_flight, full_reindex_in_flight.is_some(), scip_import_in_flight, drain_rt,
            daemon_token,
        ) {
            Ok(Some(p)) => Routed::Started(PendingWork::ScipImport(p)),
            Ok(None) => Routed::Done,
            Err(reply) => Routed::NotYet(WriteRequest::ScipImport { scip_path, enriched_ast_generation }, reply),
        },
        other => match serve_request_locked(root, &other, reply, registry, held, reopen_backoff, drain_in_flight) {
            Ok(()) => Routed::Done,
            Err(reply) => Routed::NotYet(other, reply),
        },
    }
}

fn unreadable_sidecar(path: &Path, e: &anyhow::Error) -> WriteResult {
    WriteResult::Err {
        message: format!("could not read the write's sidecar {}: {e:#}", path.display()),
    }
}

/// Remove deferred writes whose clients have disconnected (D3), returning
/// how many.
fn drop_gone(deferred: &mut VecDeque<(WriteRequest, WriteReply)>) -> usize {
    let before = deferred.len();
    deferred.retain(|(_, reply)| !reply.is_gone());
    before - deferred.len()
}
```

- [ ] **Step 5: Wire the loop.**

Rename the local `control_port`/`control_rx` to `port`/`port_rx` throughout `run_write_coordinator`.

Before the loop:

```rust
    // #204 transitional: a `.request` file becomes a deferred write whose
    // answer goes back as its `.result`. Deleted with the file-drop path.
    let mut file_replies: Vec<(mpsc::Receiver<WriteResult>, PathBuf)> = Vec::new();
```

Replace the body of the `if let Ok(entries) = std::fs::read_dir(&requests_dir)` block (~L1573-1597) with:

```rust
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_none_or(|ext| ext != "request") {
                        continue;
                    }
                    liveness.touch();
                    if crate::daemon_protocol::request_client_is_gone(&path) {
                        crate::daemon_protocol::discard_request(&path);
                        continue;
                    }
                    let Ok(contents) = std::fs::read_to_string(&path) else { continue };
                    let result_path = path.with_extension("result");
                    std::fs::remove_file(&path).ok();
                    match serde_json::from_str::<WriteRequest>(&contents) {
                        Ok(request) => {
                            let (reply, rx) = WriteReply::channel();
                            file_replies.push((rx, result_path));
                            deferred.push_back((request, reply));
                        }
                        Err(e) => {
                            let result = WriteResult::Err { message: format!("failed to read/parse request: {e}") };
                            let _ = crate::daemon_protocol::write_atomic(&result_path, &serde_json::to_string(&result).unwrap_or_default());
                        }
                    }
                }
```

Right after that `read_dir` block, still inside `if serve_requests`, route everything deferred:

```rust
            // #204: every write waits here until it can start, in arrival
            // order. A write whose client has left is dropped, not started (D3).
            let dropped = drop_gone(&mut deferred);
            if dropped > 0 {
                eprintln!("[daemon] dropped {dropped} write(s): their clients disconnected");
            }
            for (request, reply) in std::mem::take(&mut deferred) {
                match route_write(
                    root, request, reply, &queue, &shared_registry, &make_registry,
                    &mut held_prism, &mut reopen_backoff, drain_in_flight.is_some(),
                    &mut full_reindex_in_flight, &drain_rt, daemon_token,
                    scip_import_in_flight.is_some(),
                ) {
                    Routed::Done => {}
                    Routed::Started(PendingWork::FullReindex(p)) => full_reindex_in_flight = Some(p),
                    Routed::Started(PendingWork::ScipImport(p)) => scip_import_in_flight = Some(p),
                    Routed::NotYet(request, reply) => deferred.push_back((request, reply)),
                }
            }
            file_replies.retain(|(rx, result_path)| match rx.try_recv() {
                Ok(result) => {
                    let _ = crate::daemon_protocol::write_atomic(result_path, &serde_json::to_string(&result).unwrap_or_default());
                    false
                }
                Err(mpsc::TryRecvError::Empty) => true,
                Err(mpsc::TryRecvError::Disconnected) => false,
            });
```

In the two reap blocks, drop the `request_path` destructuring and the `std::fs::remove_file(&request_path)` lines, and pass `replies`/`reply`. In `work_in_flight!`, replace `|| has_pending_request(&root.join(".infigraph").join("requests"))` with:

```rust
                || !deferred.is_empty()
                || port.in_flight() > 0
```

Delete `has_pending_request` and its test module `pending_request_tests`. In the post-loop teardown, after `drop(port_rx)` and the `wait_idle`, add:

```rust
    for (_, reply) in deferred.drain(..) {
        reply.send(WriteResult::Err { message: coordinator_port::SHUTTING_DOWN.to_string() });
    }
```

In the teardown's drain / full-reindex / SCIP-import joins, pass `in_flight.replies`/`in_flight.reply`, and delete their `remove_file(&in_flight.request_path)` lines. After those joins, flush `file_replies` once, with the same `retain` body.

- [ ] **Step 6: Run to see it pass**

Run:
- `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib daemon::`
- `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test watch_daemon --test daemon_protocol_e2e --test daemon_protocol_watcher_wiring --test watch_control --test daemon_kuzu_backend -- --test-threads=1`
- `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_kuzu_e2e --test daemon_control -- --test-threads=1`

Expected: all PASS. File-drop still round-trips through the bridge, and `a_legacy_watch_control_request_file_gets_a_prompt_error` still gets its `Err` through the parse-failure branch.

- [ ] **Step 7: Commit**

```bash
git add -A crates/infigraph-core
git commit --no-verify -m "refactor(daemon): answer writes through WriteReply; one deferred queue (#204)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```

---

### Task 5: In-process submitters: compaction, auto-recovery, SCIP import

**Files:**
- Modify: `crates/infigraph-core/src/recovery.rs` (`drain_recovery_sentinel` L212-242, tests L319-363)
- Modify: `crates/infigraph-core/src/daemon/mod.rs`:
  - the recovery call ~L1486
  - the compaction submit ~L1560-1566
  - delete `submit_compaction_rebuild` ~L2594-2613
  - `FullReindexCallback` L285
  - `spawn_scip_enrich` L170-181 and its two call sites (~L1262, ~L1348)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (the `on_full_reindex` closure L609-740)
- Modify: `crates/infigraph-core/tests/watch_daemon.rs:1118`, `crates/infigraph-core/tests/daemon_scip_staleness.rs:75` (the callback arity)
- Modify: `crates/infigraph-core/tests/daemon_kuzu_e2e.rs` (`a_third_recovery_trigger…` ~L1500-1535)

**Interfaces:**
- Consumes: Task 3's `WriteSubmitter`, `WriteReply::internal()`. Task 4's `deferred`.
- Produces:
  - `pub fn drain_recovery_sentinel(infigraph_dir: &Path) -> anyhow::Result<bool>` (true: a rebuild is wanted)
  - `pub type FullReindexCallback = dyn Fn(PathBuf, ScipEnrichJob, CancellationToken, WriteSubmitter) + Send + Sync;`
  - `fn request_internal_rebuild(deferred: &mut VecDeque<(WriteRequest, WriteReply)>, running: bool)`

- [ ] **Step 1: Write the failing tests.**

In `recovery.rs`, change the two sentinel tests' "a `.request` file exists" assertions:

```rust
        assert!(drain_recovery_sentinel(dir).unwrap(), "a rebuild is wanted");
        assert!(!dir.join("requests").exists(), "and no file is written for it");
```

and, for the breaker test:

```rust
        assert!(!drain_recovery_sentinel(dir).unwrap(), "no rebuild once tripped");
```

In `tests/daemon_kuzu_e2e.rs::a_third_recovery_trigger…`, replace the `requests` count assertion with `assert!(!infigraph_core::recovery::drain_recovery_sentinel(&infigraph_dir).unwrap())`, and drop the earlier bare call.

In `mod.rs` tests:

```rust
    #[test]
    fn internal_rebuilds_collapse_while_one_is_waiting_or_running() {
        let mut deferred = VecDeque::new();
        request_internal_rebuild(&mut deferred, false);
        request_internal_rebuild(&mut deferred, false);
        assert_eq!(deferred.len(), 1, "one waiting rebuild absorbs the next request");
        let mut deferred = VecDeque::new();
        request_internal_rebuild(&mut deferred, true);
        assert!(deferred.is_empty(), "a running rebuild absorbs it too");
        let (client, _rx) = WriteReply::channel();
        let mut deferred = VecDeque::from([(WriteRequest::FullReindex, client)]);
        request_internal_rebuild(&mut deferred, false);
        assert_eq!(deferred.len(), 1, "a client's waiting rebuild counts");
    }
```

- [ ] **Step 2: Run to see it fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib recovery daemon::`
Expected: FAIL to compile (`request_internal_rebuild` missing; `drain_recovery_sentinel` returns `()`).

- [ ] **Step 3: Implement.**

`recovery.rs`: `drain_recovery_sentinel` returns `Result<bool>`:
- `Ok(false)` when nothing is pending;
- `Ok(false)` after tripping the breaker;
- `Ok(true)` after `record_recovery_attempt` and the audit line, in place of the `requests_dir` + `write_atomic` lines.

Rewrite its doc comment's middle sentence to: "under the crash-loop threshold, answers `true` -- the coordinator then queues a `FullReindex`, the same request type and code path `infigraph rebuild` uses".

`mod.rs`:

```rust
/// Queue a rebuild the daemon asked for itself (compaction, auto-recovery),
/// unless one is already waiting or running -- the same collapsing the fixed
/// `compaction.request` file name used to give for free. A client's waiting
/// `FullReindex` counts: it rebuilds the same tree.
fn request_internal_rebuild(deferred: &mut VecDeque<(WriteRequest, WriteReply)>, running: bool) {
    if running || deferred.iter().any(|(r, _)| *r == WriteRequest::FullReindex) {
        return;
    }
    deferred.push_back((WriteRequest::FullReindex, WriteReply::internal()));
}
```

The recovery call site:

```rust
            match crate::recovery::drain_recovery_sentinel(&infigraph_dir) {
                Ok(true) => request_internal_rebuild(&mut deferred, full_reindex_in_flight.is_some()),
                Ok(false) => {}
                Err(e) => eprintln!("[watch] recovery-sentinel handling failed: {e}"),
            }
```

The compaction `Ok(())` arm:

```rust
                        Ok(()) => {
                            eprintln!(
                                "[daemon] compaction: requesting a rebuild ({})",
                                if escalate { "escalated" } else { "drift" }
                            );
                            request_internal_rebuild(&mut deferred, full_reindex_in_flight.is_some());
                        }
```

Delete `submit_compaction_rebuild`, and update the recovery and compaction comments that mention "drop a request file" to say "queue a `FullReindex`".

`FullReindexCallback` gains the fourth parameter. `spawn_scip_enrich` gains `submitter: WriteSubmitter` and calls `cb(root, job, token, submitter)`. At both call sites, pass `WriteSubmitter::new(port.clone())`.

In `info_commands.rs`, the closure takes `submit: infigraph_core::daemon::coordinator_port::WriteSubmitter` as its fourth argument. Delete the `requests_dir` line and replace the `submit_write_request_cancellable(…)` call with:

```rust
                    match submit.submit(request, &token) {
```

Keep every match arm below it. Update the comment above it: "routed through the coordinator's own port -- no separate direct-write path for 'the daemon triggered this itself' vs 'an external caller asked for it'".

The two test callbacks take a fourth `_submit` argument.

- [ ] **Step 4: Run to see it pass**

Run:
- `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib recovery daemon::`
- `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test watch_daemon --test daemon_scip_staleness --test daemon_kuzu_e2e -- --test-threads=1`

Expected: PASS. `daemon_kuzu_e2e`'s auto-recovery test (the one before the breaker test) still clears the sentinel and records exactly one attempt, now through the deferred queue.

- [ ] **Step 5: Commit**

```bash
git add -A crates/
git commit --no-verify -m "feat(daemon): the daemon's own writes go in-process, not through files (#204)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```

---

### Task 6: The read service serves writes

**Files:**
- Modify: `crates/infigraph-core/src/daemon/read_endpoint.rs` (`impl ReadStream`, ~L354)
- Modify: `crates/infigraph-core/src/daemon/read_service.rs` (`serve_one`'s `Write` arm; `spawn_control` L439-481)
- Test: `crates/infigraph-core/tests/read_service.rs` (a new section after the #155 section)

**Interfaces:**
- Consumes: Task 1's `WriteFrame`. Task 3's `admit_write`, `WriteReply`, `enter`.
- Produces:
  - `pub(crate) fn ReadStream::peer_closed(&self) -> bool`
  - `pub const GONE_POLL: Duration = Duration::from_millis(250);`
  - daemon behavior: an admission frame, then the outcome frame
  - removes `WRITES_NOT_SERVED`

- [ ] **Step 1: Write the failing tests** (in `tests/read_service.rs`, after `a_control_reply_carries_the_coordinators_outcome`)

```rust
// ---- #204: writes on the read socket ----

use infigraph_core::daemon::coordinator_port::{PortMsg, PORT_QUEUE};
use infigraph_core::daemon::read_protocol::WriteFrame;
use infigraph_core::daemon_protocol::{WriteRequest, WriteResult};

fn send_write(root: &Path) -> infigraph_core::daemon::read_endpoint::ReadStream {
    let mut s = ReadEndpoint::for_root(root).connect().unwrap();
    write_op(&mut s, &WriteFrame { write: WriteRequest::FullReindex }).unwrap();
    s
}

#[test]
fn a_write_is_admitted_then_answered_with_the_coordinators_result() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _port, rx, _l) = control_service(dir.path());
    let mut s = send_write(dir.path());
    assert!(matches!(read_reply::<_, ()>(&mut s).unwrap(), Some(OpReply::Ok(()))));
    let PortMsg::Write { request, reply } = rx.recv().unwrap() else { panic!("a write") };
    assert_eq!(request, WriteRequest::FullReindex);
    reply.send(WriteResult::Ok { total_files: 3, indexed_files: 3 });
    assert!(matches!(
        read_reply::<_, WriteResult>(&mut s).unwrap(),
        Some(OpReply::Ok(WriteResult::Ok { total_files: 3, .. }))
    ));
    svc.shutdown();
}

#[cfg(unix)]
#[test]
fn a_client_that_disconnects_marks_its_write_gone() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _port, rx, _l) = control_service(dir.path());
    let mut s = send_write(dir.path());
    let _ = read_reply::<_, ()>(&mut s).unwrap();
    let PortMsg::Write { reply, .. } = rx.recv().unwrap() else { panic!() };
    drop(s);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !reply.is_gone() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(reply.is_gone(), "noticed within a few GONE_POLLs");
    svc.shutdown();
}

#[cfg(unix)]
#[test]
fn a_write_waiting_for_a_slot_gives_up_when_its_client_leaves() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, port, rx, _l) = control_service(dir.path());
    let held: Vec<_> = (0..PORT_QUEUE).map(|_| port.submit_control(ControlRequest {
        role: WatchRole::Code, action: WatchAction::Stop }).unwrap()).collect();
    let s = send_write(dir.path());
    std::thread::sleep(Duration::from_millis(100));
    drop(s);
    std::thread::sleep(Duration::from_millis(100));
    // The queue still holds only the 64 controls: the abandoned write gave
    // up rather than taking a slot once one frees.
    let arrived: Vec<_> = rx.try_iter().collect();
    drop(held);
    assert_eq!(arrived.len(), PORT_QUEUE);
    assert!(arrived.iter().all(|m| matches!(m, PortMsg::Control(_))));
    assert!(port.wait_idle(Duration::from_secs(2)), "its thread ended");
    svc.shutdown();
}

#[test]
fn a_write_to_a_shutting_down_coordinator_is_refused_before_admission() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _port, rx, _l) = control_service(dir.path());
    drop(rx);
    let mut s = send_write(dir.path());
    assert!(matches!(read_reply::<_, ()>(&mut s).unwrap(), Some(OpReply::Err(m)) if m.contains("shutting down")));
    svc.shutdown();
}

#[test]
fn a_malformed_write_frame_is_closed_without_an_answer() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _port, _rx, _l) = control_service(dir.path());
    let mut s = ReadEndpoint::for_root(dir.path()).connect().unwrap();
    let body = br#"{"write":{"NoSuchVariant":null}}"#;
    s.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
    s.write_all(body).unwrap();
    assert!(read_reply::<_, ()>(&mut s).unwrap().is_none(), "EOF: reads as Incompatible");
    svc.shutdown();
}
```

(Add `use std::io::Write as _;` if the file does not already import it.)

- [ ] **Step 2: Run to see it fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test read_service`
Expected: the new tests FAIL. The service still refuses with `WRITES_NOT_SERVED`.

- [ ] **Step 3: Implement `peer_closed`** in `impl ReadStream` (`read_endpoint.rs`):

```rust
    /// Whether the client has hung up. A write's client sends nothing after
    /// its frame, so any readable state -- EOF, or stray bytes -- means the
    /// connection is over (#204 D2). Never blocks.
    #[cfg(unix)]
    pub(crate) fn peer_closed(&self) -> bool {
        use std::os::fd::{AsFd as _, AsRawFd as _};
        let interprocess::local_socket::Stream::UdSocket(s) = &self.inner;
        let mut pollfd = libc::pollfd {
            fd: s.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd, borrowed from a stream that outlives the call.
        unsafe { libc::poll(&mut pollfd, 1, 0) > 0 }
    }

    /// Windows named pipes: not detected; the write's work runs and its
    /// answer is discarded. Tracked with the reader-thread gap in #206.
    #[cfg(not(unix))]
    pub(crate) fn peer_closed(&self) -> bool {
        false
    }
```

- [ ] **Step 4: Implement the service side.** Refactor `spawn_control` into a shared spawner and two bodies (DRY: control and writes share the thread-plus-handover logic):

```rust
/// Runs `serve` on its own thread, never a pool worker: control can wait
/// `CONTROL_REPLY_TIMEOUT` and a write minutes, and a few of those on the
/// pool would stop every read. Counted through the port, never joined by
/// the service (see `InFlightGuard`).
fn spawn_on_port(
    port: Option<Arc<CoordinatorPort>>,
    name: &'static str,
    mut stream: ReadStream,
    serve: impl FnOnce(&CoordinatorPort, &mut ReadStream) + Send + 'static,
) {
    let Some(port) = port else {
        let _ = write_reply::<_, ()>(&mut stream, &OpReply::Err(NO_CONTROL.to_string()));
        return;
    };
    let guard = port.enter();
    // The stream is handed over only once the thread exists, so a failed
    // spawn can still answer the client instead of hanging up on it -- a
    // hang-up would read as an incompatible build.
    let (hand_over, take) = mpsc::channel::<ReadStream>();
    let spawned = std::thread::Builder::new().name(name.into()).spawn(move || {
        let _guard = guard;
        if let Ok(mut stream) = take.recv() {
            serve(&port, &mut stream);
        }
    });
    match spawned {
        Ok(_) => {
            let _ = hand_over.send(stream);
        }
        Err(e) => {
            eprintln!("[{name}] could not start a thread: {e}");
            let _ = write_reply::<_, ()>(
                &mut stream,
                &OpReply::Err(format!("the daemon could not start a thread: {e}")),
            );
        }
    }
}

fn serve_control(port: &CoordinatorPort, request: ControlRequest, stream: &mut ReadStream) {
    let reply = match port.submit_control(request) {
        // ... the existing body of spawn_control's thread, unchanged
    };
    let _ = write_reply(stream, &reply);
}

/// One socket write (#204): admit it, ack, wait for the coordinator's
/// answer while watching for the client to leave, then answer.
fn serve_socket_write(port: &CoordinatorPort, request: WriteRequest, stream: &mut ReadStream) {
    let (reply, rx) = WriteReply::channel();
    let gone = reply.gone_flag();
    if let Err(msg) = port.admit_write(request, reply, || stream.peer_closed()) {
        let _ = write_reply::<_, ()>(stream, &OpReply::Err(msg));
        return;
    }
    if write_reply(stream, &OpReply::Ok(())).is_err() {
        gone.store(true, Ordering::SeqCst);
        return;
    }
    loop {
        match rx.recv_timeout(GONE_POLL) {
            Ok(result) => {
                let _ = write_reply(stream, &OpReply::Ok(result));
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stream.peer_closed() {
                    gone.store(true, Ordering::SeqCst);
                    return;
                }
            }
            // `Drop` answers every unsent reply, so this is unreachable in
            // practice; answer rather than hang up if it ever is not.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = write_reply::<_, WriteResult>(stream, &OpReply::Err(SHUTTING_DOWN.to_string()));
                return;
            }
        }
    }
}
```

`serve_one`'s arms become:

```rust
        ClientFrame::Control(ControlFrame { control: request }) => {
            spawn_on_port(control.cloned(), "infigraph-control", stream, move |port, s| {
                serve_control(port, request, s)
            });
            return Ok(());
        }
        ClientFrame::Write(WriteFrame { write: request }) => {
            spawn_on_port(control.cloned(), "infigraph-write", stream, move |port, s| {
                serve_socket_write(port, request, s)
            });
            return Ok(());
        }
```

Delete `spawn_control` and `WRITES_NOT_SERVED`. Add `pub const GONE_POLL: Duration = Duration::from_millis(250);` beside the other consts, documented "How often a waiting write checks that its client is still there (#204 D2)."

- [ ] **Step 5: Run to see it pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test read_service --test daemon_control_client --test daemon_control -- --test-threads=1`
Expected: PASS, including all #155 control tests.

- [ ] **Step 6: Commit**

```bash
git add -A crates/infigraph-core
git commit --no-verify -m "feat(daemon): the read service serves writes, with admission and disconnect (#204)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```

---

### Task 7: The write client, `daemon/writes.rs`

**Files:**
- Create: `crates/infigraph-core/src/daemon/writes.rs`
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (`pub mod writes;`)
- Modify: `crates/infigraph-core/src/daemon/control.rs` (`ControlError` L24-50, `exchange` L100-140, `query_status` / `send_control`)
- Modify: `crates/infigraph-core/src/daemon_protocol.rs` (move `blocking_fault` out; add `WriteRequest::kind`)
- Test: `crates/infigraph-core/tests/daemon_writes_client.rs` (create)

**Interfaces:**
- Consumes: Task 1's `WriteFrame::ACKED`; Task 6's daemon behavior.
- Produces:
  - `pub struct WriteOpts<'a> { pub timeout: Duration, pub cancel: Option<&'a CancellationToken> }`
  - `pub fn submit(root: &Path, request: &WriteRequest, opts: WriteOpts) -> anyhow::Result<WriteResult>`
  - `pub fn sidecar_path(root: &Path, ext: &str) -> PathBuf`
  - `pub const SIDECAR_DIR: &str = "write-tmp";`
  - `ControlError::Lost`
  - `fn exchange<O: DaemonOp, E: From<ControlError>>(stream, op, stop: impl FnMut() -> Option<E>) -> Result<O::Reply, E>` (crate-private in `control.rs`, made `pub(crate)`)
  - `pub fn WriteRequest::kind(&self) -> &'static str`

- [ ] **Step 1: Write the failing tests** (`tests/daemon_writes_client.rs`)

```rust
//! The write client (#204) against a real read service with a stub
//! coordinator, and against fake listeners on the real endpoint.
use std::sync::Arc;
use std::time::{Duration, Instant};

use infigraph_core::daemon::control::ControlError;
use infigraph_core::daemon::coordinator_port::{CoordinatorPort, PortMsg};
use infigraph_core::daemon::liveness::Liveness;
use infigraph_core::daemon::read_endpoint::ReadEndpoint;
use infigraph_core::daemon::read_protocol::{read_client_frame, write_reply, OpReply};
use infigraph_core::daemon::read_service::ReadService;
use infigraph_core::daemon::writes::{sidecar_path, submit, WriteOpts};
use infigraph_core::daemon_protocol::{WriteRequest, WriteRequestCancelled, WriteResult};

fn opts(timeout: Duration) -> WriteOpts<'static> {
    WriteOpts { timeout, cancel: None }
}

fn service(root: &std::path::Path) -> (ReadService, std::sync::mpsc::Receiver<PortMsg>) {
    let (port, rx) = CoordinatorPort::new(1800, 60);
    let svc = ReadService::start_serving(root, Arc::new(|| None), None, 2, Arc::new(Liveness::new()), Some(port)).unwrap();
    (svc, rx)
}

#[test]
fn a_write_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, rx) = service(dir.path());
    let answer = std::thread::spawn(move || {
        let PortMsg::Write { reply, .. } = rx.recv().unwrap() else { panic!() };
        reply.send(WriteResult::Ok { total_files: 1, indexed_files: 1 });
    });
    let r = submit(dir.path(), &WriteRequest::UpsertRepo { namespace: "n".into() }, opts(Duration::from_secs(5))).unwrap();
    assert!(matches!(r, WriteResult::Ok { total_files: 1, .. }));
    answer.join().unwrap();
    svc.shutdown();
}

#[test]
fn a_cancelled_write_returns_promptly_and_its_waiter_goes() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, rx) = service(dir.path());
    let token = tokio_util::sync::CancellationToken::new();
    let t = {
        let (root, token) = (dir.path().to_path_buf(), token.clone());
        std::thread::spawn(move || submit(&root, &WriteRequest::FullReindex,
            WriteOpts { timeout: Duration::from_secs(30), cancel: Some(&token) }))
    };
    let PortMsg::Write { reply, .. } = rx.recv().unwrap() else { panic!() };
    let cancelled_at = Instant::now();
    token.cancel();
    let err = t.join().unwrap().unwrap_err();
    assert!(err.downcast_ref::<WriteRequestCancelled>().is_some());
    assert!(cancelled_at.elapsed() < Duration::from_millis(500));
    #[cfg(unix)]
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !reply.is_gone() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(reply.is_gone(), "closing the connection is the withdrawal");
    }
    svc.shutdown();
}

#[test]
fn no_daemon_is_no_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let err = submit(dir.path(), &WriteRequest::FullReindex, opts(Duration::from_secs(1))).unwrap_err();
    assert_eq!(err.downcast_ref::<ControlError>(), Some(&ControlError::NoDaemon));
}

/// A fake daemon that reads one frame, then runs `then` on the stream.
fn fake(root: &std::path::Path, then: impl FnOnce(&mut infigraph_core::daemon::read_endpoint::ReadStream) + Send + 'static) -> std::thread::JoinHandle<()> {
    let listener = ReadEndpoint::for_root(root).bind().unwrap();
    std::thread::spawn(move || {
        let mut s = listener.accept().unwrap();
        let _ = read_client_frame(&mut s);
        then(&mut s);
    })
}

#[test]
fn eof_before_admission_is_incompatible() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), |_| {});
    let err = submit(dir.path(), &WriteRequest::FullReindex, opts(Duration::from_secs(5))).unwrap_err();
    assert_eq!(err.downcast_ref::<ControlError>(), Some(&ControlError::Incompatible));
    f.join().unwrap();
}

#[test]
fn eof_after_admission_is_lost_not_incompatible() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), |s| { write_reply(s, &OpReply::Ok(())).unwrap(); });
    let err = submit(dir.path(), &WriteRequest::FullReindex, opts(Duration::from_secs(5))).unwrap_err();
    assert_eq!(err.downcast_ref::<ControlError>(), Some(&ControlError::Lost));
    f.join().unwrap();
}

#[test]
fn an_admission_refusal_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), |s| { write_reply::<_, ()>(s, &OpReply::Err("the daemon is shutting down".into())).unwrap(); });
    let err = submit(dir.path(), &WriteRequest::FullReindex, opts(Duration::from_secs(5))).unwrap_err();
    assert!(matches!(err.downcast_ref::<ControlError>(), Some(ControlError::Refused(m)) if m.contains("shutting down")));
    f.join().unwrap();
}

#[test]
fn a_silent_daemon_times_out_at_the_callers_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), |s| {
        write_reply(s, &OpReply::Ok(())).unwrap();
        std::thread::sleep(Duration::from_secs(2));
    });
    let started = Instant::now();
    let err = submit(dir.path(), &WriteRequest::FullReindex, opts(Duration::from_millis(300))).unwrap_err();
    assert!(err.to_string().contains("FullReindex"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(1));
    f.join().unwrap();
}

#[test]
fn sidecars_live_under_write_tmp_and_are_unique() {
    let dir = tempfile::tempdir().unwrap();
    let a = sidecar_path(dir.path(), "extractions.json");
    let b = sidecar_path(dir.path(), "extractions.json");
    assert_ne!(a, b);
    assert!(a.starts_with(dir.path().join(".infigraph").join("write-tmp")));
    assert!(a.to_string_lossy().ends_with(".extractions.json"));
    assert!(a.parent().unwrap().is_dir(), "created on demand");
}
```

Move the three fault tests from `daemon_protocol.rs`'s `mod tests` into `writes.rs`'s `mod tests`:
- `a_latched_fault_fails_a_submit_fast_with_the_daemons_error`
- `a_fault_latched_mid_wait_ends_the_wait_and_withdraws_the_request`
- `a_growth_refusal_still_admits_a_full_reindex`

Their fault setup is unchanged. They call `submit(root, …, WriteOpts { … })`. The mid-wait one needs a listener: start a `service(root)` whose stub never answers, latch the fault after admission, and assert `DaemonFaulted` within 1s. Its "withdraws the request" check becomes "the stub's `reply.is_gone()` within 2s" (unix).

- [ ] **Step 2: Run to see it fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test daemon_writes_client`
Expected: FAIL to compile (`daemon::writes` missing).

- [ ] **Step 3: Generalize `exchange` and add `Lost`** in `control.rs`:

```rust
    /// The daemon accepted a write, then closed before answering: it exited
    /// while serving it.
    Lost,
```

with `Display`: `ControlError::Lost => f.write_str("the daemon exited while serving this request; see .infigraph/daemon.log")`.

```rust
/// How often a waiting caller's stop check runs.
const EXCHANGE_POLL: Duration = Duration::from_millis(50);

/// Stops at `deadline` with `Unresponsive`: status and control's wait.
fn by(deadline: Duration) -> impl FnMut() -> Option<ControlError> {
    let started = std::time::Instant::now();
    move || (started.elapsed() >= deadline).then_some(ControlError::Unresponsive)
}

/// Send `op` and wait for its reply, running `stop` every `EXCHANGE_POLL`;
/// its first `Some` ends the wait with that error.
///
/// (Keep the existing paragraph on the helper-thread read, unix `shutdown(2)`
/// on abandonment, and the Windows gap in #206.) Abandoning closes the
/// connection, which a daemon serving a write reads as its client leaving.
pub(crate) fn exchange<O: DaemonOp, E: From<ControlError>>(
    mut stream: ReadStream,
    op: &O,
    mut stop: impl FnMut() -> Option<E>,
) -> Result<O::Reply, E>
where
    O::Reply: Send + 'static,
{
    write_op(&mut stream, op).map_err(|_| E::from(ControlError::Unresponsive))?;
    #[cfg(unix)]
    let hangup = Arc::new(Mutex::new(stream.lease_shutdown()));
    #[cfg(not(unix))]
    let hangup = Arc::new(Mutex::new(None::<()>));
    let reader_hangup = hangup.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let got = read_outcome::<O>(&mut stream);
        reader_hangup.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(stream);
        let _ = tx.send(got);
    });
    loop {
        match rx.recv_timeout(EXCHANGE_POLL) {
            Ok(got) => return got.map_err(E::from),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(E::from(ControlError::Incompatible)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(e) = stop() {
                    #[cfg(unix)]
                    if let Some(h) = hangup.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        h.shutdown();
                    }
                    #[cfg(not(unix))]
                    let _ = &hangup;
                    return Err(e);
                }
            }
        }
    }
}

/// The reply frames for one op: an admission frame first when `O::ACKED`
/// (#204), so an EOF after it is `Lost`, not `Incompatible`.
fn read_outcome<O: DaemonOp>(stream: &mut ReadStream) -> Result<O::Reply, ControlError> {
    if O::ACKED {
        match read_reply::<_, ()>(stream) {
            Ok(Some(OpReply::Ok(()))) => {}
            Ok(Some(OpReply::Err(m))) => return Err(ControlError::Refused(m)),
            Ok(None) | Err(_) => return Err(ControlError::Incompatible),
        }
    }
    match read_reply::<_, O::Reply>(stream) {
        Ok(Some(OpReply::Ok(v))) => Ok(v),
        Ok(Some(OpReply::Err(m))) => Err(ControlError::Refused(m)),
        Ok(None) | Err(_) if O::ACKED => Err(ControlError::Lost),
        Ok(None) | Err(_) => Err(ControlError::Incompatible),
    }
}
```

`query_status` calls `exchange(stream, &StatusFrame::default(), by(STATUS_DEADLINE))`, and `send_control` calls `exchange(stream, &ControlFrame { … }, by(CONTROL_DEADLINE))`. Make `not_connected` `pub(crate)`.

- [ ] **Step 4: Implement `writes.rs`:**

```rust
//! The write client (#204): the only sender of `Write` frames. Every routed
//! write -- `DaemonKuzuBackend`, `cmd_index`'s full reindex,
//! `index_via_daemon` -- comes through `submit`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::control::{exchange, not_connected};
use super::read_endpoint::connect_allowing_for_startup;
use super::read_protocol::WriteFrame;
use crate::daemon_protocol::{DaemonFaulted, WriteRequest, WriteRequestCancelled, WriteResult};

/// Where bulk payloads wait for the daemon, under `.infigraph/`.
pub const SIDECAR_DIR: &str = "write-tmp";

pub struct WriteOpts<'a> {
    pub timeout: Duration,
    pub cancel: Option<&'a CancellationToken>,
}

/// The latched fault that should stop `request` from waiting on the daemon,
/// if any (#165).
fn blocking_fault(infigraph_dir: &Path, request: &WriteRequest) -> Option<DaemonFaulted> {
    let fault = crate::daemon::fault::live_fault(infigraph_dir)?;
    (!fault.class.admits(request)).then_some(DaemonFaulted(fault))
}

/// Send one write and wait for its result. Fails fast on a latched fault,
/// before connecting and while waiting; `opts.cancel` and `opts.timeout` end
/// the wait, and ending it closes the connection -- the daemon reads that
/// as the client leaving and drops the write if it has not started.
pub fn submit(root: &Path, request: &WriteRequest, opts: WriteOpts) -> anyhow::Result<WriteResult> {
    let infigraph_dir = root.join(".infigraph");
    if let Some(faulted) = blocking_fault(&infigraph_dir, request) {
        return Err(anyhow::Error::new(faulted));
    }
    // A routed write is a use of the daemon for as long as it waits: its
    // lease must not be released under it (`daemon::lease`).
    let _use = crate::daemon::lease::in_use(root);
    let stream = connect_allowing_for_startup(root)
        .map_err(|_| anyhow::Error::new(not_connected(root)))?;
    let started = Instant::now();
    exchange(stream, &WriteFrame { write: request.clone() }, || {
        if opts.cancel.is_some_and(|t| t.is_cancelled()) {
            return Some(anyhow::Error::new(WriteRequestCancelled));
        }
        if let Some(faulted) = blocking_fault(&infigraph_dir, request) {
            return Some(anyhow::Error::new(faulted));
        }
        (started.elapsed() >= opts.timeout).then(|| {
            anyhow::anyhow!(
                "the daemon did not answer the {} write within {:?}",
                request.kind(),
                opts.timeout
            )
        })
    })
}

/// Distinguishes sidecars one process creates within the same nanosecond.
static SIDECAR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh sidecar path for a bulk payload, `<pid>-<nanos>-<n>.<ext>`, with
/// its directory created. The caller removes it on every error; the daemon
/// removes it once read; the daemon's startup sweeps leftovers older than 6h.
pub fn sidecar_path(root: &Path, ext: &str) -> PathBuf {
    let dir = root.join(".infigraph").join(SIDECAR_DIR);
    let _ = std::fs::create_dir_all(&dir);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let n = SIDECAR_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{}-{nanos}-{n}.{ext}", std::process::id()))
}
```

In `daemon_protocol.rs`:
- delete `blocking_fault` (it moved);
- keep `DaemonFaulted` and `WriteRequestCancelled` there, since callers downcast them by that path;
- add:

```rust
impl WriteRequest {
    /// The variant's name, for messages.
    pub fn kind(&self) -> &'static str {
        match self {
            WriteRequest::Index { .. } => "Index",
            WriteRequest::ScipImport { .. } => "ScipImport",
            WriteRequest::IngestStructured { .. } => "IngestStructured",
            WriteRequest::UpsertRepo { .. } => "UpsertRepo",
            WriteRequest::DeriveTestedBy { .. } => "DeriveTestedBy",
            WriteRequest::UpsertSimilarEdge { .. } => "UpsertSimilarEdge",
            WriteRequest::WriteCallsServiceEdges { .. } => "WriteCallsServiceEdges",
            WriteRequest::WriteCrossServiceEdges { .. } => "WriteCrossServiceEdges",
            WriteRequest::UpsertDependencies { .. } => "UpsertDependencies",
            WriteRequest::ReplaceConcerns { .. } => "ReplaceConcerns",
            WriteRequest::ReplaceTaintFlows { .. } => "ReplaceTaintFlows",
            WriteRequest::ReplaceResolvesTo { .. } => "ReplaceResolvesTo",
            WriteRequest::StoreClusters { .. } => "StoreClusters",
            WriteRequest::StoreConfigBindings { .. } => "StoreConfigBindings",
            WriteRequest::UpsertFilesBulk { .. } => "UpsertFilesBulk",
            WriteRequest::RemoveFiles { .. } => "RemoveFiles",
            WriteRequest::ResolveCalls { .. } => "ResolveCalls",
            WriteRequest::FullReindex => "FullReindex",
        }
    }
}
```

The file-drop `submit_write_request_named_cancellable` still calls `blocking_fault(staging_dir, …)` until Task 9. Point it at `crate::daemon::writes::blocking_fault(staging_dir.parent()?, …)`, making `blocking_fault` `pub(crate)`, rather than keeping two copies.

- [ ] **Step 5: Run to see it pass**

Run:
- `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --test daemon_writes_client --lib daemon::writes`
- `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_control_client --test daemon_control -- --test-threads=1`

Expected: PASS. Status and control behave as before on the generic `exchange`.

- [ ] **Step 6: Commit**

```bash
git add -A crates/infigraph-core
git commit --no-verify -m "feat(daemon): the write client, on #155's exchange (#204)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```

---

### Task 8: Every caller writes over the socket

**Files:**
- Modify: `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs` (the 18 write methods L237-700; delete `staging_dir` L113-115)
- Modify: `crates/infigraph-core/src/lib.rs` (`index_via_daemon` L820-848)
- Modify: `crates/infigraph-cli/src/index.rs` (`cmd_index` L170-175)
- Test: `crates/infigraph-core/tests/daemon_kuzu_backend.rs` (`spawn_one_request_server` L26-60, and the sidecar-cleanup tests L186, L244), `tests/daemon_protocol_e2e.rs`, `tests/daemon_protocol_watcher_wiring.rs`, `tests/watch_control.rs:139-165`

**Interfaces:**
- Consumes: Task 7's `writes::{submit, WriteOpts, sidecar_path}`.
- Produces: `DaemonKuzuBackend::{write, write_with_sidecar, unexpected}` (private helpers).

- [ ] **Step 1: Move the tests onto the socket.**
  - `tests/daemon_kuzu_backend.rs::spawn_one_request_server` becomes a real `ReadService` whose stub coordinator serves exactly one write against the test's `Infigraph`:

    ```rust
    /// A read service whose stub coordinator serves exactly one write with
    /// `serve_write`, against a graph this test opened itself.
    fn spawn_one_write_server(project_dir: &Path) -> (ReadService, std::thread::JoinHandle<()>) {
        let (port, rx) = CoordinatorPort::new(1800, 60);
        let svc = ReadService::start_serving(project_dir, Arc::new(|| None), None, 2,
            Arc::new(Liveness::new()), Some(port)).unwrap();
        let root = project_dir.to_path_buf();
        let server = std::thread::spawn(move || {
            let PortMsg::Write { request, reply } = rx.recv().unwrap() else { panic!("a write") };
            let mut infigraph = Infigraph::open(&root, bundled_registry().unwrap()).unwrap();
            infigraph.init().unwrap();
            reply.send(infigraph_core::daemon_protocol::serve_write(&infigraph, &request));
        });
        (svc, server)
    }
    ```

    Callers change `let server = spawn_one_request_server(dir);` to `let (svc, server) = spawn_one_write_server(dir);`, and `svc.shutdown()` after `server.join()`. Keep the existing `INFIGRAPH_BACKEND=kuzu` pin for the in-test `Infigraph`.
  - `wrapper_write_calls_service_edges_cleans_up_arrow_sibling` and `wrapper_ingest_structured_data_inline_cleans_up_sibling` look in `.infigraph/write-tmp/` instead of `.infigraph/requests/`, asserting it holds no files after the call.
  - `tests/daemon_protocol_e2e.rs`: the polling server thread becomes `spawn_one_write_server`, and the client calls `writes::submit(root, &request, WriteOpts { timeout: Duration::from_secs(30), cancel: None })`.
  - `tests/daemon_protocol_watcher_wiring.rs` and `tests/watch_control.rs`: replace `submit_write_request(&staging_dir, &request, T)` with `writes::submit(root, &request, WriteOpts { timeout: T, cancel: None })`. These run a real coordinator, so the socket path is exercised end to end. Keep `watch_loop_does_not_serve_requests_when_serve_requests_is_false`'s expectation of an error. It now comes from `NoDaemon`, because nothing binds the endpoint.

- [ ] **Step 2: Run to see it fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_kuzu_backend -- --test-threads=1`
Expected: FAIL. The backend still drops files, so the socket stub never receives the write and the call times out.

- [ ] **Step 3: Implement the backend helpers** (in `impl DaemonKuzuBackend`, replacing `staging_dir`):

```rust
    /// Send one write to the daemon. A `WriteResult::Err` becomes an error,
    /// so each method matches only the success variant it expects.
    fn write(&self, request: WriteRequest, timeout: Duration) -> Result<WriteResult> {
        match crate::daemon::writes::submit(&self.root, &request, WriteOpts { timeout, cancel: None })? {
            WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Ok(other),
        }
    }

    /// `write`, for a request whose payload rides in a sidecar file: `fill`
    /// writes it, and it is removed again if the write fails, since the
    /// daemon removes a sidecar only once it has consumed it.
    fn write_with_sidecar(
        &self,
        ext: &str,
        fill: impl FnOnce(&Path) -> Result<()>,
        request: impl FnOnce(PathBuf) -> WriteRequest,
        timeout: Duration,
    ) -> Result<WriteResult> {
        let path = crate::daemon::writes::sidecar_path(&self.root, ext);
        if let Err(e) = fill(&path) {
            std::fs::remove_file(&path).ok();
            return Err(e);
        }
        let result = self.write(request(path.clone()), timeout);
        if result.is_err() {
            std::fs::remove_file(&path).ok();
        }
        result
    }

    fn unexpected(kind: &str, other: WriteResult) -> anyhow::Error {
        anyhow::anyhow!("unexpected WriteResult for {kind}: {other:?}")
    }
```

Each method becomes one of three shapes. A plain request:

```rust
    fn upsert_similar_edge(&self, id_a: &str, id_b: &str, score: f32) -> Result<()> {
        let request = WriteRequest::UpsertSimilarEdge { id_a: id_a.to_string(), id_b: id_b.to_string(), score };
        match self.write(request, Duration::from_secs(30))? {
            WriteResult::Ok { .. } => Ok(()),
            other => Err(Self::unexpected("UpsertSimilarEdge", other)),
        }
    }
```

A sidecar request:

```rust
    fn upsert_files_bulk(&self, extractions: &[FileExtraction], existing_hashes_empty: bool) -> Result<()> {
        if extractions.is_empty() {
            return Ok(());
        }
        match self.write_with_sidecar(
            "extractions.json",
            |p| crate::daemon_protocol::write_extractions_json(p, extractions),
            |extractions_path| WriteRequest::UpsertFilesBulk { extractions_path, existing_hashes_empty },
            Self::BULK_WRITE_TIMEOUT,
        )? {
            WriteResult::Ok { .. } => Ok(()),
            other => Err(Self::unexpected("UpsertFilesBulk", other)),
        }
    }
```

A value-returning request:

```rust
    fn derive_tested_by_edges(&self, changed_files: Option<&[&str]>) -> Result<usize> {
        let request = WriteRequest::DeriveTestedBy {
            files: changed_files.map(|f| f.iter().map(|s| s.to_string()).collect()),
        };
        match self.write(request, Duration::from_secs(60))? {
            WriteResult::Ok { indexed_files, .. } => Ok(indexed_files),
            other => Err(Self::unexpected("DeriveTestedBy", other)),
        }
    }
```

Apply them to every method. Request fields are built exactly as today; only the plumbing changes:

| Method | Request | Sidecar (ext, writer) | Success → return | Timeout |
|---|---|---|---|---|
| `upsert_similar_edge` | `UpsertSimilarEdge` | — | `Ok{..}` → `()` | 30s |
| `upsert_files_bulk` | `UpsertFilesBulk` | `extractions.json`, `write_extractions_json` | `Ok{..}` → `()` | `BULK_WRITE_TIMEOUT` |
| `remove_file` | `RemoveFiles{files: vec![file]}` | — | `Ok{..}` → `()` | 60s |
| `derive_tested_by_edges` | `DeriveTestedBy` | — | `Ok{indexed_files,..}` → `indexed_files` | 60s |
| `upsert_repo` | `UpsertRepo` | — | `Ok{..}` → `()` | 30s |
| `write_calls_service_edges` | `WriteCallsServiceEdges` | `edges.arrow`, `write_calls_service_edges_arrow` | `Ok{..}` → `()` | 60s |
| `resolve_calls` | `ResolveCalls` | `extractions.json`, `write_extractions_json` | `ResolveOk(s)` → `s` | `BULK_WRITE_TIMEOUT` |
| `import_scip_index_enriched_at` | `ScipImport` | — | `ScipImportOk(s)` → `s` | 120s |
| `ingest_structured_data` | `IngestStructured{source: Inline(path)}` | `data.json`, `write_ingest_data` | `Ok{total_files, indexed_files}` → `IngestResult` (as today) | 120s |
| `ingest_structured_file` | `IngestStructured{source: File}` | — | same as above | 120s |
| `ingest_structured_directory` | `IngestStructured{source: Directory}` | — | same as above | 120s |
| `upsert_dependencies` | `UpsertDependencies` | — | `Ok{..}` → `()` | 30s |
| `replace_taint_flows` | `ReplaceTaintFlows` | — | `Ok{..}` → `()` | 30s |
| `replace_concerns` | `ReplaceConcerns` | — | `Ok{..}` → `()` | 30s |
| `replace_resolves_to` | `ReplaceResolvesTo` | — | `Ok{..}` → `()` | 30s |
| `store_clusters` | `StoreClusters` | — | `ClustersOk(s)` → `s` | 30s |
| `store_config_bindings` | `StoreConfigBindings` | — | `Ok{..}` → `()` | 30s |
| `write_cross_service_edges` | `WriteCrossServiceEdges` | `edges.arrow`, `write_cross_service_edges_arrow` | `Ok{indexed_files,..}` → `indexed_files` | 60s |

Keep each method's existing early returns (for example `if extractions.is_empty()`) and doc comments. Update the struct's doc comment: tier 2 now reads "route through `daemon::writes::submit` over the read socket (#204)", and the opening line reads "Routes writes to the daemon over its read socket".

`lib.rs::index_via_daemon`:

```rust
        let request = crate::daemon_protocol::WriteRequest::Index { paths };
        let opts = crate::daemon::writes::WriteOpts { timeout, cancel: None };
        match crate::daemon::writes::submit(&self.root, &request, opts)? {
```

The arms are unchanged. In `cmd_index`, delete `staging_dir` and call:

```rust
            let result = infigraph_core::daemon::writes::submit(
                root,
                &infigraph_core::daemon_protocol::WriteRequest::FullReindex,
                infigraph_core::daemon::writes::WriteOpts {
                    timeout: std::time::Duration::from_secs(600),
                    cancel: None,
                },
            )?;
```

- [ ] **Step 4: Run to see it pass**

Run:
- `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_kuzu_backend --test daemon_protocol_e2e --test daemon_protocol_watcher_wiring --test watch_control -- --test-threads=1`
- `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_kuzu_e2e -- --test-threads=1`
- `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli -- --test-threads=1`

Expected: all PASS. `daemon_kuzu_e2e`'s producers-while-draining test still asserts "nothing orphaned". Task 9 rewrites that assertion.

- [ ] **Step 5: Commit**

```bash
git add -A crates/
git commit --no-verify -m "feat(daemon): every routed write goes over the socket (#204)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```

---

### Task 9: Delete file-drop; legacy refusal and sidecar sweep

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs`. Delete:
  - `REQUEST_COUNTER`, `generate_request_name`
  - `submit_write_request`, `submit_write_request_named`, `submit_write_request_cancellable`, `submit_write_request_named_cancellable`
  - `request_client_is_gone`, `discard_request`, `serve_one_request`
  - their tests (L430-560, L700-830)
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (the pickup block and `file_replies` bridge from Task 4; add the legacy sweep and startup sidecar sweep)
- Create: `crates/infigraph-core/src/scratch.rs`
- Modify: `crates/infigraph-core/src/lib.rs` (`pub mod scratch;`)
- Modify: `crates/infigraph-cli/src/index.rs` (`sweep_stale_scip_scratch` L1005-1029 delegates)
- Test: `crates/infigraph-core/tests/daemon_control.rs` (the legacy test, generalized), `crates/infigraph-core/tests/daemon_kuzu_e2e.rs` (L747-752, L1140), `crates/infigraph-core/tests/watch_daemon.rs` (the three file-fixture tests from Task 4)

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `pub fn scratch::sweep_older_than(dir: &Path, age: Duration, extensions: &[&str]) -> usize`
  - `fn refuse_legacy_requests(infigraph_dir: &Path) -> usize`
  - `const LEGACY_SWEEP_INTERVAL: Duration = Duration::from_secs(2);`
  - `const SIDECAR_STALE_AFTER: Duration = Duration::from_secs(6 * 3600);`

- [ ] **Step 1: Write the failing tests.**

`scratch.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_old_files_with_a_listed_extension_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("a.scip");
        let young = dir.path().join("b.scip");
        let other = dir.path().join("c.txt");
        for p in [&old, &young, &other] {
            std::fs::write(p, b"x").unwrap();
        }
        let past = std::time::SystemTime::now() - Duration::from_secs(7 * 3600);
        for p in [&old, &other] {
            std::fs::File::options().write(true).open(p).unwrap().set_modified(past).unwrap();
        }
        assert_eq!(sweep_older_than(dir.path(), Duration::from_secs(6 * 3600), &["scip"]), 1);
        assert!(!old.exists() && young.exists() && other.exists());
        assert_eq!(sweep_older_than(&dir.path().join("missing"), Duration::ZERO, &["scip"]), 0);
    }
}
```

In `tests/daemon_control.rs`, generalize the legacy test into two. Keep the `WatchControl` one and add a data-write one, both asserting the new message:

```rust
/// A pre-#204 client drops any write as a file. The daemon answers it within
/// one sweep with an error naming its build, and removes it.
#[test]
fn a_legacy_write_request_file_gets_a_prompt_error_naming_the_build() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    let requests = dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&requests).unwrap();
    infigraph_core::daemon_protocol::write_atomic(&requests.join("1-2-3.request"), r#""FullReindex""#).unwrap();
    let result = requests.join("1-2-3.result");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !result.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let reply = std::fs::read_to_string(&result).expect("a reply within one sweep");
    assert!(reply.contains("no longer accepts file-drop requests"), "{reply}");
    assert!(reply.contains(infigraph_core::build_hash()), "{reply}");
    assert!(!requests.join("1-2-3.request").exists());
    stop(d);
}
```

In the existing `a_legacy_watch_control_request_file_gets_a_prompt_error`, add `assert!(reply.contains("no longer accepts file-drop requests"), "{reply}");`.

In `tests/daemon_kuzu_e2e.rs`:
- `producers_keep_accepting_work_while_a_drain_is_in_flight` (~L747): replace the "no leftover `.request`" check with `assert!(!project.path().join(".infigraph").join("requests").exists(), "writes never touch requests/")`;
- the `full_reindex_with_no_daemon…` check at ~L1140 keeps its assertion. It already expects no `requests/`.

In `tests/watch_daemon.rs`, move the three Task-4 file fixtures onto `writes::submit`:
- `out_of_scope_write_request_contends_with_a_held_index_lock`: submit `UpsertRepo` from a thread while the test holds `index.lock`; assert it has not returned after 1s; release the lock; assert `Ok` within 10s.
- `full_reindex_build_task_can_be_cancelled_before_it_starts_the_swap` and `scip_enrichment_task_is_cancellable_via_daemon_token`: submit `FullReindex` with `writes::submit` from a thread in place of writing the request file. Their cancellation assertions are unchanged, and the client thread's result is not asserted: it is either `Lost` or a shutting-down `Err`.

- [ ] **Step 2: Run to see it fail**

Run:
- `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib scratch`
- `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_control -- --test-threads=1`

Expected: FAIL (`scratch` missing; the reply is still "failed to read/parse request" from the Task 4 bridge, not the build message).

- [ ] **Step 3: Implement.**

`scratch.rs`:

```rust
//! Age-based cleanup for run-unique scratch files (#139, #204): a producer
//! removes its own files, and this is the backstop for one that crashed.

use std::path::Path;
use std::time::Duration;

/// Remove files in `dir` older than `age` whose extension is one of
/// `extensions`. Anything younger belongs, or may belong, to a run still in
/// progress. Returns how many were removed; a missing `dir` is zero.
pub fn sweep_older_than(dir: &Path, age: Duration, extensions: &[&str]) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let listed = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.contains(&e));
        let stale = listed
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|a| a > age);
        if stale && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}
```

`index.rs::sweep_stale_scip_scratch` body becomes `infigraph_core::scratch::sweep_older_than(scip_tmp, SCIP_SCRATCH_STALE_AFTER, &["scip", "partial"]);`. Keep its doc comment and the `.partial` comment.

`mod.rs`:
- delete the Task 4 bridge: the `file_replies` declaration, the `read_dir` block, the `retain` and the post-loop flush;
- delete the `requests_dir` local;
- add:

```rust
/// How often the daemon answers file-drop requests from pre-#204 clients.
/// Well under the 30s those clients wait for control and the minutes they
/// wait for writes, so they fail fast instead of timing out.
const LEGACY_SWEEP_INTERVAL: Duration = Duration::from_secs(2);

/// Age past which a write sidecar is assumed orphaned by a client that died
/// before the daemon read it. Matches the SCIP scratch backstop.
const SIDECAR_STALE_AFTER: Duration = Duration::from_secs(6 * 3600);

/// Answer every file-drop request a pre-#204 client left in `requests/` with
/// an error naming this build, and remove it (#204 D5). Remove one release
/// after #204 ships: by then no such client remains.
fn refuse_legacy_requests(infigraph_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(infigraph_dir.join("requests")) else {
        return 0;
    };
    let result = WriteResult::Err {
        message: format!(
            "this daemon (build {}) no longer accepts file-drop requests; restart the client \
             (MCP: /mcp reconnect)",
            crate::build_hash()
        ),
    };
    let json = serde_json::to_string(&result).expect("WriteResult::Err always serializes");
    let mut refused = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "request") {
            let _ = crate::daemon_protocol::write_atomic(&path.with_extension("result"), &json);
            std::fs::remove_file(&path).ok();
            refused += 1;
        }
    }
    refused
}
```

In the loop, inside `if serve_requests`, where the `read_dir` block was:

```rust
            if last_legacy_sweep.elapsed() >= LEGACY_SWEEP_INTERVAL {
                last_legacy_sweep = std::time::Instant::now();
                let refused = refuse_legacy_requests(&infigraph_dir);
                if refused > 0 {
                    eprintln!("[daemon] refused {refused} file-drop request(s) from a pre-#204 client");
                }
            }
```

Declare `let mut last_legacy_sweep = std::time::Instant::now() - LEGACY_SWEEP_INTERVAL;` before the loop, so the first tick sweeps. Where the read service is first bound (the `serve_requests` branch that also sweeps orphaned endpoints), add:

```rust
            let swept = crate::scratch::sweep_older_than(
                &root.join(".infigraph").join(crate::daemon::writes::SIDECAR_DIR),
                SIDECAR_STALE_AFTER,
                &["json", "arrow"],
            );
            if swept > 0 {
                eprintln!("[daemon] swept {swept} write sidecar(s) left by clients that are gone");
            }
```

`daemon_protocol.rs`: delete the functions listed under Files and their tests. Also delete `submit_write_request_named_cancellable`'s call into `writes::blocking_fault` from Task 7, and make `blocking_fault` private again. Update the module doc's first paragraph to: "Write request and result types (#204: they travel on the daemon's read socket; see `daemon::writes`), the daemon's `serve_write`, and the sidecar codecs for bulk payloads." `write_atomic` stays: `refuse_legacy_requests`, `write_ingest_data` and other modules use it.

Then remove any remaining file-drop reference. Run `mcp__infigraph__search` with `regex=true` for `submit_write_request|generate_request_name|request_client_is_gone|discard_request|serve_one_request|join\("requests"\)`. The only hits left should be `refuse_legacy_requests`, the legacy tests, and `docs/`.

- [ ] **Step 4: Run to see it pass**

Run:
- `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib`
- `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --tests -- --test-threads=1`
- `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli -- --test-threads=1`

Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add -A crates/
git commit --no-verify -m "feat(daemon): delete file-drop writes; refuse legacy clients, sweep sidecars (#204)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```

---

### Task 10: End-to-end checks, docs, full verification

**Files:**
- Create: `crates/infigraph-core/tests/daemon_socket_writes.rs`
- Modify: `CLAUDE.md` (the "Cross-cutting invariants" lease bullet and the #203 note)
- Modify: `crates/infigraph-cli/resources/integrations/**` only if a search finds a `requests/` mention there. Check with `mcp__infigraph__search` for `\.infigraph/requests` in `*.md` under `crates/`.

**Interfaces:**
- Consumes: the whole stack, against a real coordinator (`run_write_coordinator` with `serve_requests = true`, started as `tests/daemon_control.rs::start` does).

- [ ] **Step 1: Write the end-to-end tests** (`tests/daemon_socket_writes.rs`). Reuse `tests/daemon_control.rs`'s `start`/`stop` helpers: copy them into a shared `tests/common/daemon.rs` module and `mod common;` from both files, so they are not duplicated.

```rust
mod common;
use common::daemon::{start, stop, ENV_LOCK};
use infigraph_core::daemon::writes::{submit, WriteOpts};
use infigraph_core::daemon_protocol::{WriteRequest, WriteResult};
use std::time::{Duration, Instant};

fn opts() -> WriteOpts<'static> {
    WriteOpts { timeout: Duration::from_secs(120), cancel: None }
}

/// Success criterion 1: on an idle daemon a write no longer waits for a
/// 200ms tick.
#[test]
fn an_idle_daemon_answers_a_write_within_one_tick() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "def f():\n    pass\n").unwrap();
    let d = start(dir.path());
    submit(dir.path(), &WriteRequest::Index { paths: None }, opts()).unwrap(); // warm: graph open
    let mut samples: Vec<Duration> = (0..5)
        .map(|_| {
            let t = Instant::now();
            let r = submit(dir.path(), &WriteRequest::UpsertRepo { namespace: "n".into() }, opts()).unwrap();
            assert!(matches!(r, WriteResult::Ok { .. }), "{r:?}");
            t.elapsed()
        })
        .collect();
    samples.sort();
    assert!(samples[2] < Duration::from_millis(200), "median {:?} of {samples:?}", samples[2]);
    stop(d);
}

/// Both concurrent clients are answered by a rebuild. That it is *one*
/// rebuild is pinned at the unit level (Task 4's
/// `a_full_reindex_requested_while_one_runs_joins_it_instead_of_queuing_another`);
/// this pins that joining works through the socket.
#[test]
fn concurrent_rebuilds_are_all_answered() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "def f():\n    pass\n").unwrap();
    let d = start(dir.path());
    let clients: Vec<_> = (0..2)
        .map(|_| {
            let root = dir.path().to_path_buf();
            std::thread::spawn(move || submit(&root, &WriteRequest::FullReindex, opts()))
        })
        .collect();
    for c in clients {
        assert!(matches!(c.join().unwrap().unwrap(), WriteResult::FullReindexOk { .. }));
    }
    stop(d);
}

/// D3 end to end: a rebuild deferred behind a stalled coordinator, whose
/// client is killed, never runs. A rebuild swaps a new graph file in, so an
/// unchanged inode proves it did not happen.
#[cfg(unix)]
#[test]
fn a_killed_clients_deferred_rebuild_never_runs() {
    use std::os::unix::fs::MetadataExt as _;
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    // Read once at coordinator start; the loop then stalls while the file exists.
    let stall = dir.path().join("stall");
    std::env::set_var("INFIGRAPH_TEST_COORDINATOR_STALL_FILE", &stall);
    let d = start(dir.path());
    std::env::remove_var("INFIGRAPH_TEST_COORDINATOR_STALL_FILE");
    submit(dir.path(), &WriteRequest::Index { paths: None }, opts()).unwrap(); // the graph exists
    let graph = dir.path().join(".infigraph").join("graph");
    let inode_before = std::fs::metadata(&graph).unwrap().ino();
    std::fs::write(&stall, b"").unwrap();
    // A client in a child process, so killing it closes its socket for real.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "submit_full_reindex_child", "--ignored", "--nocapture"])
        .env("INFIGRAPH_TEST_WRITE_ROOT", dir.path())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    child.kill().unwrap();
    let _ = child.wait();
    std::fs::remove_file(&stall).unwrap();
    // Long enough for a started rebuild of a one-file project to swap.
    std::thread::sleep(Duration::from_secs(5));
    let status = infigraph_core::daemon::control::query_status(dir.path()).unwrap();
    assert!(!status.work_in_flight, "nothing deferred or running");
    assert_eq!(std::fs::metadata(&graph).unwrap().ino(), inode_before, "no rebuild swapped in");
    stop(d);
}

/// Child half of the test above: submits a FullReindex and waits to be killed.
#[test]
#[ignore]
fn submit_full_reindex_child() {
    let Some(root) = std::env::var_os("INFIGRAPH_TEST_WRITE_ROOT") else { return };
    let _ = submit(std::path::Path::new(&root), &WriteRequest::FullReindex, opts());
}
```

`start` runs the coordinator in-process (see `tests/daemon_control.rs`), so its log lines go to the test's stderr and are not asserted on. The tests check state a client can observe: replies, `StatusReport`, and the graph file's identity. Before relying on the inode check, confirm that `finish_full_reindex` swaps in a new file by rename rather than rewriting in place: `mcp__infigraph__get_code_snippet` on `crates/infigraph-core/src/daemon/mod.rs::finish_full_reindex`, and look for the `graph.rebuilding` → `graph` rename. If the graph is a directory in this layout, compare the directory's inode instead. The assertion stays the same.

- [ ] **Step 2: Run to see it pass.** These tests exercise behavior Tasks 4–9 already built. If one fails, the defect is in that task's code, and the fix belongs there.

Run: `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_socket_writes -- --test-threads=1`
Expected: PASS.

- [ ] **Step 3: Update `CLAUDE.md`.**
- In the lease bullet, change "which `RemoteExec::query_rows` and routed write submission take" to "which `RemoteExec::query_rows` and `daemon::writes::submit` take".
- After that bullet's control/status sentence, add: "Data writes travel on the same socket too (#204): `daemon::writes::submit` is the only client, a write is acked on admission, and a client that closes its connection has withdrawn it. Bulk payloads ride in `.infigraph/write-tmp/` sidecars. There is no `requests/` directory; a daemon answers any file a pre-#204 client drops there with an error naming its build."

- [ ] **Step 4: Full verification** (the disk-constrained workflow: per crate, pruning between if space is low; check `df -h .` first):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build -p infigraph-cli -p infigraph-mcp
env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core -- --test-threads=1
env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli -- --test-threads=1
env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-mcp -- --test-threads=1
env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs -- --test-threads=1
```

Expected: all green. A failure that reproduces at the merge-base in a throwaway worktree is pre-existing. Record it in the task report; don't fix it here.

- [ ] **Step 5: Commit, with the hook**

```bash
git add -A
git commit -m "test(daemon): socket writes end to end; document the one transport (#204)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```

If the hook's perf gates fail, follow CLAUDE.md's "CI / toolchain gotchas" guidance for each gate before treating it as a regression.
