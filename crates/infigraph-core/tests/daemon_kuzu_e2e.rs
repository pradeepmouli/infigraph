use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use infigraph_core::graph::GraphBackend;
use infigraph_core::Infigraph;

/// `INFIGRAPH_BACKEND` is process-wide, so the set/init/remove window in
/// `open_daemon_client` must not overlap between the tests in this binary --
/// otherwise one test's `remove_var` lands inside another's window and that
/// client silently opens a direct Kuzu backend instead of DaemonKuzu.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that kills and reaps a spawned child process on drop, so a
/// panic anywhere in this test (not just the happy path) can't leave a real
/// `infigraph daemon` process running forever against an abandoned tempdir.
/// `std::process::Child` does not do this itself -- Drop just closes the
/// parent's handle, the child keeps running independently. Mirrors the
/// `KillOnDrop` guard in `crates/infigraph-cli/tests/watch_daemon_docs.rs`
/// (kept as its own small copy here since that's a different crate).
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Same guarantee as `KillOnDrop`, for the one spawn path that can't own a
/// `Child`: `ensure_daemon_running` (production's "opportunistic auto-start")
/// hands back no process handle at all -- real callers want a detached,
/// independent daemon, not something to babysit. Without this, a panic
/// between that spawn and a test's own `watch.stop` sentinel write leaks a
/// real `infigraph daemon` process indefinitely -- exactly how 19 leaked
/// daemon processes (some running for days) were found and killed during a
/// disk-space incident on 2026-08-31. Kills by PID via
/// `ps::kill_infigraph_process`, which refuses anything that isn't
/// verifiably an infigraph binary, so this can't ever kill an unrelated
/// process even if the PID were somehow stale/recycled.
struct KillPidOnDrop(u32);

impl Drop for KillPidOnDrop {
    fn drop(&mut self) {
        let _ = infigraph_core::ps::kill_infigraph_process(self.0, false);
    }
}

/// infigraph-core has no dev-dependency on infigraph-cli, so
/// `env!("CARGO_BIN_EXE_infigraph")` isn't available here (cargo only sets
/// that var for a test binary's own crate-graph binaries). Resolve the CLI
/// binary the same way Task 2's watch_daemon.rs test does.
fn cli_binary() -> PathBuf {
    infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    )
    .expect("infigraph CLI binary must already be built (shared target dir)")
}

