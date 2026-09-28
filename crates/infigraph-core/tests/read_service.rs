//! End-to-end tests for the daemon's read service.

use std::path::Path;
use std::sync::Arc;

use infigraph_core::daemon::read_endpoint::ReadEndpoint;
use infigraph_core::daemon::read_protocol::{collect_rows, write_request, ReadRequest, Store};
use infigraph_core::graph::GraphStore;

/// End-to-end: a client gets rows back over the socket.
#[test]
fn a_client_reads_rows_over_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
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
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, store, 4).unwrap();

    let rows = client_query(root, "MATCH (f:File) RETURN f.id").unwrap();
    assert_eq!(rows, vec![vec!["a.rs".to_string()]]);

    svc.shutdown();
}

/// A write sent to the read service is refused, and the refusal comes from
/// the database, not from a keyword check.
#[test]
fn a_write_sent_to_the_read_service_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    drop(GraphStore::open(&graph).unwrap());
    let store = open_shared_store(&graph);
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, store, 2).unwrap();

    let err = client_query(
        root,
        "CREATE (:File {id: 'x', name: 'x', path: 'x', language: 'rust', symbol_count: 0})",
    )
    .expect_err("a write must be refused");
    assert!(err.to_string().contains("not a read"), "unexpected: {err}");

    svc.shutdown();
}

/// A bare `COMMIT` returns an empty result rather than an error, and that
/// is a decision, not an accident.
///
/// lbug's parser judges `COMMIT` read-only, so it passes `ensure_read_only`
/// and reaches `raw_query_on`'s transaction-control no-op -- which is
/// exactly what `DaemonKuzuBackend::raw_query` does today through
/// `open_read` -> `KuzuBackend::raw_query`. Refusing it instead is arguably
/// more correct for a read service, but it would change observable
/// behaviour for `raw_query`'s ~247 callers in a change whose only purpose
/// is transport routing, and this plan's constraint is that the write
/// pipeline and its semantics are untouched.
///
/// This does not weaken the truncation guarantee: the reply is a legitimate
/// empty result *with* an `End` frame. A truncated stream has no `End` and
/// is still an error -- see `read_protocol` and the test below.
#[test]
fn a_bare_commit_returns_an_empty_result_exactly_as_the_local_backend_does() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    let store = open_shared_store(&graph);
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, store, 2).unwrap();

    let got = client_query(root, "COMMIT").expect("a bare COMMIT must not be an error");
    assert!(
        got.is_empty(),
        "a transaction-control statement no-ops to zero rows, as it does locally: {got:?}"
    );

    svc.shutdown();
}

/// Reads must be served in parallel *while* an index operation holds
/// `index.lock`. If reads ever queue behind indexing, the read service has
/// been wired into the write pipeline by mistake and the whole point is
/// lost.
#[test]
fn reads_are_served_concurrently_while_an_index_operation_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
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
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, store, 8).unwrap();

    // Hold index.lock for the duration, as a real index operation would.
    let _index_lock = infigraph_core::lockfile::acquire(
        &root.join(".infigraph").join("index.lock"),
        "test-index-op",
        std::time::Duration::from_secs(5),
    )
    .expect("acquire index.lock");

    let start = std::time::Instant::now();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let root = root.to_path_buf();
            std::thread::spawn(move || client_query(&root, "MATCH (f:File) RETURN f.id"))
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap().unwrap(), vec![vec!["a.rs".to_string()]]);
    }
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "8 reads took {:?} with index.lock held -- they are queueing behind it",
        start.elapsed()
    );

    svc.shutdown();
}

