# Daemon Lease Connections and Idle Exit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A detached `infigraph daemon` counts the client processes holding a lease on its read socket, and exits cleanly after a grace period once none are held, nothing has touched it, and no work is in flight.

**Architecture:** Clients send an `Attach` first frame on a connection they hold for their process lifetime. The read service parks that connection on a dedicated thread, outside the read pool, and counts it in a coordinator-owned `Liveness`. The coordinator loop checks a pure `idle_exit_due` on a coarse interval and leaves through its existing clean-shutdown `break`. The client side is a process-wide, idempotent `lease::hold(root)`, called from the two lifecycle entry points every client already passes through.

**Tech Stack:** Rust; `interprocess` local sockets (existing `ReadEndpoint`); `serde` untagged enum; the crate's `settings!` macro.

**Spec:** `docs/superpowers/specs/2026-09-25-daemon-lease-idle-exit-design.md`

## Global Constraints

- The settings group is exactly `daemon_idle { grace_secs: u64 = 1800, check_secs: u64 = 60 }`, which gives the env vars `INFIGRAPH_DAEMON_IDLE_GRACE_SECS` and `INFIGRAPH_DAEMON_IDLE_CHECK_SECS`. `grace_secs = 0` disables idle exit.
- `ReadRequest`'s wire shape must not change. Old clients must parse as `ClientFrame::Read`.
- Held leases never occupy a read-pool worker (`READ_SERVICE_WORKERS`).
- `lease::hold` never blocks its caller, never returns an error and never panics.
- A daemon never holds a lease on its own root.
- The idle check runs only when `serve_requests == true`, so in-process `watch_project` is untouched.
- Every cargo command runs with `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu`. Test one crate at a time, never `cargo test --all` in one go on this machine. Confirm a suspected flake with `--test-threads=1`.
- Before any `infigraph-cli` or `infigraph-mcp` integration test, run `cargo build -p infigraph-cli` (the tests spawn that binary).
- Commits end with:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and
  `Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV`

## Review Focus

1. **Daemon restart while a client holds a lease** (`infigraph daemon-restart`, or a build-mismatch respawn). The client's lease thread must attach to the successor, so an idle MCP session keeps the new daemon alive. Pinned in Task 4 (`hold_reattaches_to_a_successor_service`).
2. **More leases than read workers.** Reads must still be answered with `READ_SERVICE_WORKERS + 2` leases held. Pinned in Task 3.
3. **A lease holder killed with SIGKILL.** The daemon must see EOF and release the lease. Pinned in Task 6 (`a_killed_lease_holder_releases_its_lease`).
4. **An idle deadline reached while a drain or reindex is running.** The daemon must not exit mid-write. Pinned in Task 1 (`in_flight_work_blocks_exit`) and wired in Task 6.
5. **A read-socket rebind (#187) while leases are open.** Dropping the old service closes its parked leases, and the clients re-attach to the new one. Because `Liveness` belongs to the coordinator and is handed to both services, the count is back to the right number within the startup grace. Pinned in Task 4 (`hold_reattaches_to_a_successor_service`, which uses one shared `Liveness` just as the coordinator does).

---

### File structure

| File | Responsibility |
|---|---|
| `crates/infigraph-core/src/daemon/liveness.rs` (new) | `Liveness` counters plus the pure `idle_exit_due` decision |
| `crates/infigraph-core/src/daemon/read_protocol.rs` | `ClientFrame` / `Attach` first-frame codec |
| `crates/infigraph-core/src/daemon/read_service.rs` | Dispatch `Attach` to a lease thread, `touch()` on reads, `start_serving` taking a `Liveness` |
| `crates/infigraph-core/src/daemon/read_endpoint.rs` | `connect_allowing_for_startup(root)` free function, moved from `RemoteExec` |
| `crates/infigraph-core/src/daemon/lease.rs` (new) | Client `hold`, `is_held`, `mark_self_daemon` |
| `crates/infigraph-core/src/daemon/lifecycle.rs` | Call `lease::hold` at the two entry points |
| `crates/infigraph-core/src/daemon/mod.rs` | `daemon_idle` settings, coordinator wiring and idle exit |
| `crates/infigraph-cli/src/info_commands.rs` | `cmd_daemon` marks its own root |
| `crates/infigraph-cli/tests/daemon_idle_exit.rs` (new) | Real-daemon idle, lease and SIGKILL tests |

---

### Task 1: `Liveness` and `idle_exit_due`

**Files:**
- Create: `crates/infigraph-core/src/daemon/liveness.rs`
- Modify: `crates/infigraph-core/src/daemon/mod.rs` (add `pub mod liveness;` next to the other `pub mod` lines)

**Interfaces:**
- Produces: `pub struct Liveness`; `Liveness::new() -> Self` (stamps now); `lease_opened(&self)`; `lease_closed(&self)` (also stamps); `touch(&self)`; `leases(&self) -> usize`; `idle_for(&self, now_secs: u64) -> Option<Duration>` (`None` while any lease is held); `pub fn now_secs() -> u64`; `pub fn idle_exit_due(idle_for: Option<Duration>, grace: Duration, work_in_flight: bool) -> bool`.

- [ ] **Step 1: Write the failing tests** at the bottom of `liveness.rs`, with the module skeleton declaring only the signatures and bodies `todo!()`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const G: Duration = Duration::from_secs(10);

    #[test]
    fn exit_is_due_at_the_grace_boundary_inclusive() {
        assert!(!idle_exit_due(Some(Duration::from_secs(9)), G, false));
        assert!(idle_exit_due(Some(G), G, false));
    }

    #[test]
    fn a_held_lease_blocks_exit() {
        assert!(!idle_exit_due(None, G, false));
    }

    #[test]
    fn in_flight_work_blocks_exit() {
        assert!(!idle_exit_due(Some(Duration::from_secs(3600)), G, true));
    }

    #[test]
    fn zero_grace_disables_idle_exit() {
        assert!(!idle_exit_due(Some(Duration::from_secs(3600)), Duration::ZERO, false));
    }

    #[test]
    fn leases_count_and_closing_one_stamps_activity() {
        let l = Liveness::new();
        let start = now_secs();
        l.lease_opened();
        l.lease_opened();
        assert_eq!(l.leases(), 2);
        assert_eq!(l.idle_for(start + 100), None, "held leases mean not idle");
        l.lease_closed();
        l.lease_closed();
        assert_eq!(l.leases(), 0);
        let idle = l.idle_for(now_secs() + 5).unwrap();
        assert!(idle >= Duration::from_secs(5) && idle < Duration::from_secs(7));
    }

    #[test]
    fn touch_resets_idle_time() {
        let l = Liveness::new();
        l.last_activity.store(now_secs() - 100, std::sync::atomic::Ordering::Relaxed);
        assert!(l.idle_for(now_secs()).unwrap() >= Duration::from_secs(100));
        l.touch();
        assert!(l.idle_for(now_secs()).unwrap() < Duration::from_secs(2));
    }
}
```

- [ ] **Step 2: Run and see the tests fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib daemon::liveness`
Expected: FAIL (panics at `todo!()`).