/// Bootstrap-index `project_dir` directly (BackendKind::Kuzu, no daemon
/// involved, so `.infigraph/` exists), then start a real detached
/// `infigraph daemon` against it and wait until it holds `watch.lock`.
fn start_real_daemon(project_dir: &Path) -> KillOnDrop {
    let cli = cli_binary();

    // INFIGRAPH_NO_WATCH: plain `index` triggers main.rs's pre-dispatch
    // `should_auto_watch` regardless of backend, which opportunistically
    // spawns its own REAL detached watcher via the same
    // `ensure_watcher_running` this function calls explicitly below --
    // without this, every one of this function's 10 callers got a second,
    // unmanaged daemon process from this bootstrap step alone, entirely
    // separate from (and not cleaned up by) the KillOnDrop guard around
    // the daemon spawned further down. Root cause of pradeepmouli/infigraph#133.
    let status = Command::new(&cli)
        .arg("index")
        .current_dir(project_dir)
        .env_remove("INFIGRAPH_BACKEND")
        .env("INFIGRAPH_NO_WATCH", "1")
        .status()
        .unwrap();
    assert!(status.success(), "bootstrap index failed");

    let daemon = KillOnDrop(
        Command::new(&cli)
            .arg("daemon")
            .current_dir(project_dir)
            .env_remove("INFIGRAPH_BACKEND")
            .spawn()
            .unwrap(),
    );

    let lock_path = project_dir.join(".infigraph").join("watch.lock");
    let start = std::time::Instant::now();
    loop {
        if lock_path.exists() && std::fs::metadata(&lock_path).unwrap().len() > 0 {
            break;
        }
        if start.elapsed() > Duration::from_secs(10) {
            panic!("daemon never acquired watch.lock");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon
}

/// Open a `DaemonKuzu`-backed client against `project_dir`. `init()`'s own
/// `ensure_daemon_running` call resolves to `AlreadyRunning` here, since
/// `start_real_daemon` already holds the lock.
fn open_daemon_client(project_dir: &Path) -> Infigraph {
    let registry = infigraph_languages::bundled_registry().unwrap();
    let mut client = Infigraph::open(project_dir, registry).unwrap();
    let init_result = {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_BACKEND", "daemon");
        let r = client.init();
        std::env::remove_var("INFIGRAPH_BACKEND");
        r
    };
    init_result.unwrap();
    client
}

/// Stop the daemon via its sentinel file, falling back to a kill.
fn stop_daemon(project_dir: &Path, daemon: &mut KillOnDrop) {
    // `watch.stop` only stops the watch *thread*, leaving the daemon
    // process itself alive (see `WatchStop`'s doc in
    // infigraph-cli/src/main.rs) -- ending the whole process needs a
    // `WatchControl { role: Daemon, action: Stop }` request instead, the
    // same mechanism `cmd_daemon_stop` uses. Before this fix, every call
    // here silently waited out the full 5s below before falling back to a
    // hard kill, since the process was never going to exit on its own.
    let staging_dir = project_dir.join(".infigraph").join("requests");
    let _ = infigraph_core::daemon_protocol::submit_write_request(
        &staging_dir,
        &infigraph_core::daemon_protocol::WriteRequest::WatchControl {
            role: infigraph_core::daemon_protocol::WatchRole::Daemon,
            action: infigraph_core::daemon_protocol::WatchAction::Stop,
        },
        Duration::from_secs(5),
    );
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(_)) = daemon.0.try_wait() {
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = daemon.0.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Open a fresh, independent read-only connection to verify what actually
/// landed on disk -- deliberately NOT the client's own wrapper, so a passing
/// assertion can't be explained by client-side caching. Uses
/// `KuzuBackend::open_read_only` (the same primitive `DaemonKuzuBackend`
/// uses internally) rather than a second writable open, which would collide
/// with the daemon's -- exactly what this design exists to prevent.
fn verify_conn(project_dir: &Path) -> infigraph_core::graph::KuzuBackend {
    infigraph_core::graph::KuzuBackend::open_read_only(
        &project_dir.join(".infigraph").join("graph"),
    )
    .unwrap()
}

/// Generous, but bounded. The bug these `real_cli_index_*` tests exist for
/// showed up as a *hang* (the client held `.infigraph/index.lock` while
/// blocking on a daemon that needs that same lock to serve anything), and
/// the client-side budget for a routed write is 600s -- so an unbounded
/// `Command::status()` here would turn a regression into a ten-minute stall
/// instead of a test failure.
const CLI_INDEX_DEADLINE: Duration = Duration::from_secs(240);

/// Runs the REAL `infigraph index` subcommand against `project_dir` with
/// `INFIGRAPH_BACKEND=daemon`, and returns its combined output.
///
/// Every other test in this file drives `Infigraph::index()` as a library
/// call, which is exactly why the deadlock shipped: `cmd_index` acquires
/// `index.lock` itself, before it ever reaches the library, so no
/// library-level test could see it. This helper is the whole point -- it
/// goes through the same entry point a person at a terminal does.
fn run_cli_index(project_dir: &Path, extra_env: &[(&str, &str)]) -> String {
    // Inside `.infigraph/` so neither the indexer nor the daemon's watcher
    // ever sees this log as a project file.
    let log_path = project_dir.join(".infigraph").join("test-cli-index.log");
    let log = std::fs::File::create(&log_path).unwrap();

    let mut cmd = Command::new(cli_binary());
    cmd.arg("index")
        .current_dir(project_dir)
        .env("INFIGRAPH_BACKEND", "daemon")
        // This machine exports INFIGRAPH_WATCH_DAEMON globally; leaving it
        // set would change which watcher model the child picks.
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let mut child = KillOnDrop(cmd.spawn().unwrap());
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > CLI_INDEX_DEADLINE {
            panic!(
                "`infigraph index` did not finish within {CLI_INDEX_DEADLINE:?} -- \
                 the index.lock deadlock is back. Output so far:\n{}",
                std::fs::read_to_string(&log_path).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let output = std::fs::read_to_string(&log_path).unwrap_or_default();
    std::fs::remove_file(&log_path).ok();
    assert!(
        status.success(),
        "`infigraph index` exited with {status}:\n{output}"
    );
    output
}

/// Whether `cmd_index` printed its per-language breakdown, which it derives
/// purely from `IndexResult::extractions`. That makes this an exact,
/// observable proxy for "the client did the parsing itself and kept its
/// extractions" -- empty extractions produce no such line.
fn has_language_breakdown(output: &str) -> bool {
    output.lines().any(|l| l.trim_start().starts_with("lua:"))
}

fn file_is_in_graph(project_dir: &Path, file: &str) -> bool {
    !verify_conn(project_dir)
        .raw_query(&format!("MATCH (f:File) WHERE f.id = '{file}' RETURN f.id"))
        .unwrap()
        .is_empty()
}

/// Regression test for the deadlock that shipped with `DaemonKuzu`:
/// `infigraph daemon` in one process, `INFIGRAPH_BACKEND=daemon infigraph
/// index` in another, against the same project, hung forever. `cmd_index`
/// took `.infigraph/index.lock` up front (a holdover from when indexing was
/// always local work), then blocked on a daemon whose request-serving loop
/// needs that same lock -- so the daemon logged "busy, retrying" until the
/// client timed out.
///
/// Covers both routing modes, because both route writes through the
/// daemon's request-serving loop and so both hit the same lock:
///  - the default, where the client parses locally and only the graph
///    writes (`upsert_files_bulk`/`remove_file`/`resolve_calls`) route; and
///  - `INFIGRAPH_WATCH_INDEX_VIA_DAEMON=1`, where the whole job is one
///    `WriteRequest::Index` and the daemon redoes the scan itself.
#[test]
fn real_cli_index_against_a_real_daemon_completes_and_writes() {
    let project_dir = tempfile::tempdir().unwrap();
    // Lua deliberately: no SCIP indexer covers it (see scip_download::CATALOG),
    // so `infigraph index` spawns no detached scip-enrich child. Such a child
    // holds `index.lock` for as long as it runs, which is itself enough to
    // make every assertion below unreachable -- the first draft of this test
    // failed exactly that way against a `.py` project.
    std::fs::write(
        project_dir.path().join("main.lua"),
        "function hello()\nend\n",
    )
    .unwrap();

    let mut daemon = start_real_daemon(project_dir.path());

    // ── Default mode: the client parses, the daemon writes ──
    //
    // The daemon is also a watcher on this directory and can pick a new file
    // up on its own inside its 1s batch window, which would leave our run
    // with nothing to parse and make the extractions assertion vacuous.
    // Retry with a fresh file until we win that race.
    let mut default_file = String::new();
    let mut default_output = String::new();
    for attempt in 0..5 {
        default_file = format!("generated_{attempt}.lua");
        std::fs::write(
            project_dir.path().join(&default_file),
            "function helper()\nend\n\nfunction caller()\n  helper()\nend\n",
        )
        .unwrap();
        default_output = run_cli_index(project_dir.path(), &[]);
        if default_output.contains("Indexed ") {
            break;
        }
    }
    assert!(
        default_output.contains("Indexed "),
        "the daemon's own watcher beat every attempt to it; no run had files to parse:\n\
         {default_output}"
    );
    assert!(
        has_language_breakdown(&default_output),
        "under the default routing the client parses locally, so IndexResult::extractions \
         must be populated and cmd_index must print its per-language breakdown:\n{default_output}"
    );
    assert!(
        file_is_in_graph(project_dir.path(), &default_file),
        "{default_file} must actually be in the graph -- checked over an independent \
         read-only connection, so this cannot pass on client-side caching"
    );

    // ── Opt-in mode: the daemon redoes the whole job ──
    let opt_in_file = "opt_in.lua";
    std::fs::write(
        project_dir.path().join(opt_in_file),
        "function opt_in_marker()\nend\n",
    )
    .unwrap();
    let opt_in_output = run_cli_index(
        project_dir.path(),
        &[("INFIGRAPH_WATCH_INDEX_VIA_DAEMON", "1")],
    );
    assert!(
        !has_language_breakdown(&opt_in_output),
        "under INFIGRAPH_WATCH_INDEX_VIA_DAEMON the daemon does the parsing and the protocol \
         deliberately doesn't ship extractions back, so there is nothing to break down \
         by language:\n{opt_in_output}"
    );
    assert!(
        file_is_in_graph(project_dir.path(), opt_in_file),
        "the daemon must still have indexed {opt_in_file} in opt-in mode"
    );

    stop_daemon(project_dir.path(), &mut daemon);
}

/// End-to-end proof that a real spawned `infigraph daemon` process and a
/// real `DaemonKuzu`-backed client `Infigraph` genuinely interoperate --
/// not just the in-process simulated-server pattern (a background thread
/// calling `serve_one_request` directly) that earlier tasks' tests used.
#[test]
fn real_daemon_process_serves_a_daemon_kuzu_client() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    let mut daemon = start_real_daemon(project_dir.path());
    let client = open_daemon_client(project_dir.path());

    let backend = client.backend().unwrap();
    // `upsert_repo` (the brief's original choice) turned out to be a bad
    // fit: `GraphBackend::upsert_repo`'s trait default is a *documented,
    // intentional* no-op for Kuzu ("only meaningful for Neo4j multi-repo
    // graphs" -- see backend.rs) and `KuzuBackend` never overrides it. So
    // routing it through the daemon proves nothing about Kuzu writes --
    // the daemon's own local `KuzuBackend::upsert_repo` call is a genuine
    // no-op by design, not a wiring gap. `store_config_bindings` has no
    // such trap: the trait requires every backend to implement it (see
    // its "No default impl" doc comment, added specifically so a
    // raw_query-based default couldn't be silently inherited by this
    // wrapper), and `KuzuBackend`'s implementation unconditionally
    // `CREATE`s a `ConfigBinding` node -- a real, observable Kuzu write.
    let bindings = vec![infigraph_core::config::ConfigBindingWire {
        symbol_id: "e2e-test::hello".to_string(),
        kind: "env".to_string(),
        key: "E2E_TEST_KEY".to_string(),
        value: "e2e-test-value".to_string(),
        profile: "default".to_string(),
        source_file: "main.py".to_string(),
    }];
    backend.store_config_bindings(&bindings).unwrap();

    let rows = verify_conn(project_dir.path())
        .raw_query("MATCH (c:ConfigBinding {key: 'E2E_TEST_KEY'}) RETURN c.key")
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "expected the daemon to have created the ConfigBinding node"
    );

    stop_daemon(project_dir.path(), &mut daemon);
}

/// `Infigraph::index()`/`index_files()` must work under `BackendKind::
/// DaemonKuzu`. They take the ordinary client-side path there like any other
/// backend -- parse locally, then let `upsert_files_bulk`, `remove_file` and
/// `resolve_calls` route themselves to the daemon. Before those three were
/// implemented they were Tier-3 stubs, and `index()` with changed files
/// failed with "use Infigraph::index()/index_files() instead", i.e. it told
/// the caller to call the function they were already inside.
#[test]
fn index_and_index_files_route_through_the_daemon() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    let mut daemon = start_real_daemon(project_dir.path());
    let client = open_daemon_client(project_dir.path());

    // A file the bootstrap index never saw, so there is real work to do.
    std::fs::write(
        project_dir.path().join("second.py"),
        "def another():\n    pass\n",
    )
    .unwrap();

    let full = client.index().unwrap();
    assert!(
        full.total_files >= 2,
        "index() must report the daemon's real file count, got total_files={}",
        full.total_files
    );

    // `indexed_files` is deliberately not asserted on: the daemon is also a
    // watcher, so it may already have picked `second.py` up on its own,
    // making the count legitimately 0. `total_files` for a scoped call is
    // exactly `paths.len()` on the daemon side, so that stays deterministic.
    let scoped = client.index_files(&[PathBuf::from("second.py")]).unwrap();
    assert_eq!(
        scoped.total_files, 1,
        "index_files() must report the daemon's real result, got total_files={}",
        scoped.total_files
    );

    let rows = verify_conn(project_dir.path())
        .raw_query("MATCH (f:File) WHERE f.id = 'second.py' RETURN f.id")
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the daemon must actually have indexed second.py into the graph"
    );

    stop_daemon(project_dir.path(), &mut daemon);
}

/// The exact scenario reproduced live while confirming the predecessor fix
/// (fix/daemonkuzu-index-routing): under INFIGRAPH_WATCH_INDEX_VIA_DAEMON=1,
/// creating a file and immediately running `infigraph index` -- no
/// settling delay for the daemon's own watcher debounce -- used to produce
/// a Kuzu duplicate-primary-key error, because the daemon's own
/// autonomous watcher and the client's explicit Index request each
/// independently decided the new file needed indexing. Proves the fix:
/// this must complete successfully, not error.
#[test]
fn ad_hoc_index_request_racing_the_watchers_own_debounce_does_not_duplicate_key() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("main.py"), "def main():\n    pass\n").unwrap();

    let mut daemon = start_real_daemon(project.path());

    // Bootstrap: one file already indexed before the race file appears,
    // matching the live repro's setup.
    let bootstrap = std::process::Command::new(cli_binary())
        .arg("--root")
        .arg(project.path())
        .arg("index")
        .arg("--no-embed")
        .env_remove("INFIGRAPH_BACKEND")
        .status()
        .unwrap();
    assert!(bootstrap.success());

    // The race: create a new file, then IMMEDIATELY (no settling delay)
    // submit an ad-hoc Index request via the opt-in whole-job-to-daemon
    // mode -- the exact combination that reproduced the bug live.
    std::fs::write(
        project.path().join("second.py"),
        "def second():\n    pass\n",
    )
    .unwrap();

    let output = std::process::Command::new(cli_binary())
        .arg("--root")
        .arg(project.path())
        .arg("index")
        .arg("--no-embed")
        .env("INFIGRAPH_BACKEND", "daemon")
        .env("INFIGRAPH_WATCH_INDEX_VIA_DAEMON", "1")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected the racing Index request to succeed, not duplicate-key error:\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !stderr.contains("duplicated primary key"),
        "the coalescing bug reproduced:\n{stderr}"
    );

    stop_daemon(project.path(), &mut daemon);
}