/// A write committed by the daemon must be visible to the very next read.
///
/// If the read service ever holds its own `Database` -- even in the same
/// process -- it cannot see the writer's uncommitted WAL, and this fails.
/// That is #149 reproduced inside the daemon, and no test without a
/// concurrent writer would notice.
#[test]
fn a_write_is_visible_to_the_next_read_through_the_service() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();

    let store = open_shared_store(&graph);
    let svc =
        infigraph_core::daemon::read_service::ReadService::start(root, store.clone(), 4).unwrap();

    assert!(client_query(root, "MATCH (f:File) RETURN f.id")
        .unwrap()
        .is_empty());

    // Write through the SAME store the service holds, without checkpointing.
    {
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'fresh.rs', name: 'fresh.rs', path: 'fresh.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();
    }

    let rows = client_query(root, "MATCH (f:File) RETURN f.id").unwrap();
    assert_eq!(
        rows,
        vec![vec!["fresh.rs".to_string()]],
        "the read service must observe the daemon's own uncheckpointed write; \
         if this is empty, the service is holding a second Database handle"
    );

    svc.shutdown();
}

/// The client-side executor speaks the same protocol the service serves,
/// and satisfies the same `QueryExec` trait `GraphQuery` runs on -- which is
/// what lets all 1045 lines of Cypher serve both paths unchanged.
#[test]
fn remote_exec_satisfies_query_exec_against_a_live_service() {
    use infigraph_core::graph::query_exec::QueryExec;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
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
    let svc = infigraph_core::daemon::read_service::ReadService::start(root, store, 2).unwrap();

    // `RemoteExec` satisfies `QueryExec`, so it can be handed to
    // `GraphQuery::new_with(exec)` by value exactly like `LocalExec`.
    let exec = infigraph_core::graph::remote_exec::RemoteExec::new(root);
    let rows = exec.query_rows("MATCH (f:File) RETURN f.id").unwrap();
    assert_eq!(rows, vec![vec!["a.rs".to_string()]]);

    // And the point of the trait: a real `GraphQuery` runs over it unchanged.
    let q = infigraph_core::graph::GraphQuery::new_with(exec);
    let via_graph_query = q.raw_query("MATCH (f:File) RETURN f.id").unwrap();
    assert_eq!(via_graph_query, rows);

    svc.shutdown();
}

/// Killing the service mid-response must surface an error, never a
/// successful empty result. This is the one failure mode with a precedent
/// in this codebase: `Infigraph::init` once served a 0-symbol graph as
/// healthy after a "successful" rebuild.
///
/// Note this is the frame-boundary case, not the mid-frame one: the bytes
/// written here are a complete, well-formed `Rows` frame and nothing more,
/// so the stream is byte-identical to a valid response that simply has not
/// finished. Only the absent `End` distinguishes it from an empty result.
#[test]
fn a_service_that_dies_mid_response_produces_an_error_not_an_empty_result() {
    use infigraph_core::daemon::read_protocol::{collect_rows, write_frame, ReadFrame};

    // Simulate the wire directly: rows, then the connection dies.
    let mut buf = Vec::new();
    write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["a.rs".to_string()]])).unwrap();
    // No End frame -- the peer went away.

    let err =
        collect_rows(&mut buf.as_slice()).expect_err("a stream with no End frame must be an error");
    assert!(
        err.to_string().contains("not an empty result set"),
        "the error must say plainly that this is not an empty result: {err}"
    );
}