- [ ] **Step 3: Implement**

```rust
//! How much a daemon is still needed (#38, #124): client processes holding a
//! lease on its read socket, and when anything last touched it. Owned by the
//! coordinator for the daemon's whole run and shared with the read service,
//! never created inside the service -- the service is rebuilt when its socket
//! is rebound (#187), and a count living there would reset under open leases.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct Liveness {
    leases: AtomicUsize,
    pub(crate) last_activity: AtomicU64,
}

impl Default for Liveness {
    fn default() -> Self {
        Self::new()
    }
}

impl Liveness {
    /// Starting counts as activity, so a fresh daemon gets a full grace.
    pub fn new() -> Self {
        Self {
            leases: AtomicUsize::new(0),
            last_activity: AtomicU64::new(now_secs()),
        }
    }

    pub fn lease_opened(&self) {
        self.leases.fetch_add(1, Ordering::SeqCst);
    }

    /// Stamps activity too: the grace runs from the moment the last client
    /// left, not from its last query.
    pub fn lease_closed(&self) {
        self.touch();
        self.leases.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn touch(&self) {
        self.last_activity.store(now_secs(), Ordering::SeqCst);
    }

    pub fn leases(&self) -> usize {
        self.leases.load(Ordering::SeqCst)
    }

    /// `None` while any lease is held -- a leased daemon is never idle.
    pub fn idle_for(&self, now_secs: u64) -> Option<Duration> {
        if self.leases() > 0 {
            return None;
        }
        let last = self.last_activity.load(Ordering::SeqCst);
        Some(Duration::from_secs(now_secs.saturating_sub(last)))
    }
}

/// Whether the daemon should exit for idleness now. `grace == 0` disables
/// the feature; in-flight work always wins, so an exit never cuts a write.
pub fn idle_exit_due(idle_for: Option<Duration>, grace: Duration, work_in_flight: bool) -> bool {
    if grace.is_zero() || work_in_flight {
        return false;
    }
    idle_for.is_some_and(|idle| idle >= grace)
}
```

- [ ] **Step 4: Run and see the tests pass.** Same command. Expected: 6 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/daemon/liveness.rs crates/infigraph-core/src/daemon/mod.rs
git commit -m "feat(core): a daemon Liveness tracks leases and idle time (#38)"
```

---

### Task 2: `ClientFrame` first-frame protocol

**Files:**
- Modify: `crates/infigraph-core/src/daemon/read_protocol.rs`

**Interfaces:**
- Produces: `pub struct Attach { pub attach_pid: u32 }`; `#[serde(untagged)] pub enum ClientFrame { Attach(Attach), Read(ReadRequest) }`; `pub fn write_attach<W: Write>(w: &mut W, pid: u32) -> Result<()>`; `pub fn read_client_frame<R: Read>(r: &mut R) -> Result<ClientFrame>`; `pub(crate) fn read_len_prefixed` becomes visible in the crate (the lease thread uses it to wait for EOF). `read_request` is removed; its only non-test caller is `serve_one`, which Task 3 changes. `write_request` is kept unchanged.

- [ ] **Step 1: Write the failing tests** in the existing `mod tests`, replacing `a_request_round_trips`'s `read_request` call:

```rust
    #[test]
    fn a_request_round_trips() {
        let req = ReadRequest {
            store: Store::Graph,
            query: "MATCH (f:File) RETURN f.id".to_string(),
            params: vec![],
            chunk_size: 1024,
        };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let ClientFrame::Read(got) = read_client_frame(&mut buf.as_slice()).unwrap() else {
            panic!("a ReadRequest must parse as ClientFrame::Read");
        };
        assert_eq!(got.query, req.query);
        assert_eq!(got.store, Store::Graph);
    }

    #[test]
    fn an_attach_round_trips() {
        let mut buf = Vec::new();
        write_attach(&mut buf, 4242).unwrap();
        let ClientFrame::Attach(a) = read_client_frame(&mut buf.as_slice()).unwrap() else {
            panic!("expected Attach");
        };
        assert_eq!(a.attach_pid, 4242);
    }

    /// Wire compatibility: an old client's request JSON, written by hand, must
    /// still parse as a read. If this breaks, every pre-lease client breaks.
    #[test]
    fn an_old_client_request_still_parses_as_a_read() {
        let json = br#"{"store":"Graph","query":"RETURN 1","params":[],"chunk_size":8}"#;
        let mut buf = (json.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(json);
        assert!(matches!(
            read_client_frame(&mut buf.as_slice()).unwrap(),
            ClientFrame::Read(_)
        ));
    }
```