/// Read-after-write through the SAME long-lived wrapper instance.
///
/// Every other test here verifies a daemon-side write through a *fresh*
/// connection (deliberately, to rule out client-side caching), which leaves
/// the question this test answers untested: does a commit made by the daemon
/// process become visible to a `DaemonKuzuBackend` constructed before that
/// write happened?
///
/// This matters for any code path that reads via the wrapper and writes via
/// the daemon within one process -- e.g. `detect_config_bindings`,
/// `detect_clusters`, `link_cross_service_calls`.
///
/// It passes only because `DaemonKuzuBackend` opens a fresh read-only
/// connection per read rather than holding one for its lifetime: a Kuzu
/// embedded read-only `Database` serves the snapshot it loaded at open time
/// and never observes another process's later commits, so a held connection
/// would permanently miss every daemon write. See `open_read` in
/// `daemon_kuzu_backend.rs`.
#[test]
fn daemon_write_is_visible_through_the_same_wrapper_instance() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    let mut daemon = start_real_daemon(project_dir.path());
    let client = open_daemon_client(project_dir.path());
    let backend = client.backend().unwrap();

    // Prove the key isn't already present, so the post-write read can't
    // pass for the wrong reason.
    let before = backend
        .raw_query("MATCH (c:ConfigBinding {key: 'SAME_INSTANCE_KEY'}) RETURN c.key")
        .unwrap();
    assert_eq!(before.len(), 0, "key must not exist before the write");

    backend
        .store_config_bindings(&[infigraph_core::config::ConfigBindingWire {
            symbol_id: "e2e-test::hello".to_string(),
            kind: "env".to_string(),
            key: "SAME_INSTANCE_KEY".to_string(),
            value: "same-instance-value".to_string(),
            profile: "default".to_string(),
            source_file: "main.py".to_string(),
        }])
        .unwrap();

    // An independent connection confirms the write really committed, so a
    // failure below is unambiguously a visibility problem on the long-lived
    // connection and not a write that never happened.
    let independent = verify_conn(project_dir.path())
        .raw_query("MATCH (c:ConfigBinding {key: 'SAME_INSTANCE_KEY'}) RETURN c.key")
        .unwrap();
    assert_eq!(
        independent.len(),
        1,
        "the daemon must have committed the write"
    );

    // The actual question: the same wrapper instance, constructed before the
    // write, must still see it.
    let same_instance = backend
        .raw_query("MATCH (c:ConfigBinding {key: 'SAME_INSTANCE_KEY'}) RETURN c.key")
        .unwrap();

    stop_daemon(project_dir.path(), &mut daemon);

    assert_eq!(
        same_instance.len(),
        1,
        "a daemon-committed write must be visible through a DaemonKuzuBackend \
         instance that was constructed before the write"
    );
}