/// A real write coordinator must answer reads on its endpoint. Without this
/// the service exists but nothing starts it.
///
/// Asserts only that the read *succeeds*, not that it returns rows. Whether
/// the daemon has indexed anything yet is a separate concern, and on macOS
/// a `TempDir` root is symlinked, where watch-driven indexing currently
/// delivers no events at all (#151). The property this task introduces is
/// that the daemon binds the endpoint and serves from its own store, and
/// only the daemon can answer here -- nothing else binds this name.
#[test]
#[ignore = "drives a real write coordinator; run explicitly"]
fn a_running_daemon_answers_reads_on_its_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("a.rs"), "pub fn hello() {}\n").unwrap();

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = token.clone();
    let root_for_thread = root.clone();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root_for_thread,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50,
            stop_rx,
            |_| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
            &token_for_thread,
            None,
            None,
        )
    });

    // Generous: the coordinator builds the whole bundled language registry
    // before it opens anything, which its own comment notes costs seconds in
    // a debug build.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut served = false;
    let mut last_err = None;
    while std::time::Instant::now() < deadline {
        match client_query(&root, "MATCH (f:File) RETURN f.id") {
            Ok(_) => {
                served = true;
                break;
            }
            Err(e) => last_err = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    token.cancel();
    let _ = stop_tx.send(());
    let _ = handle.join();

    assert!(
        served,
        "a running daemon must serve reads on its endpoint; last error: {last_err:?}"
    );
}

/// #187: the service knows when its socket file is gone, and can still be
/// shut down then -- stopping it used to rely on connecting to itself, which
/// hangs the join forever once the file is removed.
#[test]
fn a_service_notices_its_socket_file_was_removed_and_still_shuts_down() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let graph = root.join(".infigraph").join("graph");
    std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
    drop(GraphStore::open(&graph).unwrap());
    let svc = infigraph_core::daemon::read_service::ReadService::start(
        root,
        open_shared_store(&graph),
        1,
    )
    .unwrap();
    assert!(svc.socket_intact());

    let Some(socket) = ReadEndpoint::for_root(root).socket_path() else {
        svc.shutdown();
        return; // no socket file on this platform (Linux, Windows): nothing to reap
    };
    std::fs::remove_file(&socket).unwrap();
    assert!(!svc.socket_intact());
    assert!(client_query(root, "MATCH (f:File) RETURN f.id").is_err());

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        svc.shutdown();
        let _ = done_tx.send(());
    });
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .is_ok(),
        "shutdown must not hang once the socket file is gone"
    );
}

/// #187: a daemon whose socket file is removed out from under it (macOS
/// reaps `/tmp` entries older than ~3 days) must serve reads again, not sit
/// alive, holding `watch.lock`, while every client is refused forever.
#[test]
#[ignore = "drives a real write coordinator; run explicitly"]
fn a_running_daemon_rebinds_a_socket_that_was_removed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("a.rs"), "pub fn hello() {}\n").unwrap();

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = token.clone();
    let root_for_thread = root.clone();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root_for_thread,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50,
            stop_rx,
            |_| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
            &token_for_thread,
            None,
            None,
        )
    });

    let served_within = |secs: u64| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        let mut last_err = None;
        while std::time::Instant::now() < deadline {
            match client_query(&root, "MATCH (f:File) RETURN f.id") {
                Ok(_) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Err(last_err)
    };

    let first = served_within(90);
    let Some(socket) = ReadEndpoint::for_root(&root).socket_path() else {
        token.cancel();
        let _ = stop_tx.send(());
        let _ = handle.join();
        return; // no socket file on this platform (Linux, Windows): nothing to reap
    };
    std::fs::remove_file(&socket).unwrap();
    assert!(
        client_query(&root, "MATCH (f:File) RETURN f.id").is_err(),
        "with its socket file gone, no client can reach the daemon"
    );
    let again = served_within(30);

    token.cancel();
    let _ = stop_tx.send(());
    let _ = handle.join();

    assert!(first.is_ok(), "the daemon never served at all: {first:?}");
    assert!(
        again.is_ok(),
        "the daemon must serve again after its socket file was removed: {again:?}"
    );
}

// ── helpers ──────────────────────────────────────────────────────────

/// The one `GraphStore` the service serves from.
///
/// Deliberately a plain `GraphStore::open` behind an `Arc`, not a bespoke
/// `Database`: opening through the store is what carries `validate_db_file`'s
/// truncation preflight, `refuse_newer_schema`, the bounded write buffer
/// pool and the `write_phase` breadcrumbs.
fn open_shared_store(graph: &Path) -> Arc<GraphStore> {
    Arc::new(GraphStore::open(graph).unwrap())
}

/// Ask the read service at `root` for `cypher`, exactly as `RemoteExec` will.
fn client_query(root: &Path, cypher: &str) -> anyhow::Result<Vec<Vec<String>>> {
    let mut stream = ReadEndpoint::for_root(root).connect()?;
    write_request(
        &mut stream,
        &ReadRequest {
            store: Store::Graph,
            query: cypher.to_string(),
            params: vec![],
            chunk_size: 1024,
        },
    )?;
    collect_rows(&mut stream)
}