- [ ] **Step 2: Run and see the tests fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib daemon::read_protocol`
Expected: compile error, because `ClientFrame`, `write_attach` and `read_client_frame` are not defined.

- [ ] **Step 3: Implement.** Add these after `ReadRequest`, and replace `read_request` with `read_client_frame`:

```rust
/// A lease (#38, #124): sent as the first and only frame on a connection the
/// client holds for as long as it uses the daemon. The daemon answers nothing;
/// the connection exists so its EOF -- which the kernel delivers even when the
/// client is SIGKILLed -- tells the daemon a user went away.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attach {
    pub attach_pid: u32,
}

/// The first frame of every connection. `untagged` keeps a `ReadRequest`
/// byte-identical on the wire, so clients from before leases still parse;
/// `Attach`'s field is one no `ReadRequest` has, so the two never collide.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientFrame {
    Attach(Attach),
    Read(ReadRequest),
}

pub fn write_attach<W: Write>(w: &mut W, pid: u32) -> Result<()> {
    write_len_prefixed(w, &serde_json::to_vec(&Attach { attach_pid: pid })?)
}

pub fn read_client_frame<R: Read>(r: &mut R) -> Result<ClientFrame> {
    let body = read_len_prefixed(r)?.ok_or_else(|| anyhow::anyhow!("no request"))?;
    Ok(serde_json::from_slice(&body)?)
}
```

Change `fn read_len_prefixed` to `pub(crate) fn read_len_prefixed`. Delete `read_request`, then run `cargo check -p infigraph-core --all-targets` and update every remaining `read_request` caller: `serve_one` (done in Task 3; for now make it compile with `let ClientFrame::Read(req) = read_client_frame(&mut stream)? else { anyhow::bail!("unexpected attach") };`) and any test file the check names.

- [ ] **Step 4: Run and see the tests pass.** Same command. Expected: all `read_protocol` tests pass. Then `cargo check -p infigraph-core -p infigraph-docs --all-targets` compiles.

- [ ] **Step 5: Commit**

```bash
git add -A crates/infigraph-core
git commit -m "feat(core): the read socket's first frame can be a lease Attach (#38, #124)"
```

---

### Task 3: The read service parks leases and counts activity

**Files:**
- Modify: `crates/infigraph-core/src/daemon/read_service.rs`
- Test: `crates/infigraph-core/tests/read_service.rs`

**Interfaces:**
- Consumes: `Liveness` (Task 1); `ClientFrame`, `read_client_frame` and `read_len_prefixed` (Task 2).
- Produces: `ReadService::start_serving(root: &Path, source: StoreSource, docs: Option<RowSource>, workers: usize, liveness: Arc<Liveness>) -> Result<Self>`. `start_with_sources` keeps its signature and delegates with `Arc::new(Liveness::new())`, so the `infigraph-docs` tests are untouched.

- [ ] **Step 1: Write the failing tests** in `tests/read_service.rs`. The helper `indexed_project_and_source` is new and is defined here. It is the same one-`File`-node graph setup `a_client_reads_rows_over_the_socket` uses, wrapped as a `StoreSource`.

```rust
use infigraph_core::daemon::liveness::{self, Liveness};
use infigraph_core::daemon::read_protocol::write_attach;
use infigraph_core::daemon::read_service::{ReadService, StoreSource};

/// A temp project whose graph holds one `File` node, and a `StoreSource`
/// serving it -- the shape the daemon hands `start_serving`.
fn indexed_project_and_source() -> (tempfile::TempDir, StoreSource) {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    {
        let store = GraphStore::open(&graph).unwrap();
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();
    }
    let store = open_shared_store(&graph);
    let source: StoreSource = Arc::new(move || Some(store.clone()));
    (dir, source)
}

fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

fn attach(root: &Path) -> infigraph_core::daemon::read_endpoint::ReadStream {
    let mut s = ReadEndpoint::for_root(root).connect().unwrap();
    write_attach(&mut s, std::process::id()).unwrap();
    s
}

#[test]
fn a_lease_is_counted_while_held_and_released_on_drop() {
    let (project, source) = indexed_project_and_source(); // this file's existing setup, factored if needed
    let liveness = Arc::new(Liveness::new());
    let _svc = ReadService::start_serving(project.path(), source, None, 2, liveness.clone()).unwrap();
    let lease = attach(project.path());
    assert!(wait_for(|| liveness.leases() == 1), "attach must be counted");
    drop(lease);
    assert!(wait_for(|| liveness.leases() == 0), "EOF must release the lease");
}

/// Review Focus 2: leases must never occupy pool workers.
#[test]
fn more_leases_than_workers_do_not_starve_reads() {
    let (project, source) = indexed_project_and_source();
    let liveness = Arc::new(Liveness::new());
    let workers = 2;
    let _svc =
        ReadService::start_serving(project.path(), source, None, workers, liveness.clone()).unwrap();
    let leases: Vec<_> = (0..workers + 2).map(|_| attach(project.path())).collect();
    assert!(wait_for(|| liveness.leases() == workers + 2));
    let rows = client_query(project.path(), "MATCH (f:File) RETURN count(f)").unwrap();
    assert_eq!(rows.len(), 1, "a read must still be served with every worker's worth of leases held");
    drop(leases);
}

#[test]
fn a_read_touches_liveness() {
    let (project, source) = indexed_project_and_source();
    let liveness = Arc::new(Liveness::new());
    let _svc = ReadService::start_serving(project.path(), source, None, 2, liveness.clone()).unwrap();
    liveness.last_activity_for_test(liveness::now_secs() - 500);
    client_query(project.path(), "MATCH (f:File) RETURN count(f)").unwrap();
    assert!(liveness.idle_for(liveness::now_secs()).unwrap() < std::time::Duration::from_secs(5));
}

