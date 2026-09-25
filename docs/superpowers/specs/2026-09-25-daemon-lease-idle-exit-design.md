# Daemon Lease Connections and Idle Exit — Design

**Issues:** #38 (idle exit for a live daemon with no client), #124 (daemon half: track connected clients, self-terminate at zero).
**Status:** approved direction 2026-09-25; spec under review.

## Problem

A detached `infigraph daemon` on the current build, for a directory that still
exists, runs forever. The existing exits cover a deleted root (`path_is_gone`,
06c6d56), a stale build (`prune_stale_holder`), a stop sentinel, and a fault.
None covers "nobody needs me any more". `doctor` flags the case (8739e8e) by
cross-checking the MCP instance registry, and nothing acts on it.

The registry is the wrong signal to act on. An instance records one
`project_path`, the directory it was launched in, but an MCP worker reads any
project through `path=` and group tools, and since #159 every CLI read goes
through the daemon too. A daemon serving those reads looks orphaned to the
registry.

## Goal

The daemon knows, directly and in real time, how many client processes are
using it, and exits cleanly after a grace period once that count is zero and
nothing else has touched it.

Success criteria:
- A daemon with no leases and no requests for `grace_secs` exits within one
  check interval, releasing `watch.lock` and unlinking its socket.
- A daemon with a lease held by any live process never idles out, however long
  that process goes without querying.
- A client process that dies by any means (exit, panic, SIGKILL) releases its
  lease without cooperating.
- Nothing on the read path gets slower, and held leases never occupy
  read-pool workers.

## Non-goals

- #124's supervisor half (`kill_on_drop` / process-group for the MCP worker).
  Independent; stays open on #124.
- Changing `doctor`. It keeps its registry-based warning; switching it to ask
  the daemon for its lease count is a follow-up (filed as an issue when this
  lands, per the no-scope-out rule).
- The write `.request` file protocol. Unchanged; writes count as activity only.

## Design

### Wire protocol (`daemon/read_protocol.rs`)

The first frame on a connection becomes a `ClientFrame`:

```rust
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientFrame {
    Attach(Attach),     // { "attach_pid": u32 }
    Read(ReadRequest),  // unchanged shape
}
```

`untagged` keeps a `ReadRequest` byte-identical on the wire, so a new daemon
serves an old client unchanged. `Attach` has a field no `ReadRequest` has, so
the two can never be confused. `read_request` becomes `read_client_frame`;
`write_request` is kept for reads, and `write_attach` is added.

After an `Attach`, the daemon writes no response frames. The client never writes
anything else either. The connection exists only so that its EOF can be observed.

### Daemon side

**Liveness state.** The coordinator owns one `Arc<Liveness>` for the whole run
(`daemon/liveness.rs`, new):

```rust
pub struct Liveness {
    leases: AtomicUsize,
    last_activity: AtomicU64, // epoch seconds
}
```