// ── leases (#38, #124) ───────────────────────────────────────────────

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
    let (project, source) = indexed_project_and_source();
    let liveness = Arc::new(Liveness::new());
    let _svc = ReadService::start_serving(project.path(), source, None, 2, liveness.clone(), None)
        .unwrap();
    let lease = attach(project.path());
    assert!(
        wait_for(|| liveness.leases() == 1),
        "attach must be counted"
    );
    drop(lease);
    assert!(
        wait_for(|| liveness.leases() == 0),
        "EOF must release the lease"
    );
}

/// Review Focus 2: leases must never occupy pool workers.
#[test]
fn more_leases_than_workers_do_not_starve_reads() {
    let (project, source) = indexed_project_and_source();
    let liveness = Arc::new(Liveness::new());
    let workers = 2;
    let _svc = ReadService::start_serving(
        project.path(),
        source,
        None,
        workers,
        liveness.clone(),
        None,
    )
    .unwrap();
    let leases: Vec<_> = (0..workers + 2).map(|_| attach(project.path())).collect();
    assert!(wait_for(|| liveness.leases() == workers + 2));
    let rows = client_query(project.path(), "MATCH (f:File) RETURN count(f)").unwrap();
    assert_eq!(
        rows.len(),
        1,
        "a read must still be served with every worker's worth of leases held"
    );
    drop(leases);
}

#[test]
fn a_read_touches_liveness() {
    let (project, source) = indexed_project_and_source();
    let liveness = Arc::new(Liveness::new());
    let _svc = ReadService::start_serving(project.path(), source, None, 2, liveness.clone(), None)
        .unwrap();
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
    let svc = ReadService::start_serving(project.path(), source, None, 2, liveness.clone(), None)
        .unwrap();
    let mut lease = attach(project.path());
    assert!(wait_for(|| liveness.leases() == 1));
    drop(svc);
    assert!(
        wait_for(|| liveness.leases() == 0),
        "shutdown must end parked leases"
    );
    assert!(
        matches!(
            infigraph_core::daemon::read_protocol::read_frame(&mut lease),
            Ok(Some(infigraph_core::daemon::read_protocol::ReadFrame::End))
        ),
        "a parked lease is acknowledged"
    );
    let mut buf = [0u8; 1];
    assert_eq!(
        std::io::Read::read(&mut lease, &mut buf).unwrap_or(0),
        0,
        "the client must see EOF"
    );
}

// ── client leases (#38, #124) ────────────────────────────────────────

use infigraph_core::daemon::lease;

#[test]
fn hold_attaches_once_and_is_idempotent() {
    let (project, source) = indexed_project_and_source();
    let liveness = Arc::new(Liveness::new());
    let _svc = ReadService::start_serving(project.path(), source, None, 2, liveness.clone(), None)
        .unwrap();
    lease::hold(project.path());
    lease::hold(project.path());
    assert!(wait_for(|| liveness.leases() == 1));
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(
        liveness.leases(),
        1,
        "a second hold must not open a second lease"
    );
    assert!(lease::is_held(project.path()));
}

#[test]
fn hold_is_a_noop_for_the_process_own_daemon_root() {
    let (project, source) = indexed_project_and_source();
    let liveness = Arc::new(Liveness::new());
    let _svc = ReadService::start_serving(project.path(), source, None, 2, liveness.clone(), None)
        .unwrap();
    lease::mark_self_daemon(project.path());
    lease::hold(project.path());
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(liveness.leases(), 0);
    assert!(!lease::is_held(project.path()));
}

#[test]
fn hold_with_no_daemon_forgets_the_root() {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".infigraph")).unwrap();
    lease::hold(project.path());
    assert!(wait_for(|| !lease::is_held(project.path())));
}