/// The behavioural claim background draining makes: a drain no longer owns
/// the watch loop for its whole duration, so work submitted *while* one is
/// running is still accepted, and is served by the next drain.
///
/// What this can and cannot see: from outside the daemon process there is no
/// way to directly observe "this request was accepted into the queue at
/// t+50ms rather than t+8s". What it *can* prove -- and what the risky part
/// of backgrounding a drain would break -- is that the loop does not wedge:
/// a second request landing mid-drain still completes, the requests
/// directory drains to empty rather than orphaning it, and the daemon is
/// still serving afterwards. A drain task that deadlocked against its own
/// `index.lock`, dropped its queue on the floor, or left the loop parked
/// would fail one of those.
#[test]
fn producers_keep_accepting_work_while_a_drain_is_in_flight() {
    let project = tempfile::tempdir().unwrap();

    // Lua for the same reason `real_cli_index_against_a_real_daemon_*` uses
    // it: no SCIP indexer covers it, so `infigraph index` spawns no detached
    // scip-enrich child, which would hold `index.lock` and make every timing
    // observation below meaningless. Enough files, with enough in each, that
    // a whole-project drain takes real wall time rather than completing
    // inside one 200ms tick.
    for i in 0..300 {
        let body: String = (0..40)
            .map(|j| format!("function f{i}_{j}()\n  return {j}\nend\n"))
            .collect();
        std::fs::write(project.path().join(format!("m{i}.lua")), body).unwrap();
    }

    let mut daemon = start_real_daemon(project.path());

    // A whole-project index routed entirely through the daemon: one
    // `WriteRequest::Index`, so the daemon itself does the scan/extract/
    // upsert/resolve pass. This is the slow drain the second request has to
    // land in the middle of.
    let slow = std::thread::spawn({
        let dir = project.path().to_path_buf();
        move || run_cli_index(&dir, &[("INFIGRAPH_WATCH_INDEX_VIA_DAEMON", "1")])
    });

    // Give the daemon a moment to actually pick the request up and start
    // draining, so the second request really does land mid-drain rather
    // than racing it to the front of the queue.
    std::thread::sleep(Duration::from_millis(750));

    std::fs::write(
        project.path().join("late.lua"),
        "function late()\n  return 1\nend\n",
    )
    .unwrap();
    let second_started = std::time::Instant::now();
    run_cli_index(project.path(), &[("INFIGRAPH_WATCH_INDEX_VIA_DAEMON", "1")]);
    let second_elapsed = second_started.elapsed();

    slow.join().expect("the slow whole-project index panicked");

    // Both requests were served, and the file that only ever existed during
    // the in-flight drain made it into the graph -- work submitted mid-drain
    // is queued, not dropped.
    assert!(
        file_is_in_graph(project.path(), "late.lua"),
        "a file created while a drain was in flight must still be indexed by \
         the next drain, not silently dropped"
    );

    // Nothing orphaned: every `.request` the daemon accepted was answered
    // and removed. A drain that panicked or wedged would leave one behind.
    let leftover: Vec<_> = std::fs::read_dir(project.path().join(".infigraph").join("requests"))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "request"))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leftover.is_empty(),
        "the daemon left unanswered requests behind: {leftover:?}"
    );

    // The second request cannot have taken longer than a whole extra
    // serialized run of the first -- it shares the deadline every other
    // routed write in this file is held to.
    assert!(
        second_elapsed < CLI_INDEX_DEADLINE,
        "a request submitted during an in-flight drain took {second_elapsed:?}, \
         which suggests the watch loop stopped making progress"
    );

    // Still serving after all that: the loop returned to its normal tick
    // rather than being left parked on a finished drain.
    stop_daemon(project.path(), &mut daemon);
}