/// A service going away must end its parked leases, so each client sees EOF
/// and can follow the daemon to a successor. That matters for an in-process
/// service (tests, a #187 rebind); a real daemon's exit closes them anyway.
#[cfg(unix)]
#[test]
fn dropping_the_service_releases_its_leases() {
    let (project, source) = indexed_project_and_source();
    let liveness = Arc::new(Liveness::new());
    let svc = ReadService::start_serving(project.path(), source, None, 2, liveness.clone()).unwrap();
    let mut lease = attach(project.path());
    assert!(wait_for(|| liveness.leases() == 1));
    drop(svc);
    assert!(wait_for(|| liveness.leases() == 0), "shutdown must end parked leases");
    let mut buf = [0u8; 1];
    assert_eq!(
        std::io::Read::read(&mut lease, &mut buf).unwrap_or(0),
        0,
        "the client must see EOF"
    );
}
```

`last_activity` is `pub(crate)`, so integration tests need a hook. Add to `Liveness`, gated for tests and docs-hidden:

```rust
    #[doc(hidden)]
    pub fn last_activity_for_test(&self, secs: u64) {
        self.last_activity.store(secs, Ordering::SeqCst);
    }
```

- [ ] **Step 2: Run and see the tests fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test read_service`
Expected: compile error (`start_serving` is not defined).

- [ ] **Step 3: Implement.** In `read_service.rs`:

1. Rename the body of `start_with_sources` to `start_serving` with the extra `liveness: Arc<Liveness>` parameter, and make `start_with_sources` delegate:

```rust
    pub fn start_with_sources(
        root: &Path,
        source: StoreSource,
        docs: Option<RowSource>,
        workers: usize,
    ) -> Result<Self> {
        Self::start_serving(root, source, docs, workers, Arc::new(Liveness::new()))
    }
```

2. **Shutdown handle on `ReadStream`** (`read_endpoint.rs`). A parked lease blocks in `read`. To end it from another thread without closing an fd the lease thread still owns, shut the socket down; don't close it:

```rust
/// Ends a parked lease from another thread: `shutdown(2)` wakes the lease
/// thread's blocking read with EOF and delivers EOF to the client. It never
/// closes the fd -- the lease thread still owns it -- so callers must stop
/// using a handle before its stream is dropped (see `LeaseBook`).
#[cfg(unix)]
pub(crate) struct LeaseShutdown(std::os::fd::RawFd);

#[cfg(unix)]
impl LeaseShutdown {
    pub(crate) fn shutdown(&self) {
        // SAFETY: the fd is open -- `LeaseBook` removes this handle, under
        // its lock, before the owning stream is dropped.
        unsafe {
            libc::shutdown(self.0, libc::SHUT_RDWR);
        }
    }
}

impl ReadStream {
    /// `None` where there is no socket fd (Windows named pipes); a daemon's
    /// exit still closes those, and in-process shutdown there simply leaves
    /// the lease until its client goes away.
    #[cfg(unix)]
    pub(crate) fn lease_shutdown(&self) -> Option<LeaseShutdown> {
        use std::os::fd::{AsFd as _, AsRawFd as _};
        let interprocess::local_socket::Stream::UdSocket(s) = &self.inner;
        Some(LeaseShutdown(s.as_fd().as_raw_fd()))
    }
}
```