/// The case where a synchronous attach would hang its caller: a daemon that
/// holds `watch.lock` but has not bound its socket yet makes
/// `connect_allowing_for_startup` wait out its 30s grace. `hold` sits on
/// every `Infigraph::init`, so it must return regardless.
#[test]
fn hold_never_blocks_on_a_daemon_that_is_still_starting() {
    let project = tempfile::tempdir().unwrap();
    let lock_path = project.path().join(".infigraph").join("watch.lock");
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let _lock = infigraph_core::lockfile::try_acquire(&lock_path, "test-daemon")
        .unwrap()
        .unwrap();
    let t = std::time::Instant::now();
    lease::hold(project.path());
    assert!(
        t.elapsed() < std::time::Duration::from_secs(2),
        "hold must return at once, not wait for the daemon: took {:?}",
        t.elapsed()
    );
    assert!(
        lease::is_held(project.path()),
        "the attach is still pending"
    );
}

/// Review Focus 1: a restarted daemon is re-attached to without any new
/// `hold`. `daemon_is_alive` needs `watch.lock` held, so hold it here the way
/// a daemon does.
#[cfg(unix)]
#[test]
fn hold_reattaches_to_a_successor_service() {
    let (project, source) = indexed_project_and_source();
    let lock_path = project.path().join(".infigraph").join("watch.lock");
    let _lock = infigraph_core::lockfile::try_acquire(&lock_path, "test-daemon")
        .unwrap()
        .unwrap();
    // One Liveness across both services, exactly as the coordinator shares it
    // across a #187 rebind -- so this also pins Review Focus 5.
    let liveness = Arc::new(Liveness::new());
    let svc = ReadService::start_serving(
        project.path(),
        source.clone(),
        None,
        2,
        liveness.clone(),
        None,
    )
    .unwrap();
    lease::hold(project.path());
    assert!(wait_for(|| liveness.leases() == 1));
    drop(svc); // ends its parked leases, so the client sees EOF
    assert!(wait_for(|| liveness.leases() == 0));
    let _svc2 = ReadService::start_serving(project.path(), source, None, 2, liveness.clone(), None)
        .unwrap();
    assert!(
        wait_for(|| liveness.leases() == 1),
        "the lease must follow the daemon across a restart"
    );
}

/// Final-review C1: a daemon that does not understand `Attach` -- a build
/// from before leases -- reads the frame, fails to parse it and closes. The
/// lease thread must take that as "no leases here" and stop, not reconnect
/// in a tight loop (each attempt also logs a line in that daemon's log).
#[test]
fn a_daemon_that_rejects_attach_is_not_reconnected_in_a_loop() {
    let project = tempfile::tempdir().unwrap();
    let lock = project.path().join(".infigraph").join("watch.lock");
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    // A lock file, so the lease thread waits out its grace between attempts
    // rather than giving up because no daemon ever ran here.
    std::fs::write(&lock, b"").unwrap();

    let listener = ReadEndpoint::for_root(project.path()).bind().unwrap();
    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stub = {
        let (accepted, stop) = (accepted.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let Ok(Some(mut stream)) =
                    listener.accept_timeout(std::time::Duration::from_millis(50))
                else {
                    continue;
                };
                accepted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // An old daemon: one frame read, not understood, closed.
                let mut len = [0u8; 4];
                let _ = std::io::Read::read_exact(&mut stream, &mut len);
                drop(stream);
            }
        })
    };

    lease::hold(project.path());
    std::thread::sleep(std::time::Duration::from_secs(2));
    let n = accepted.load(std::sync::atomic::Ordering::Relaxed);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    stub.join().unwrap();
    // One retry is allowed: a single un-acked EOF is also what a daemon that
    // is shutting down looks like, and a restart must be followed (see
    // `hold_reattaches_to_a_successor_service`). Twice in a row is a refusal.
    assert!(
        n <= 2,
        "a rejected Attach must end the lease attempt, got {n} connections in 2s"
    );
    assert!(
        wait_for(|| !lease::is_held(project.path())),
        "the root must be forgotten, so a later hold can try again"
    );
}

// ---- #155: Status and Control on the read socket ----

use infigraph_core::daemon::coordinator_port::{
    ControlMsg, CoordinatorPort, PortMsg, BUSY, PORT_QUEUE,
};
use infigraph_core::daemon::read_protocol::{
    read_reply, write_op, ControlFrame, ControlRequest, OpReply, RoleState, StatusFrame,
    StatusReport, WatchAction, WatchRole,
};