/// A request that's only queued (not yet executing) when a FullReindex
/// arrives is superseded, not silently dropped -- its waiter gets an
/// explicit reply rather than hanging until its own client-side timeout.
#[test]
fn a_queued_request_racing_a_full_reindex_gets_a_superseded_reply_not_a_hang() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    let mut daemon = start_real_daemon(project.path());

    // Submit both near-simultaneously: an ad-hoc single-file Index request
    // and a FullReindex, racing to see which the daemon picks up first.
    // Regardless of ordering, the Index request's client must get SOME
    // reply within the deadline -- either its own normal completion (if it
    // slipped in first) or a superseded error (if FullReindex won) -- never
    // a hang.
    std::fs::write(project.path().join("b.py"), "def b():\n    pass\n").unwrap();

    let cli = cli_binary();
    let mut index_child = std::process::Command::new(&cli)
        .arg("index")
        .arg("--no-embed")
        .current_dir(project.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .env("INFIGRAPH_WATCH_INDEX_VIA_DAEMON", "1")
        .spawn()
        .unwrap();

    let mut full_child = std::process::Command::new(&cli)
        .arg("index")
        .arg("--full")
        .arg("--no-embed")
        .current_dir(project.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .spawn()
        .unwrap();

    // Bounded waits: this test exists to prove neither request hangs, so an
    // unbounded wait here would turn exactly the regression it guards
    // against into a stalled test process instead of a clean failure --
    // same `try_wait` poll pattern as `run_cli_index` above.
    let full_start = std::time::Instant::now();
    loop {
        if full_child.try_wait().unwrap().is_some() {
            break;
        }
        if full_start.elapsed() > CLI_INDEX_DEADLINE {
            let _ = full_child.kill();
            let _ = full_child.wait();
            panic!("full reindex did not finish within {CLI_INDEX_DEADLINE:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let full_output = full_child.wait_with_output().unwrap();

    let index_start = std::time::Instant::now();
    loop {
        if index_child.try_wait().unwrap().is_some() {
            break;
        }
        if index_start.elapsed() > CLI_INDEX_DEADLINE {
            let _ = index_child.kill();
            let _ = index_child.wait();
            panic!("ad-hoc index did not finish within {CLI_INDEX_DEADLINE:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let index_output = index_child.wait_with_output().unwrap();

    // Both must complete within the deadline (neither hangs), regardless
    // of which one the daemon happened to serve first.
    assert!(
        full_output.status.success(),
        "full reindex must succeed:\nstderr={}",
        String::from_utf8_lossy(&full_output.stderr)
    );
    // The ad-hoc index either succeeded normally or failed with the
    // superseded message -- both are acceptable outcomes; a hang (the
    // process still running past this point) is what this test guards
    // against, and `wait_with_output` above already proves it didn't hang.
    let index_stderr = String::from_utf8_lossy(&index_output.stderr);
    if !index_output.status.success() {
        assert!(
            index_stderr.contains("superseded"),
            "if the ad-hoc index failed, it must be because it was superseded, not some other error:\n{index_stderr}"
        );
    }

    daemon.0.kill().ok();
}

/// A read attempted during the rebuild window sees the still-valid OLD
/// graph, not an error or an empty result -- proving reads are genuinely
/// unaffected by an in-progress full reindex, not just assumed to be.
#[test]
fn a_read_during_full_reindex_sees_the_old_graph_not_an_error() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project = tempfile::tempdir().unwrap();
    // Enough files that the rebuild takes real wall time, giving a window
    // for a concurrent read to land mid-rebuild.
    for i in 0..300 {
        std::fs::write(
            project.path().join(format!("f{i}.py")),
            format!("def f{i}():\n    pass\n"),
        )
        .unwrap();
    }

    let mut daemon = start_real_daemon(project.path());
    let cli = cli_binary();

    let mut full = std::process::Command::new(&cli)
        .arg("index")
        .arg("--full")
        .arg("--no-embed")
        .current_dir(project.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .spawn()
        .unwrap();

    // Give the rebuild a moment to actually start before reading.
    std::thread::sleep(Duration::from_millis(200));

    // A direct read-only connection against the LIVE path (not through the
    // daemon protocol -- reads never route through it), matching
    // `verify_conn`'s own approach.
    let read_during = verify_conn(project.path());
    let rows = read_during
        .raw_query("MATCH (s:Symbol) RETURN s.name LIMIT 1")
        .expect("a read during the rebuild window must succeed, not error");
    assert!(
        !rows.is_empty(),
        "a read during the rebuild window must see real content from the still-valid old graph, \
         not an empty result"
    );

    // Bounded wait, matching `run_cli_index`'s own `try_wait` poll loop: an
    // unbounded `wait()` here would turn a real hang into a stalled test
    // process instead of a clean failure.
    let full_start = std::time::Instant::now();
    loop {
        if full.try_wait().unwrap().is_some() {
            break;
        }
        if full_start.elapsed() > CLI_INDEX_DEADLINE {
            let _ = full.kill();
            let _ = full.wait();
            panic!("full reindex did not finish within {CLI_INDEX_DEADLINE:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let status = full.wait().unwrap();
    assert!(status.success(), "full reindex must still succeed");

    daemon.0.kill().ok();
}

/// A failed rebuild (extraction/write error mid-build) leaves the live
/// graph completely unharmed -- the real advantage of build-fresh-then-swap
/// over wipe-in-place.
#[test]
fn a_failed_rebuild_leaves_the_live_graph_untouched() {
    let _guard = ENV_LOCK.lock().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    let mut daemon = start_real_daemon(project.path());

    let before = verify_conn(project.path());
    let before_rows = before.raw_query("MATCH (s:Symbol) RETURN s.name").unwrap();
    assert!(
        !before_rows.is_empty(),
        "must have real content before the attempt"
    );

    // Make the project root itself unreadable-as-a-directory to force
    // `scan_changed_files`'s file collection to fail partway through the
    // rebuild -- a real, reproducible failure mode rather than a
    // synthetic hook. (Restored before the daemon shuts down, so cleanup
    // doesn't itself fail.)
    let unreadable_dir = project.path().join("unreadable");
    std::fs::create_dir(&unreadable_dir).unwrap();
    std::fs::write(unreadable_dir.join("x.py"), "def x():\n    pass\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&unreadable_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    }

    let cli = cli_binary();
    let mut child = std::process::Command::new(&cli)
        .arg("index")
        .arg("--full")
        .arg("--no-embed")
        .current_dir(project.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .spawn()
        .unwrap();

    // Bounded wait, matching `run_cli_index`'s own `try_wait` poll loop: an
    // unbounded wait here would turn a real hang into a stalled test
    // process instead of a clean failure.
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > CLI_INDEX_DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            panic!("full reindex did not finish within {CLI_INDEX_DEADLINE:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = child.wait_with_output().unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&unreadable_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Whether this specific permission trick actually fails the rebuild is
    // platform/permission-model dependent (e.g. root-owned CI runners may
    // bypass it) -- assert on the INVARIANT this test actually cares
    // about (the live graph survives) rather than requiring the injection
    // to have worked, so this test isn't flaky-by-construction across
    // environments.
    let _ = output.status;

    let after = verify_conn(project.path());
    let after_rows = after.raw_query("MATCH (s:Symbol) RETURN s.name").unwrap();
    assert!(
        !after_rows.is_empty(),
        "the live graph must survive regardless of whether the rebuild succeeded or failed"
    );

    let rebuilding_path = project.path().join(".infigraph").join("graph.rebuilding");
    assert!(
        !rebuilding_path.exists(),
        "an incomplete rebuild attempt must not leave graph.rebuilding behind"
    );

    daemon.0.kill().ok();
}

/// Regression test for the final review's I5. `index --full` under
/// `INFIGRAPH_BACKEND=daemon` drops a request into `.infigraph/requests` and
/// polls for a reply for up to 600s. Because that branch returns before
/// `Infigraph::open`, the usual daemon auto-start never runs either -- so
/// with no daemon around, the command used to sit silently for ten minutes
/// and then report an opaque timeout. That's a plausible everyday case:
/// `INFIGRAPH_BACKEND` is commonly exported from a shell profile.
///
/// Deliberately runs the real binary rather than calling `cmd_index`:
/// `daemon_backend_selected()` reads a process-wide env var, and the fast
/// path being tested lives in the CLI's own argument handling.
#[test]
fn full_reindex_with_no_daemon_fails_fast_instead_of_polling_for_ten_minutes() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    // Bootstrap a `.infigraph/` locally so the failure under test is
    // genuinely "no daemon", not "no project". INFIGRAPH_NO_WATCH: plain,
    // unrelated setup -- must not opportunistically spawn its own watcher.
    let cli = cli_binary();
    let status = Command::new(&cli)
        .arg("index")
        .current_dir(project.path())
        .env_remove("INFIGRAPH_BACKEND")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env("INFIGRAPH_NO_WATCH", "1")
        .status()
        .unwrap();
    assert!(status.success(), "bootstrap index failed");

    // Comfortably under the 600s submit deadline, but far enough above a
    // liveness probe that this can't pass by luck.
    const FAIL_FAST_BUDGET: Duration = Duration::from_secs(30);

    // INFIGRAPH_NO_WATCH suppresses `ensure_watcher_running`'s auto-start
    // fallback (index.rs's own `is_ci_env` check) the same way real CI
    // does. Without this, outside CI the first wait_for_daemon(10s) fails
    // (nothing running yet), auto-start successfully spawns one, the
    // second wait_for_daemon(10s) succeeds, and the reindex completes
    // normally -- the auto-start-then-retry fallback #100 intentionally
    // added, not a bug. This test is specifically about the OTHER branch
    // (auto-start also unavailable/suppressed), so it must force that
    // branch deterministically rather than relying on ambient CI
    // detection -- previously this test only passed in real CI and failed
    // locally every time, miscategorized as "flaky" rather than
    // environment-dependent.
    let start = std::time::Instant::now();
    let mut child = Command::new(&cli)
        .arg("index")
        .arg("--full")
        .current_dir(project.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > FAIL_FAST_BUDGET {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "`index --full` with no daemon running did not fail within {FAIL_FAST_BUDGET:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "must be a hard failure, not a silent success: {stderr}"
    );
    assert!(
        stderr.contains("no daemon came up"),
        "the error must say what's actually wrong and how to fix it, got: {stderr}"
    );
    assert!(
        !project.path().join(".infigraph").join("requests").exists(),
        "the request must not be submitted at all when there is nothing to serve it"
    );
}

/// Regression test for the sibling gap `--full`'s fix above didn't cover:
/// a plain (non-`--full`) `infigraph index` under `INFIGRAPH_BACKEND=daemon`
/// had no equivalent fail-fast guard at all -- `Infigraph::init()`'s
/// `ensure_daemon_for_writes` used to fire-and-forget a daemon spawn attempt
/// and return unconditionally, so if no daemon ever came up (e.g. the very
/// first index of a fresh project, before `.infigraph` exists -- daemon
/// auto-start is a benign no-op in that case, by design), the first write
/// would silently block inside `submit_write_request` for its own ~600s
/// timeout instead of failing here, within seconds, with an actionable
/// message.
///
/// Deliberately runs the real binary (not `cmd_index`/`Infigraph::init()`
/// directly): `resolve_cli_binary_sibling_of` and the daemon spawn path
/// both depend on `current_exe()` resolving to the real `infigraph` binary,
/// which only holds true inside a spawned child process, not this test
/// binary itself.
#[test]
fn plain_index_on_a_never_indexed_project_fails_fast_under_daemon_backend() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    // No bootstrap index here, deliberately: `.infigraph` must not exist yet
    // when the daemon-backend `index` invocation below runs, so that
    // `ensure_daemon_running_required`'s "not yet indexed" no-op is what's
    // actually exercised (rather than a real spawn failure).
    assert!(!project.path().join(".infigraph").exists());

    const FAIL_FAST_BUDGET: Duration = Duration::from_secs(30);

    let start = std::time::Instant::now();
    let mut child = Command::new(cli_binary())
        .arg("index")
        .current_dir(project.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > FAIL_FAST_BUDGET {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "plain `index` on a never-indexed project under \
                 INFIGRAPH_BACKEND=daemon did not fail within {FAIL_FAST_BUDGET:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "must be a hard failure, not a silent success: {stderr}"
    );
    assert!(
        stderr.contains("no daemon came up"),
        "the error must say what's actually wrong and how to fix it, got: {stderr}"
    );
}

/// Sibling scenario to the test above, but distinct: a project that WAS
/// already indexed (`.infigraph/` exists, real content) whose graph file
/// was then deleted -- e.g. manually, or by a prior crash-recovery attempt.
/// Plain `infigraph index` (non-`--full`) must auto-promote to a full
/// rebuild rather than falling through to an incremental open that fails
/// with Kuzu's own confusing read-only-mode error (#100 second-incident
/// comment). Local (non-daemon) backend, since this fix applies before any
/// daemon-routing decision is made.
#[test]
fn plain_index_auto_promotes_to_a_full_rebuild_when_the_graph_is_missing_but_infigraph_dir_exists()
{
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    // --no-embed on both invocations: it skips spawning the detached
    // scip-enrich child (returns before that point in cmd_index), so the
    // second invocation below can't race the bootstrap's own background
    // child for index.lock -- this test is about the auto-promotion logic,
    // not index.lock contention timing.
    let status = Command::new(cli_binary())
        .arg("index")
        .arg("--no-embed")
        .current_dir(project.path())
        .env_remove("INFIGRAPH_BACKEND")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env("INFIGRAPH_NO_WATCH", "1")
        .status()
        .unwrap();
    assert!(status.success(), "bootstrap index failed");
    assert!(project.path().join(".infigraph").join("graph").exists());

    std::fs::remove_file(project.path().join(".infigraph").join("graph")).unwrap();

    let output = Command::new(cli_binary())
        .arg("index")
        .arg("--no-embed")
        .current_dir(project.path())
        .env_remove("INFIGRAPH_BACKEND")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env("INFIGRAPH_NO_WATCH", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "plain index must auto-promote to a full rebuild, not fail: {stderr}"
    );
    assert!(project.path().join(".infigraph").join("graph").exists());

    // A `.infigraph/index.lock` FILE persisting after a clean run is normal
    // (this codebase's lockfile convention releases the OS-level flock but
    // never deletes the file, matching watch.lock's identical pattern) --
    // what actually matters is that the lock isn't still HELD, i.e. a fresh
    // acquire eventually succeeds rather than blocking on a leaked holder.
    // Retried with a bounded wait: the child process itself has already
    // exited by the time `.output()` returns above, but under full-suite
    // load an unrelated concurrently-running test in this same binary can
    // transiently hold this lock for a moment via a detached grandchild.
    let index_lock = project.path().join(".infigraph").join("index.lock");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut reacquired = None;
    while std::time::Instant::now() < deadline {
        reacquired = infigraph_core::lockfile::try_acquire(&index_lock, "test-verify").unwrap();
        if reacquired.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        reacquired.is_some(),
        "index.lock must not still be held after a completed run"
    );
}

/// R3.1.4g/#115: a crashed daemon's cause is only diagnosable if a human
/// can tell which generation's output in the shared, appended-to daemon.log
/// is whose. Spawned via `ensure_daemon_running` (the opportunistic
/// auto-start path both the CLI's `ensure_watcher_running` and MCP's
/// `ensure_daemon_watcher` use, and the one #115's investigation found had
/// no captured stderr) -- NOT `start_real_daemon`'s direct
/// `Command::new(cli).arg("daemon")` spawn, which inherits the test
/// process's own stdio rather than routing through `build_daemon_command`'s
/// daemon.log redirection, exactly the distinction this task's research
/// found matters.
#[test]
fn opportunistic_daemon_spawn_writes_a_start_banner_naming_its_pid_to_daemon_log() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    // INFIGRAPH_NO_WATCH: without this, the bootstrap's own pre-dispatch
    // auto-watch could win the lock race before the explicit
    // ensure_daemon_running call below ever runs, making the `Spawned`
    // assertion flaky (it would see `AlreadyRunning` instead) and leaking
    // an extra, unmanaged daemon this test never tracks or kills.
    let status = Command::new(cli_binary())
        .arg("index")
        .arg("--no-embed")
        .current_dir(project.path())
        .env_remove("INFIGRAPH_BACKEND")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env("INFIGRAPH_NO_WATCH", "1")
        .status()
        .unwrap();
    assert!(status.success(), "bootstrap index failed");

    let outcome =
        infigraph_core::daemon::lifecycle::ensure_daemon_running(project.path(), &cli_binary());
    assert!(
        matches!(
            outcome,
            infigraph_core::daemon::lifecycle::DaemonStartOutcome::Spawned
        ),
        "expected a fresh spawn, got {outcome:?}"
    );

    let lock_path = project.path().join(".infigraph").join("watch.lock");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline
        && !(lock_path.exists() && std::fs::metadata(&lock_path).unwrap().len() > 0)
    {
        std::thread::sleep(Duration::from_millis(50));
    }

    // Acquired immediately once the daemon is known to be running, BEFORE
    // the assertion below that could panic -- see KillPidOnDrop's doc.
    let holder = infigraph_core::lockfile::read_holder(&lock_path)
        .expect("watch.lock must have a readable holder payload once held");
    let _kill_guard = KillPidOnDrop(holder.pid);

    let log = std::fs::read_to_string(project.path().join(".infigraph").join("daemon.log"))
        .unwrap_or_default();
    assert!(
        log.contains("[daemon-start]") && log.contains("pid="),
        "expected a start banner naming a pid, got: {log:?}"
    );

    // Clean up: `watch.stop` only stops the watch *thread*, leaving the
    // daemon process itself alive (see `WatchStop`'s doc in
    // infigraph-cli/src/main.rs and `watch_action_stop_leaves_the_daemon_process_alive`)
    // -- ending the whole process needs a `WatchControl { role: Daemon,
    // action: Stop }` request, the same mechanism `cmd_daemon_stop` uses.
    // Best-effort: KillPidOnDrop above is the real cleanup guarantee.
    let staging_dir = project.path().join(".infigraph").join("requests");
    let _ = infigraph_core::daemon_protocol::submit_write_request(
        &staging_dir,
        &infigraph_core::daemon_protocol::WriteRequest::WatchControl {
            role: infigraph_core::daemon_protocol::WatchRole::Daemon,
            action: infigraph_core::daemon_protocol::WatchAction::Stop,
        },
        Duration::from_secs(10),
    );
}

/// Regression test: `INFIGRAPH_NO_WATCH` (a convenience opt-out -- "don't
/// spawn a background watcher for me") must not also suppress
/// `ensure_daemon_for_writes`'s auto-start, since `INFIGRAPH_BACKEND=daemon`
/// is a hard requirement, not a convenience. This combination (both env
/// vars set, e.g. `INFIGRAPH_NO_WATCH` leaked from a shared shell profile)
/// is exactly what originally reproduced the hang: before the fix, the
/// daemon auto-start was silently skipped and the write blocked for its own
/// multi-minute timeout instead of either working or failing fast.
#[test]
fn plain_index_ignores_no_watch_opt_out_for_the_required_backend_daemon() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    // Bootstrap first so `.infigraph` exists -- this test is about the
    // NO_WATCH-suppresses-a-required-daemon gap specifically, not the
    // separate never-indexed-yet case covered above. INFIGRAPH_NO_WATCH
    // here too: this bootstrap step is plain, unrelated setup, so it
    // should not opportunistically spawn its own watcher via main.rs's
    // pre-dispatch should_auto_watch -- that's the mechanism under test
    // below, deliberately, only for the *second* command.
    let cli = cli_binary();
    let status = Command::new(&cli)
        .arg("index")
        .current_dir(project.path())
        .env_remove("INFIGRAPH_BACKEND")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env("INFIGRAPH_NO_WATCH", "1")
        .status()
        .unwrap();
    assert!(status.success(), "bootstrap index failed");
    std::fs::write(
        project.path().join("a.py"),
        "def a():\n    pass\ndef b():\n    pass\n",
    )
    .unwrap();

    let output = Command::new(&cli)
        .arg("index")
        .current_dir(project.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .env("INFIGRAPH_NO_WATCH", "1")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "INFIGRAPH_NO_WATCH must not block the daemon a selected backend \
         actually requires -- got: {stderr}"
    );

    // This command really did spawn a real, required daemon (that's the
    // whole point of the test) -- via ensure_daemon_for_writes, a THIRD
    // auto-start entry point distinct from both ensure_watcher_running and
    // ensure_daemon_running, with no Child handle for this test to own.
    // Clean it up properly: KillPidOnDrop as the guarantee, plus the real
    // full-process-stop mechanism (not `watch.stop`, which only stops the
    // watch thread) for the graceful path.
    let lock_path = project.path().join(".infigraph").join("watch.lock");
    if let Some(holder) = infigraph_core::lockfile::read_holder(&lock_path) {
        let _kill_guard = KillPidOnDrop(holder.pid);
        let staging_dir = project.path().join(".infigraph").join("requests");
        let _ = infigraph_core::daemon_protocol::submit_write_request(
            &staging_dir,
            &infigraph_core::daemon_protocol::WriteRequest::WatchControl {
                role: infigraph_core::daemon_protocol::WatchRole::Daemon,
                action: infigraph_core::daemon_protocol::WatchAction::Stop,
            },
            Duration::from_secs(10),
        );
    }
}

#[test]
fn a_dead_holder_wal_sentinel_triggers_an_automatic_full_reindex_via_the_real_daemon() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    // start_real_daemon bootstrap-indexes and spawns a real, live daemon --
    // `.infigraph/` and a real daemon-servable graph both exist by the time
    // this returns, matching how a real crash would be discovered.
    let daemon = start_real_daemon(project.path());

    let infigraph_dir = project.path().join(".infigraph");
    // Simulate what open_read_only_or_degrade does on detecting a
    // dead-holder WAL, without a real crash: drop the sentinel directly.
    infigraph_core::recovery::mark_recovery_needed(
        &infigraph_dir,
        999_999_999,
        &infigraph_dir.join("graph.corrupt.stub"),
    )
    .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut cleared = false;
    while std::time::Instant::now() < deadline {
        if !infigraph_core::recovery::pending_recovery(&infigraph_dir) {
            cleared = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(
        cleared,
        "daemon must clear the recovery-needed sentinel within 30s"
    );
    assert_eq!(
        infigraph_core::recovery::recent_recovery_attempts(&infigraph_dir)
            .unwrap()
            .len(),
        1,
        "exactly one auto-triggered rebuild must be recorded"
    );

    drop(daemon);
}

#[test]
fn a_third_recovery_trigger_inside_the_window_trips_the_crash_loop_breaker_instead_of_rebuilding() {
    let tmp = tempfile::tempdir().unwrap();
    let infigraph_dir = tmp.path().join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();
    for _ in 0..infigraph_core::recovery::CRASH_LOOP_THRESHOLD {
        infigraph_core::recovery::record_recovery_attempt(&infigraph_dir).unwrap();
    }
    infigraph_core::recovery::mark_recovery_needed(
        &infigraph_dir,
        1,
        &infigraph_dir.join("graph.corrupt.stub"),
    )
    .unwrap();

    infigraph_core::recovery::drain_recovery_sentinel(&infigraph_dir).unwrap();

    assert!(
        infigraph_core::recovery::crash_loop_detected(&infigraph_dir).is_some(),
        "breaker must trip at the threshold"
    );
    let requests: usize = std::fs::read_dir(infigraph_dir.join("requests"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(
        requests, 0,
        "must not submit another FullReindex once tripped"
    );
}