(Mirror the `Listener::UdSocket` destructuring `accept_timeout` already does. If `interprocess` names the stream variant differently, use whatever `accept_timeout`'s pattern implies.)

3. **The lease book** (`read_service.rs`). This is one per service, and it's what `serve_one` needs for an `Attach`:

```rust
/// This service's parked leases and the daemon-wide `Liveness` they count
/// into. Per service, so dropping a service ends exactly its own leases.
struct LeaseBook {
    liveness: Arc<Liveness>,
    #[cfg(unix)]
    open: Mutex<std::collections::HashMap<u64, super::read_endpoint::LeaseShutdown>>,
    next: std::sync::atomic::AtomicU64,
}

impl LeaseBook {
    /// End every lease still parked. Under the lock, so no lease thread can
    /// drop its stream (and free the fd) between lookup and shutdown.
    fn end_all(&self) {
        #[cfg(unix)]
        for handle in self.open.lock().unwrap_or_else(|e| e.into_inner()).values() {
            handle.shutdown();
        }
    }
}
```

`ReadService` gains a field `leases: Arc<LeaseBook>`. `start_serving` builds it from `liveness`, and `stop_and_join` calls `self.leases.end_all()` right after setting the stop flag.

4. **Dispatch.** `serve_one` takes the concrete stream: its only caller is the accept loop, which always has a `ReadStream`. Change its signature to `fn serve_one(source: &StoreSource, docs: Option<&RowSource>, leases: &Arc<LeaseBook>, mut stream: ReadStream) -> Result<()>` and pass `leases.clone()` from the pool job. Its head becomes:

```rust
    let req = match read_client_frame(&mut stream)? {
        ClientFrame::Read(req) => req,
        ClientFrame::Attach(Attach { attach_pid }) => {
            park_lease(leases.clone(), attach_pid, stream);
            return Ok(());
        }
    };
    leases.liveness.touch();
    // ... unchanged below ...
```

5. **Parking:**

```rust
/// Holds one lease until its client goes away, on its own thread so a held
/// lease never occupies a pool worker (a handful of idle sessions would
/// otherwise starve every read). Detached: it ends on EOF, on `end_all`, or
/// with the process.
fn park_lease(leases: Arc<LeaseBook>, pid: u32, mut stream: ReadStream) {
    let id = leases.next.fetch_add(1, Ordering::Relaxed);
    #[cfg(unix)]
    if let Some(handle) = stream.lease_shutdown() {
        leases.open.lock().unwrap_or_else(|e| e.into_inner()).insert(id, handle);
    }
    leases.liveness.lease_opened();
    eprintln!("[lease] attached pid {pid} ({} held)", leases.liveness.leases());
    let book = leases.clone();
    let spawned = std::thread::Builder::new()
        .name("infigraph-lease".into())
        .spawn(move || {
            // A lease client never writes after Attach: any frame, EOF or
            // error ends the lease.
            let _ = super::read_protocol::read_len_prefixed(&mut stream);
            #[cfg(unix)]
            book.open.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            drop(stream); // only after its shutdown handle is gone
            book.liveness.lease_closed();
            eprintln!("[lease] released pid {pid} ({} held)", book.liveness.leases());
        });
    if spawned.is_err() {
        #[cfg(unix)]
        leases.open.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        leases.liveness.lease_closed();
    }
}
```

The `stream` moved into the closure is dropped on the failure path when the closure itself is dropped, which happens after the handle is removed on that path too, since the removal runs first. Keep that order.

- [ ] **Step 4: Run and see the tests pass.** Same command, then the whole file: `cargo test -p infigraph-core --test read_service`. Expected: all pass. Then `cargo test -p infigraph-docs --test docs_reads_via_daemon` still passes.

- [ ] **Step 5: Commit**

```bash
git add -A crates/infigraph-core
git commit -m "feat(core): the read service parks leases off the pool and counts activity (#38, #124)"
```

---

### Task 4: Client `lease::hold`

**Files:**
- Create: `crates/infigraph-core/src/daemon/lease.rs` (add `pub mod lease;` in `daemon/mod.rs`)
- Modify: `crates/infigraph-core/src/daemon/read_endpoint.rs` (add `connect_allowing_for_startup`)
- Modify: `crates/infigraph-core/src/graph/remote_exec.rs` (use the moved function; delete the method)
- Test: `crates/infigraph-core/tests/read_service.rs` (lease client tests live beside the service tests they need)

**Interfaces:**
- Consumes: `write_attach`, `read_len_prefixed` (Task 2); `ReadService::start_serving` and `Liveness` (Task 3).
- Produces: `pub fn connect_allowing_for_startup(root: &Path) -> anyhow::Result<ReadStream>` in `read_endpoint`; `pub fn hold(root: &Path)`, `pub fn is_held(root: &Path) -> bool`, `pub fn mark_self_daemon(root: &Path)` in `lease`.

- [ ] **Step 1: Move `connect_allowing_for_startup`.** Cut `DAEMON_STARTUP_GRACE` and the body of `RemoteExec::connect_allowing_for_startup` into `read_endpoint.rs` as `pub const DAEMON_STARTUP_GRACE` and `pub fn connect_allowing_for_startup(root: &Path) -> anyhow::Result<ReadStream>`, replacing `self.root` with `root`. Keep the doc comments. In `remote_exec.rs`, `attempt` calls `crate::daemon::read_endpoint::connect_allowing_for_startup(&self.root)?`, and `query_rows` uses `crate::daemon::read_endpoint::DAEMON_STARTUP_GRACE`. Run `cargo test -p infigraph-core --test read_service remote_exec`. Expected: passes, since this is a pure move.

- [ ] **Step 2: Write the failing tests** in `tests/read_service.rs`:

```rust
use infigraph_core::daemon::lease;

#[test]
fn hold_attaches_once_and_is_idempotent() {
    let (project, source) = indexed_project_and_source();
    let liveness = Arc::new(Liveness::new());
    let _svc = ReadService::start_serving(project.path(), source, None, 2, liveness.clone()).unwrap();
    lease::hold(project.path());
    lease::hold(project.path());
    assert!(wait_for(|| liveness.leases() == 1));
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(liveness.leases(), 1, "a second hold must not open a second lease");
    assert!(lease::is_held(project.path()));
}

#[test]
fn hold_is_a_noop_for_the_process_own_daemon_root() {
    let (project, source) = indexed_project_and_source();
    let liveness = Arc::new(Liveness::new());
    let _svc = ReadService::start_serving(project.path(), source, None, 2, liveness.clone()).unwrap();
    lease::mark_self_daemon(project.path());
    lease::hold(project.path());
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(liveness.leases(), 0);
    assert!(!lease::is_held(project.path()));
}

#[test]
fn hold_with_no_daemon_returns_immediately_and_forgets_the_root() {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".infigraph")).unwrap();
    let t = std::time::Instant::now();
    lease::hold(project.path());
    assert!(t.elapsed() < std::time::Duration::from_millis(50), "hold must never block");
    assert!(wait_for(|| !lease::is_held(project.path())));
}

/// Review Focus 1: a restarted daemon is re-attached to without any new
/// `hold`. `daemon_is_alive` needs `watch.lock` held, so hold it here the way
/// a daemon does.
#[cfg(unix)]
#[test]
fn hold_reattaches_to_a_successor_service() {
    let (project, source) = indexed_project_and_source();
    let lock_path = project.path().join(".infigraph").join("watch.lock");
    let _lock = infigraph_core::lockfile::try_acquire(&lock_path, "test-daemon").unwrap().unwrap();
    // One Liveness across both services, exactly as the coordinator shares it
    // across a #187 rebind -- so this also pins Review Focus 5.
    let liveness = Arc::new(Liveness::new());
    let svc = ReadService::start_serving(project.path(), source.clone(), None, 2, liveness.clone()).unwrap();
    lease::hold(project.path());
    assert!(wait_for(|| liveness.leases() == 1));
    drop(svc); // ends its parked leases (Task 3), so the client sees EOF
    assert!(wait_for(|| liveness.leases() == 0));
    let _svc2 = ReadService::start_serving(project.path(), source, None, 2, liveness.clone()).unwrap();
    assert!(wait_for(|| liveness.leases() == 1), "the lease must follow the daemon across a restart");
}
```

These tests share process-global lease state (`HELD`, `SELF_DAEMON`), so each one uses its own tempdir root, which keeps the keys distinct. The re-attach is only as good as `watch.lock` continuity. If a stop releases the lock before the successor takes it, the lease thread gives up, and the client's next `hold` (every `init`) attaches again. That's acceptable: the grace is 30 minutes and a respawn is cheap.

- [ ] **Step 3: Implement `lease.rs`:**

```rust
//! The client half of #38/#124: one held connection per (process, project)
//! telling that project's daemon someone still needs it. Idempotent and
//! fire-and-forget -- a lease is an optimisation over respawning, never a
//! correctness requirement, so nothing here blocks, errors or panics.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static HELD: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
static SELF_DAEMON: Mutex<Option<PathBuf>> = Mutex::new(None);

fn key(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn with_held<T>(f: impl FnOnce(&mut HashSet<PathBuf>) -> T) -> T {
    let mut guard = HELD.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashSet::new))
}

/// Called by `cmd_daemon` before its coordinator starts: a daemon holding a
/// lease on itself would never idle out.
pub fn mark_self_daemon(root: &Path) {
    *SELF_DAEMON.lock().unwrap_or_else(|e| e.into_inner()) = Some(key(root));
}

pub fn is_held(root: &Path) -> bool {
    with_held(|h| h.contains(&key(root)))
}

pub fn hold(root: &Path) {
    let root = key(root);
    if SELF_DAEMON.lock().unwrap_or_else(|e| e.into_inner()).as_ref() == Some(&root) {
        return;
    }
    if !with_held(|h| h.insert(root.clone())) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("infigraph-lease-hold".into())
        .spawn({
            let root = root.clone();
            move || {
                hold_until_no_daemon(&root);
                with_held(|h| h.remove(&root));
            }
        });
    if spawned.is_err() {
        with_held(|h| h.remove(&root));
    }
}

/// Attach, wait for the daemon to go away, and attach again to a successor
/// while `watch.lock` says there is one (a `daemon-restart`, a build-mismatch
/// respawn) -- so a session that never queries keeps its lease across
/// restarts. Returns once no daemon is left to lease from.
fn hold_until_no_daemon(root: &Path) {
    let lock = root.join(".infigraph").join("watch.lock");
    loop {
        let Ok(mut stream) = super::read_endpoint::connect_allowing_for_startup(root) else {
            return;
        };
        if super::read_protocol::write_attach(&mut stream, std::process::id()).is_err() {
            return;
        }
        // Blocks until the daemon closes the connection.
        let _ = super::read_protocol::read_len_prefixed(&mut stream);
        if !super::lifecycle::wait_for_daemon_ready(&lock, super::read_endpoint::DAEMON_STARTUP_GRACE) {
            return;
        }
    }
}
```

`hold_reattaches_to_a_successor_service` depends on Task 3's `LeaseBook::end_all`: dropping a service is what ends its parked leases. `hold_reattaches` is also a Unix-only behavior for in-process services, so mark it `#[cfg(unix)]`.

- [ ] **Step 4: Run and see the tests pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test read_service -- --test-threads=1`
Expected: all pass. Then run it without `--test-threads=1`, and it must also pass.

- [ ] **Step 5: Commit**

```bash
git add -A crates/infigraph-core
git commit -m "feat(core): clients hold one lease per daemon and follow it across restarts (#38, #124)"
```

---

### Task 5: Lease at the lifecycle entry points; the daemon never leases itself

**Files:**
- Modify: `crates/infigraph-core/src/daemon/lifecycle.rs` (`ensure_daemon_for_routed_access` ~L130, `ensure_daemon_running_required` ~L304)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`cmd_daemon`, right after `acquire_watch_lock`)
- Test: `crates/infigraph-core/src/daemon/lifecycle.rs` `#[cfg(test)]` module

**Interfaces:**
- Consumes: `lease::hold`, `lease::is_held`, `lease::mark_self_daemon` (Task 4).

- [ ] **Step 1: Write the failing test** (call-site pin) in `lifecycle.rs`'s test module:

```rust
    /// Every client funnels through these two entry points; both must lease,
    /// or a future third caller path silently never keeps a daemon alive.
    #[test]
    fn the_routed_access_entry_point_leases_a_live_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        let lock = root.join(".infigraph").join("watch.lock");
        let _held = crate::lockfile::try_acquire(&lock, "test-daemon").unwrap().unwrap();
        ensure_daemon_for_routed_access(root).unwrap();
        assert!(crate::daemon::lease::is_held(root), "routed access must hold a lease");
    }

    #[test]
    fn the_required_entry_point_leases_a_live_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        let lock = root.join(".infigraph").join("watch.lock");
        let _held = crate::lockfile::try_acquire(&lock, "test-daemon").unwrap().unwrap();
        let _ = ensure_daemon_running_required(root, std::path::Path::new("/nonexistent"));
        assert!(crate::daemon::lease::is_held(root), "the required entry point must hold a lease");
    }
```

`is_held` is true from the moment `hold` inserts, before the background attach. With no socket bound here, the thread drops the entry after `connect_allowing_for_startup` gives up (a live lock means it waits up to 30s), so asserting right after the call is deterministic. If `ensure_daemon_running_required`'s staleness pruning (`prune_stale_daemon`) treats the test's lock holder as stale, write the payload a real daemon writes; see `acquire_watch_lock` in `info_commands.rs`.

- [ ] **Step 2: Run and see the tests fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib daemon::lifecycle::tests::the_`
Expected: FAIL on the `is_held` assertions.

- [ ] **Step 3: Implement**
- In `ensure_daemon_for_routed_access`: change the early return to `if daemon_is_alive(&lock_path) { crate::daemon::lease::hold(root); return Ok(()); }`, and call `crate::daemon::lease::hold(root);` right before the final `Ok(())`.
- In `ensure_daemon_running_required`: wrap the body so the outcome is computed into `let outcome = ...;`. Then run `if matches!(outcome, DaemonStartOutcome::Spawned) || (outcome == DaemonStartOutcome::AlreadyRunning && daemon_is_alive(&root.join(".infigraph").join("watch.lock"))) { crate::daemon::lease::hold(root); }` and return `outcome`. `AlreadyRunning` also means "not indexed" or "remote", hence the liveness check.
- In `cmd_daemon`, right after `let _lock = acquire_watch_lock(&lock_path)?;`, add `infigraph_core::daemon::lease::mark_self_daemon(root);` with a comment: a daemon leasing itself would never idle out (#38).

- [ ] **Step 4: Run and see the tests pass.** Same command. Then `cargo test -p infigraph-core --test watch_daemon` (with `cargo build -p infigraph-cli` first). Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add -A crates/infigraph-core crates/infigraph-cli
git commit -m "feat(core,cli): clients lease the daemon they ensure; a daemon never leases itself (#38)"
```

---

### Task 6: Coordinator idle exit plus real-daemon tests

**Files:**
- Modify: `crates/infigraph-core/src/daemon/mod.rs`: the `settings!` block (~L58), `run_write_coordinator`'s read-service binding (~L660), the request loop (~L1482), and the loop head (after `path_is_gone`, ~L880)
- Create: `crates/infigraph-cli/tests/daemon_idle_exit.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: `pub fn daemon_idle_settings(root: &Path) -> DaemonIdle` (grace and check as `u64` fields `grace_secs`, `check_secs`).

- [ ] **Step 1: Write the failing integration tests** in `crates/infigraph-cli/tests/daemon_idle_exit.rs`. Copy `cli_binary`, `KillOnDrop` and `wait_until_gone` verbatim from `crates/infigraph-cli/tests/watch_daemon_docs.rs`; cargo compiles each test file as its own crate, and this is the established precedent. Then:

```rust
use std::process::{Command, Stdio};
use std::time::Duration;

fn indexed_project() -> tempfile::TempDir {
    let dir = tempfile::Builder::new().prefix("infigraph-idle-exit-").tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "def a():\n    pass\n").unwrap();
    let st = Command::new(cli_binary())
        .args(["index"])
        .current_dir(dir.path())
        .env("INFIGRAPH_BACKEND", "kuzu")
        .env("INFIGRAPH_NO_WATCH", "1")
        .status()
        .unwrap();
    assert!(st.success());
    dir
}