with `lease_opened()`, `lease_closed()` (which also stamps activity), `touch()`,
and `idle_for(now) -> Option<Duration>` (`None` while any lease is held). It is
owned by the coordinator and passed into `ReadService::start_with_sources`,
not created inside the service. The service is rebuilt when its socket is
rebound (#187), and a count inside it would reset to zero with leases still
open. Startup counts as activity, so a fresh daemon always gets a full grace.

**Dispatch (`daemon/read_service.rs`).** The pool job reads the first frame:
- `Read(req)`: `touch()`, then serve exactly as today.
- `Attach { attach_pid }`: `lease_opened()`, log `[lease] attached pid N`,
  then hand the stream to a fresh, detached, named thread
  (`infigraph-lease`) and return immediately, freeing the pool worker. That
  thread blocks reading the stream. On EOF or any error it calls
  `lease_closed()` and logs `[lease] released pid N`. Held leases therefore cost
  one parked thread each, never a pool slot.

A malformed first frame is still an error for that connection only, exactly as
today.

**Writes.** `route_or_serve_request` calls `touch()` for every request it
accepts, so a CLI that only writes also keeps the daemon up during the grace.

**Exit rule (`daemon/mod.rs`, coordinator loop).** Next to `path_is_gone`,
on its own `check_secs` interval:

```rust
pub fn idle_exit_due(idle_for: Option<Duration>, grace: Duration, work_in_flight: bool) -> bool
```

It is true when `grace > 0`, `idle_for >= grace`, and `!work_in_flight`, where
work in flight means `drain_in_flight`, `full_reindex_in_flight`,
`scip_in_flight`, or `scip_import_in_flight` is `Some`. When it is due, the loop
logs `[watch] idle for Ns with no lease on <root> -- shutting down` and `break`s
through the existing clean-shutdown path. It is checked only when
`serve_requests` is true, so in-process `watch_project` watching is untouched.

**Settings.** A new group in `daemon/mod.rs`:

```rust
crate::settings! {
    daemon_idle {
        grace_secs: u64 = 1800,  // 0 disables idle exit
        check_secs: u64 = 60,
    }
}
```

This gives `INFIGRAPH_DAEMON_IDLE_GRACE_SECS` and `INFIGRAPH_DAEMON_IDLE_CHECK_SECS`,
and `[daemon_idle]` in `config.toml`. It is picked up automatically by #199's
startup check through `inventory`. Like every other daemon setting, it is read
once at coordinator start.

### Client side (`daemon/lease.rs`, new)

```rust
pub fn hold(root: &Path)
```

`hold` is process-wide and idempotent. It uses a static
`Mutex<HashMap<PathBuf, ()>>` keyed by the canonical project root. If the root
is already present, it returns. Otherwise it inserts the root and spawns a
named thread (`infigraph-lease-hold`) that:
1. connects, with the same "daemon alive but not yet bound" grace that
   `RemoteExec` uses (`connect_allowing_for_startup` moves into
   `read_endpoint.rs` as a free function so both use one copy);
2. writes `Attach { attach_pid: std::process::id() }`;
3. blocks reading until EOF, which means the daemon exited or restarted;
4. if `daemon_is_alive(watch.lock)` shows a successor within the startup grace
   (a `daemon-restart` or a build-mismatch respawn), goes back to step 1, so an
   idle MCP session keeps its lease across restarts without querying;
   otherwise it removes the root from the map, and the next `hold` attaches
   again.

If the connect or write fails, the thread removes the entry and exits.
`hold` never blocks its caller, never returns an error, and never panics. A
lease is an optimisation over respawning, not a correctness requirement.
When the client process exits, the kernel closes the socket.

**Where `hold` is called.** It is called at the success paths of the two public
lifecycle entry points, which every client funnels through:
- `ensure_daemon_for_routed_access`, on both the already-alive return and
  after a successful start. This covers every `Infigraph::init*` and
  `DocIndex::init` under the daemon backend, including `path=` and group reads.
- `ensure_daemon_running_required`, on `Spawned` and on a live
  `AlreadyRunning`. This covers MCP boot-time watching, so an open MCP session
  that never queries still holds its launch project's lease, and the CLI's
  auto-watch.

A test pins both call sites, so a future third entry point cannot silently
skip leasing.

**A daemon never leases itself.** `cmd_daemon` records its own root in a
process-global before entering the coordinator, and `hold` is a no-op for that
root. Without this guard, any code path inside the daemon that reached a
lifecycle entry point would keep it alive forever. Today none does, because the
daemon is pinned to `INFIGRAPH_BACKEND=kuzu`. The guard makes that structural
rather than incidental.

### Version skew

- New client with an old daemon: the old daemon fails to parse `Attach` as a
  `ReadRequest`, closes the connection, and the lease thread sees EOF and
  drops the entry. Each later `hold` retries once, which costs one connect per
  `init`. The behavior is today's, and same-build pruning replaces the old daemon soon anyway.
- Old client with a new daemon: its reads parse as `ClientFrame::Read` and
  count as activity. It holds no lease, so the daemon may idle out between its
  reads, and its next `init` respawns the daemon. This is correct, just slower.

## Testing

- **Unit:**
  - `idle_exit_due`: the grace boundary is inclusive, in-flight work blocks the exit, a held lease (`None`) blocks it, and `grace = 0` disables it.
  - `Liveness`: counting, and `lease_closed` stamping activity.
  - `ClientFrame`: a round trip, and an old-shape `ReadRequest` JSON parsing as `Read`.
- **Read service** (`tests/read_service.rs`):
  - An attached lease raises the count and does not occupy a pool worker. Open more leases than `READ_SERVICE_WORKERS` and check that a read still answers.
  - Dropping the client stream lowers the count.
- **Real daemon** (`tests/watch_daemon.rs`, isolated env, `GRACE=2`, `CHECK=1`):
  - With no lease, the daemon exits, `watch.lock` is released, and the socket is unlinked.
  - With a lease held from the test process, the daemon is still alive after 3× the grace.
  - After the lease is dropped, the daemon exits within the grace plus one check.
  - A lease held by a child process that is then `SIGKILL`ed is released, and the daemon exits.
- **Call-site pin:** after `ensure_daemon_for_routed_access`, the lease map
  contains the root. The same holds after `ensure_daemon_running_required`.
- **Self-lease guard:** `hold` for the process's own daemon root is a no-op.

Existing tests are unaffected by the 30-minute default. Test daemons that leak
(#136) now also exit on their own after the grace.

## Rollout notes

- Log lines (`[lease] attached/released`, `[watch] idle ... shutting down`)
  go to `daemon.log`, which already names the daemon generation (#115).
- Comment on #124 saying its daemon half is done by this change and its
  supervisor half remains. Close #38.
- Follow-up issue: `doctor` reads the lease count from the daemon instead of
  the registry.