/// A service with a control port and no graph at all.
fn control_service(
    root: &Path,
) -> (
    ReadService,
    Arc<CoordinatorPort>,
    std::sync::mpsc::Receiver<PortMsg>,
    Arc<Liveness>,
) {
    let liveness = Arc::new(Liveness::new());
    let (port, rx) = CoordinatorPort::new(1800, 60);
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

/// The next message a stub coordinator receives, which must be a control one.
fn next_control(rx: &std::sync::mpsc::Receiver<PortMsg>) -> ControlMsg {
    match rx.recv().unwrap() {
        PortMsg::Control(msg) => msg,
        PortMsg::Write { request, .. } => panic!("expected control, got a {request:?} write"),
    }
}

fn status(root: &Path) -> OpReply<StatusReport> {
    let mut s = ReadEndpoint::for_root(root).connect().unwrap();
    write_op(&mut s, &StatusFrame::default()).unwrap();
    read_reply(&mut s).unwrap().expect("a reply frame")
}

fn control_frame(role: WatchRole, action: WatchAction) -> ControlFrame {
    ControlFrame {
        control: ControlRequest { role, action },
    }
}

fn control(root: &Path, role: WatchRole, action: WatchAction) -> OpReply<()> {
    let mut s = ReadEndpoint::for_root(root).connect().unwrap();
    write_op(&mut s, &control_frame(role, action)).unwrap();
    read_reply(&mut s).unwrap().expect("a reply frame")
}

#[test]
fn status_answers_before_any_graph_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, port, _rx, _l) = control_service(dir.path());
    port.state.set_role(WatchRole::Code, RoleState::Running);
    let OpReply::Ok(r) = status(dir.path()) else {
        panic!("status must answer")
    };
    assert_eq!(r.code, RoleState::Running);
    assert_eq!(r.pid, std::process::id());
    svc.shutdown();
}

#[test]
fn status_and_control_do_not_count_as_activity_but_a_read_does() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _port, rx, liveness) = control_service(dir.path());
    let answer = std::thread::spawn(move || {
        let msg = next_control(&rx);
        msg.reply.send(Ok(())).unwrap();
    });
    let then = liveness::now_secs() - 100;
    liveness.last_activity_for_test(then);
    let _ = status(dir.path());
    let _ = control(dir.path(), WatchRole::Code, WatchAction::Start);
    answer.join().unwrap();
    assert!(liveness.idle_for(liveness::now_secs()).unwrap().as_secs() >= 100);
    // A read with no graph is refused, but it still counts: it was a use.
    let _ = client_query(dir.path(), "RETURN 1");
    assert!(liveness.idle_for(liveness::now_secs()).unwrap().as_secs() < 100);
    svc.shutdown();
}

#[test]
fn a_control_reply_carries_the_coordinators_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _port, rx, _l) = control_service(dir.path());
    let answer = std::thread::spawn(move || {
        let ok = next_control(&rx);
        assert_eq!(ok.request.action, WatchAction::Stop);
        ok.reply.send(Ok(())).unwrap();
        let err = next_control(&rx);
        err.reply.send(Err("no doc-watch loop".into())).unwrap();
    });
    assert!(matches!(
        control(dir.path(), WatchRole::Code, WatchAction::Stop),
        OpReply::Ok(())
    ));
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
    let streams: Vec<_> = (0..PORT_QUEUE)
        .map(|_| {
            let mut s = ReadEndpoint::for_root(dir.path()).connect().unwrap();
            write_op(&mut s, &control_frame(WatchRole::Code, WatchAction::Stop)).unwrap();
            s // keep the connection open
        })
        .collect();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while port.in_flight() < PORT_QUEUE && std::time::Instant::now() < deadline {
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
    write_op(&mut s, &control_frame(WatchRole::Code, WatchAction::Stop)).unwrap();
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
    assert!(matches!(
        control(dir.path(), WatchRole::Code, WatchAction::Stop),
        OpReply::Err(_)
    ));
    svc.shutdown();
}