fn spawn_daemon(root: &std::path::Path, grace: &str) -> KillOnDrop {
    let child = Command::new(cli_binary())
        .args(["daemon", "--debounce", "50"])
        .current_dir(root)
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env("INFIGRAPH_DAEMON_IDLE_GRACE_SECS", grace)
        .env("INFIGRAPH_DAEMON_IDLE_CHECK_SECS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let lock = root.join(".infigraph").join("watch.lock");
    assert!(infigraph_core::daemon::lifecycle::wait_for_daemon_ready(&lock, Duration::from_secs(30)));
    KillOnDrop(child)
}

fn daemon_alive(root: &std::path::Path) -> bool {
    infigraph_core::daemon::lifecycle::daemon_is_alive(&root.join(".infigraph").join("watch.lock"))
}

fn attach_lease(root: &std::path::Path) -> infigraph_core::daemon::read_endpoint::ReadStream {
    let mut s = infigraph_core::daemon::read_endpoint::connect_allowing_for_startup(root).unwrap();
    infigraph_core::daemon::read_protocol::write_attach(&mut s, std::process::id()).unwrap();
    s
}

#[test]
fn an_unleased_daemon_exits_after_its_grace() {
    let project = indexed_project();
    let mut daemon = spawn_daemon(project.path(), "2");
    assert!(
        wait_until_gone("idle daemon", || daemon_alive(project.path())).is_some(),
        "a daemon with no lease and no requests must exit after its grace"
    );
    assert!(daemon.0.wait().unwrap().success(), "idle exit must be a clean exit");
    let sock_gone = infigraph_core::daemon::read_endpoint::ReadEndpoint::for_root(project.path())
        .connect()
        .is_err();
    assert!(sock_gone, "the read endpoint must be released on idle exit");
}

#[test]
fn a_leased_daemon_stays_up_and_exits_once_released() {
    let project = indexed_project();
    let _daemon = spawn_daemon(project.path(), "2");
    let lease = attach_lease(project.path());
    std::thread::sleep(Duration::from_secs(6)); // 3x the grace
    assert!(daemon_alive(project.path()), "a held lease must keep the daemon up");
    drop(lease);
    assert!(wait_until_gone("released daemon", || daemon_alive(project.path())).is_some());
}

/// Review Focus 3.
#[cfg(unix)]
#[test]
fn a_killed_lease_holder_releases_its_lease() {
    let project = indexed_project();
    let _daemon = spawn_daemon(project.path(), "2");
    // A child process that holds a lease: `infigraph` itself, through the
    // routed backend, sleeping inside a long-running read-only command is
    // brittle -- use a helper binary mode instead: this test binary re-execs
    // itself with an env var that makes it attach and sleep.
    let mut holder = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lease_holder_helper", "--nocapture", "--ignored"])
        .env("LEASE_HOLDER_ROOT", project.path())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(4));
    assert!(daemon_alive(project.path()), "the child's lease must keep the daemon up");
    holder.kill().unwrap(); // SIGKILL on unix
    let _ = holder.wait();
    assert!(wait_until_gone("daemon after holder SIGKILL", || daemon_alive(project.path())).is_some());
}

