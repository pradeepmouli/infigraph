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

/// infigraph-core has no dev-dependency on infigraph-cli, so
/// `env!("CARGO_BIN_EXE_infigraph")` isn't available here (cargo only sets
/// that var for a test binary's own crate-graph binaries). Resolve the CLI
/// binary the same way Task 2's watch_daemon.rs test does.
fn cli_binary() -> PathBuf {
    infigraph_core::watch::daemon::resolve_cli_binary_sibling_of(&std::env::current_exe().unwrap())
        .expect("infigraph CLI binary must already be built (shared target dir)")
}

/// Bootstrap-index `project_dir` directly (BackendKind::Kuzu, no daemon
/// involved, so `.infigraph/` exists), then start a real detached
/// `infigraph daemon` against it and wait until it holds `watch.lock`.
fn start_real_daemon(project_dir: &Path) -> KillOnDrop {
    let cli = cli_binary();

    let status = Command::new(&cli)
        .arg("index")
        .current_dir(project_dir)
        .env_remove("INFIGRAPH_BACKEND")
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
    std::fs::write(project_dir.join(".infigraph").join("watch.stop"), "").unwrap();
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
///  - `INFIGRAPH_INDEX_VIA_DAEMON=1`, where the whole job is one
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
    let opt_in_output = run_cli_index(project_dir.path(), &[("INFIGRAPH_INDEX_VIA_DAEMON", "1")]);
    assert!(
        !has_language_breakdown(&opt_in_output),
        "under INFIGRAPH_INDEX_VIA_DAEMON the daemon does the parsing and the protocol \
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
/// (fix/daemonkuzu-index-routing): under INFIGRAPH_INDEX_VIA_DAEMON=1,
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
        .env("INFIGRAPH_INDEX_VIA_DAEMON", "1")
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
        move || run_cli_index(&dir, &[("INFIGRAPH_INDEX_VIA_DAEMON", "1")])
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
    run_cli_index(project.path(), &[("INFIGRAPH_INDEX_VIA_DAEMON", "1")]);
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