// ── client-side idle release ─────────────────────────────────────────
//
// A client lets go of a lease it has not used for
// `daemon_idle.client_release_secs`, so a session that sits idle stops
// keeping its daemon alive. The next use leases again (and, through
// `ensure_daemon_for_routed_access`, respawns a daemon that has since
// exited). Unix only: releasing needs the socket's shutdown handle.

#[cfg(unix)]
fn start_with_short_release(
    release: std::time::Duration,
) -> (tempfile::TempDir, Arc<Liveness>, ReadService) {
    let (project, source) = indexed_project_and_source();
    lease::set_release_after_for_test(project.path(), release);
    let liveness = Arc::new(Liveness::new());
    let svc = ReadService::start_serving(project.path(), source, None, 2, liveness.clone(), None)
        .unwrap();
    (project, liveness, svc)
}

#[cfg(unix)]
#[test]
fn an_idle_lease_is_released_and_the_next_use_leases_again() {
    let (project, liveness, _svc) = start_with_short_release(ms(300));
    lease::hold(project.path());
    assert!(wait_for(|| liveness.leases() == 1));
    assert!(
        wait_for(|| liveness.leases() == 0),
        "an unused lease must be released"
    );
    assert!(wait_for(|| !lease::is_held(project.path())));
    std::thread::sleep(ms(400));
    assert_eq!(
        liveness.leases(),
        0,
        "a released lease must not re-attach on its own"
    );
    let _use = lease::in_use(project.path());
    assert!(
        wait_for(|| liveness.leases() == 1),
        "the next use must lease again"
    );
}

#[cfg(unix)]
#[test]
fn a_routed_read_leases_again_after_a_release() {
    let (project, liveness, _svc) = start_with_short_release(ms(300));
    lease::hold(project.path());
    assert!(wait_for(|| liveness.leases() == 1));
    assert!(wait_for(|| liveness.leases() == 0));
    let rows = infigraph_core::graph::query_exec::QueryExec::query_rows(
        &infigraph_core::graph::remote_exec::RemoteExec::new(project.path()),
        "MATCH (f:File) RETURN f.id",
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        wait_for(|| liveness.leases() == 1),
        "a routed read is a use, and must lease again"
    );
}

#[cfg(unix)]
#[test]
fn a_lease_in_use_is_not_released() {
    let (project, liveness, _svc) = start_with_short_release(ms(300));
    let busy = lease::in_use(project.path());
    assert!(wait_for(|| liveness.leases() == 1));
    std::thread::sleep(ms(1200));
    assert_eq!(
        liveness.leases(),
        1,
        "a use still in progress must keep the lease"
    );
    drop(busy);
    assert!(wait_for(|| liveness.leases() == 0));
}

#[cfg(unix)]
#[test]
fn a_pinned_lease_is_not_released() {
    let (project, liveness, _svc) = start_with_short_release(ms(300));
    lease::pin(project.path());
    lease::hold(project.path());
    assert!(wait_for(|| liveness.leases() == 1));
    std::thread::sleep(ms(1200));
    assert_eq!(liveness.leases(), 1, "a pinned lease must be kept");
    lease::unpin(project.path());
    assert!(wait_for(|| liveness.leases() == 0));
}

#[cfg(unix)]
static VETOED: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

#[cfg(unix)]
fn veto_listed_roots(root: &Path) -> bool {
    VETOED.lock().unwrap().iter().any(|r| r == root)
}

#[cfg(unix)]
#[test]
fn the_release_guard_can_keep_a_lease() {
    let (project, liveness, _svc) = start_with_short_release(ms(300));
    let root = project.path().canonicalize().unwrap();
    VETOED.lock().unwrap().push(root.clone());
    lease::set_release_guard(veto_listed_roots);
    lease::hold(project.path());
    assert!(wait_for(|| liveness.leases() == 1));
    std::thread::sleep(ms(1200));
    assert_eq!(liveness.leases(), 1, "the guard's veto must keep the lease");
    VETOED.lock().unwrap().retain(|r| r != &root);
    assert!(wait_for(|| liveness.leases() == 0));
}

#[cfg(unix)]
fn ms(n: u64) -> std::time::Duration {
    std::time::Duration::from_millis(n)
}