#[test]
#[ignore = "helper for a_killed_lease_holder_releases_its_lease; runs only when re-exec'd"]
fn lease_holder_helper() {
    let Some(root) = std::env::var_os("LEASE_HOLDER_ROOT") else { return };
    let _lease = attach_lease(std::path::Path::new(&root));
    std::thread::sleep(Duration::from_secs(600));
}

#[test]
fn zero_grace_never_idles_out() {
    let project = indexed_project();
    let _daemon = spawn_daemon(project.path(), "0");
    std::thread::sleep(Duration::from_secs(4));
    assert!(daemon_alive(project.path()));
}
```

- [ ] **Step 2: Run and see the tests fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test daemon_idle_exit -- --test-threads=1`
Expected: `an_unleased_daemon_exits_after_its_grace` and `a_killed_lease_holder_releases_its_lease` FAIL, because the daemon never exits. The two "stays up" tests pass trivially.

- [ ] **Step 3: Implement in `daemon/mod.rs`**

1. Settings, next to the `scip` group:

```rust
crate::settings! {
    daemon_idle {
        // #38: how long a daemon with no lease and no request waits before
        // exiting. 0 disables idle exit -- a watcher that never stops.
        grace_secs: u64 = 1800,
        // How often the coordinator evaluates it. Coarse: the check is
        // cheap, but an exit an extra minute late costs nothing.
        check_secs: u64 = 60,
    }
}

/// Resolved `daemon_idle` settings. Read once at coordinator start, like
/// every other daemon-lifetime setting.
pub fn daemon_idle_settings(root: &Path) -> DaemonIdle {
    DaemonIdle::resolve_or_default(
        RawDaemonIdle::default(),
        crate::settings_file::ConfigScope::Project(root),
    )
}
```

2. In `run_write_coordinator`, before `bind_read_service`: `let liveness = Arc::new(liveness::Liveness::new());`. Inside the `serve_requests` branch, clone it into the closure and call `read_service::ReadService::start_serving(&root, source.clone(), docs_reads.clone(), READ_SERVICE_WORKERS, liveness.clone())` instead of `start_with_sources`.

3. In the request loop, right before `route_or_serve_request(` (~L1487): `liveness.touch();`, since a write request is activity.

4. Before `loop {`: `let idle = daemon_idle_settings(root); let idle_grace = Duration::from_secs(idle.grace_secs); let idle_check = Duration::from_secs(idle.check_secs.max(1)); let mut last_idle_check = std::time::Instant::now();`

5. In the loop, right after the `path_is_gone` block:

```rust
        // #38: nobody holds a lease and nothing has touched this daemon for
        // its grace -- leave through the same clean shutdown as a vanished
        // root. Never mid-write: in-flight work defers the exit.
        if serve_requests && last_idle_check.elapsed() >= idle_check {
            last_idle_check = std::time::Instant::now();
            let work_in_flight = drain_in_flight.is_some()
                || full_reindex_in_flight.is_some()
                || scip_in_flight.is_some()
                || scip_import_in_flight.is_some();
            let idle_for = liveness.idle_for(liveness::now_secs());
            if liveness::idle_exit_due(idle_for, idle_grace, work_in_flight) {
                eprintln!(
                    "[watch] idle for {}s with no lease on {} -- shutting down",
                    idle_for.map(|d| d.as_secs()).unwrap_or(0),
                    root.display()
                );
                break;
            }
        }
```

- [ ] **Step 4: Run and see the tests pass.** Same command as Step 2. Expected: 5 passed (1 ignored helper). Then `cargo test -p infigraph-core --test watch_daemon` and `cargo test -p infigraph-core --test daemon_protocol_watcher_wiring --test read_service`. Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add -A crates/infigraph-core crates/infigraph-cli
git commit -m "feat(core): a daemon with no lease exits after an idle grace (#38, #124)"
```

---

### Task 7: Docs, gates and issue close-out

**Files:**
- Modify: `CLAUDE.md` (the "Cross-cutting invariants" list)

- [ ] **Step 1:** Add one invariant bullet after the "Reads do not open the stores outside the daemon" bullet:

```markdown
- **A daemon lives as long as someone leases it (#38, #124).** Clients hold one `Attach` connection per (process, project) via `daemon::lease::hold`, called from `ensure_daemon_for_routed_access` and `ensure_daemon_running_required` -- a new client entry point must go through one of them or it never keeps a daemon alive. The daemon parks each lease on its own thread (never a read-pool worker) and exits after `daemon_idle.grace_secs` (default 1800, 0 disables) with zero leases, no requests and no work in flight. A daemon never leases itself (`lease::mark_self_daemon`).
```

- [ ] **Step 2: Full gates, one crate at a time**

```bash
E="env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu"
$E cargo fmt --all -- --check
$E cargo clippy --all-targets -- -D warnings
$E cargo build -p infigraph-cli
$E cargo test -p infigraph-core
$E cargo test -p infigraph-docs
$E cargo test -p infigraph-cli
$E cargo test -p infigraph-mcp
```

Expected: all green. If there's a failure, rerun that one test with `--test-threads=1` before treating it as real.

- [ ] **Step 3: Commit, running the pre-commit hook (no `--no-verify`)**

```bash
git add CLAUDE.md
git commit -m "docs: the daemon lease invariant (#38, #124)"
```

- [ ] **Step 4: Issues** (after the user confirms pushing): comment on #124 that its daemon half landed and name the commits, noting the supervisor half remains open. Close #38 with the commits and the test evidence. File a follow-up issue: "doctor reads the daemon's lease count instead of the MCP registry", referencing `doctor::check_one_watcher` / `project_has_live_mcp_instance`.
