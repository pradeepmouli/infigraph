use infigraph_core::graph::GraphBackend;
use infigraph_core::watch::daemon::{is_ci_env, is_remote_backend, watch_daemon_mode_enabled};

/// Serializes tests that mutate process-global env vars — cargo runs this
/// binary's tests on parallel threads, so a lowered override in one test
/// must not leak into another test's window (same lesson as PR6's lockfile
/// tests and PR5's idle-decision tests).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn is_remote_backend_only_true_for_explicit_neo4j() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert!(!is_remote_backend());
    std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
    assert!(!is_remote_backend());
    std::env::set_var("INFIGRAPH_BACKEND", "neo4j");
    assert!(is_remote_backend());
    std::env::remove_var("INFIGRAPH_BACKEND");
}

#[test]
fn is_ci_env_detects_any_known_ci_var() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for v in [
        "CI",
        "GITHUB_ACTIONS",
        "JENKINS_URL",
        "BUILDKITE",
        "GITLAB_CI",
        "INFIGRAPH_NO_WATCH",
    ] {
        std::env::remove_var(v);
    }
    assert!(!is_ci_env());
    std::env::set_var("INFIGRAPH_NO_WATCH", "1");
    assert!(is_ci_env());
    std::env::remove_var("INFIGRAPH_NO_WATCH");
}

#[test]
fn watch_daemon_mode_is_opt_in_off_by_default() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
    assert!(!watch_daemon_mode_enabled());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "1");
    assert!(watch_daemon_mode_enabled());
    std::env::set_var("INFIGRAPH_WATCH_DAEMON", "0");
    assert!(!watch_daemon_mode_enabled());
    std::env::remove_var("INFIGRAPH_WATCH_DAEMON");
}

#[test]
fn ensure_daemon_running_noops_under_ci() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("CI", "1");
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();
    let outcome = infigraph_core::watch::daemon::ensure_daemon_running(
        tmp.path(),
        std::path::Path::new("/nonexistent/infigraph"),
    );
    assert_eq!(
        outcome,
        infigraph_core::watch::daemon::DaemonStartOutcome::AlreadyRunning
    );
    std::env::remove_var("CI");
}

/// A project that hasn't been indexed yet (no `.infigraph` at `root`) is a
/// benign precondition-not-met state, not a failure — e.g. the CLI calls
/// this on every command dispatch, including the very first `infigraph
/// index` on a fresh project, before `.infigraph` is created. Callers
/// (`infigraph-cli::index::ensure_watcher_running`) surface `Failed` as a
/// visible "Failed to start watcher" stderr message, so this case must stay
/// `AlreadyRunning` (silent) rather than `Failed`, or ordinary first-time
/// use prints a spurious failure line. See task-3-review.md Finding 1.
#[test]
fn ensure_daemon_running_noops_when_not_yet_indexed() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for v in [
        "CI",
        "GITHUB_ACTIONS",
        "JENKINS_URL",
        "BUILDKITE",
        "GITLAB_CI",
        "INFIGRAPH_NO_WATCH",
        "INFIGRAPH_BACKEND",
    ] {
        std::env::remove_var(v);
    }
    let tmp = tempfile::tempdir().unwrap();
    assert!(!tmp.path().join(".infigraph").exists());

    let outcome = infigraph_core::watch::daemon::ensure_daemon_running(
        tmp.path(),
        std::path::Path::new("/nonexistent/infigraph"),
    );
    assert_eq!(
        outcome,
        infigraph_core::watch::daemon::DaemonStartOutcome::AlreadyRunning,
        "not-yet-indexed projects must no-op silently, not report Failed"
    );
}

/// Selecting `DaemonKuzu` implies daemon-mode watching: `init()` must start
/// a daemon itself rather than requiring `INFIGRAPH_WATCH_DAEMON=1` to be
/// set independently (plan Global Constraints). Proves the real effect --
/// a daemon process exists and holds `.infigraph/watch.lock` afterwards --
/// not merely that `ensure_daemon_running` is called.
#[test]
fn init_daemon_backend_starts_a_daemon() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for v in [
        "CI",
        "GITHUB_ACTIONS",
        "JENKINS_URL",
        "BUILDKITE",
        "GITLAB_CI",
        "INFIGRAPH_NO_WATCH",
        "INFIGRAPH_BACKEND",
    ] {
        std::env::remove_var(v);
    }

    // init()'s daemon arm re-execs the CLI binary; skip rather than fail if
    // this test binary was built without it (infigraph-core has no
    // dev-dependency on infigraph-cli, so cargo won't build it for us).
    let Ok(_cli) = infigraph_core::watch::daemon::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    ) else {
        eprintln!("skipping: infigraph CLI binary not built in this target dir");
        return;
    };

    let project_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        project_dir.path().join("main.py"),
        "def hello():\n    pass\n",
    )
    .unwrap();

    // A read-only connection can't create a database, so the graph must
    // exist before daemon-mode init() can open it. Bootstrap with the
    // default Kuzu backend and drop it (releasing the write lock) first.
    let registry = infigraph_languages::bundled_registry().unwrap();
    let mut boot = infigraph_core::Infigraph::open(project_dir.path(), registry).unwrap();
    boot.init().unwrap();
    drop(boot);

    let lock_path = project_dir.path().join(".infigraph").join("watch.lock");
    assert!(
        infigraph_core::lockfile::try_acquire(&lock_path, "test-probe")
            .unwrap()
            .is_some(),
        "no daemon should be running before init()"
    );

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let registry = infigraph_languages::bundled_registry().unwrap();
    let mut client = infigraph_core::Infigraph::open(project_dir.path(), registry).unwrap();
    let init_result = client.init();
    std::env::remove_var("INFIGRAPH_BACKEND");
    init_result.unwrap();

    // The spawned daemon is detached (setsid), so there's no Child handle to
    // wait on -- its lock hold is the observable proof it came up.
    let start = std::time::Instant::now();
    let mut started = false;
    while start.elapsed() < std::time::Duration::from_secs(15) {
        if infigraph_core::lockfile::try_acquire(&lock_path, "test-probe")
            .map(|g| g.is_none())
            .unwrap_or(false)
        {
            started = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Stop it before asserting, so a failed assertion can't leak a daemon.
    std::fs::write(project_dir.path().join(".infigraph").join("watch.stop"), "").unwrap();
    let stop_start = std::time::Instant::now();
    while stop_start.elapsed() < std::time::Duration::from_secs(15) {
        if infigraph_core::lockfile::try_acquire(&lock_path, "test-probe")
            .map(|g| g.is_some())
            .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert!(
        started,
        "init() under INFIGRAPH_BACKEND=daemon must leave a daemon holding watch.lock"
    );
}

/// Direct, deterministic assertion on the `Command` that `spawn_daemon`
/// builds (via the `pub` `build_daemon_command` helper): its env mutations
/// must include an explicit removal of `INFIGRAPH_BACKEND`, regardless of
/// what leaked into the *test's own* environment. `Command::get_envs()`
/// (stable since Rust 1.57) iterates a command's explicit env mutations,
/// where a removed var appears as `(key, None)` — so this proves
/// `env_remove("INFIGRAPH_BACKEND")` was actually applied to the command
/// that will be exec'd. Delete that call from `build_daemon_command` and
/// this assertion fails; keep it and the test passes — no timing, no OS
/// tool dependency, no reliance on a placeholder backend panicking.
#[test]
fn build_daemon_command_strips_infigraph_backend_env_var() {
    let project_dir = tempfile::tempdir().unwrap();
    let tg_dir = project_dir.path().join(".infigraph");
    std::fs::create_dir_all(&tg_dir).unwrap();

    let cmd = infigraph_core::watch::daemon::build_daemon_command(
        project_dir.path(),
        &tg_dir,
        std::path::Path::new("/nonexistent/infigraph"),
    );

    let removed = cmd
        .get_envs()
        .any(|(key, value)| key == "INFIGRAPH_BACKEND" && value.is_none());
    assert!(
        removed,
        "expected build_daemon_command's Command to explicitly remove INFIGRAPH_BACKEND from its env"
    );
}

/// Sanity check that a daemon spawned via `ensure_daemon_running` still
/// starts up and acquires `watch.lock` end-to-end, even with
/// `INFIGRAPH_BACKEND=daemon` leaked into the *test's own* environment.
/// This supplements (does not replace) the deterministic
/// `build_daemon_command_strips_infigraph_backend_env_var` assertion above:
/// lock acquisition in `cmd_daemon` happens before backend selection is
/// even reached, so this alone can't prove the env-stripping fix works —
/// it only proves the daemon still functions normally.
#[test]
#[cfg(unix)]
fn spawn_daemon_child_still_starts_with_infigraph_backend_leaked_into_test_env() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project_dir.path().join(".infigraph")).unwrap();

    // infigraph-core has no dev-dependency on infigraph-cli, so
    // `env!("CARGO_BIN_EXE_infigraph")` isn't available here (cargo only
    // sets that var for a test binary's own crate-graph binaries). Fall
    // back to the same sibling-binary resolution MCP uses, matching the
    // pattern in crates/infigraph-cli/tests/watch_daemon_docs.rs's
    // `cli_binary` helper.
    let cli_binary = infigraph_core::watch::daemon::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    )
    .expect("infigraph CLI binary must already be built (shared target dir)");

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let outcome =
        infigraph_core::watch::daemon::ensure_daemon_running(project_dir.path(), &cli_binary);
    std::env::remove_var("INFIGRAPH_BACKEND");

    assert_eq!(
        outcome,
        infigraph_core::watch::daemon::DaemonStartOutcome::Spawned,
        "expected the daemon to spawn successfully despite INFIGRAPH_BACKEND=daemon in this test's own env"
    );

    // Give the child a moment to acquire watch.lock.
    let lock_path = project_dir.path().join(".infigraph").join("watch.lock");
    let start = std::time::Instant::now();
    loop {
        if lock_path.exists() && std::fs::metadata(&lock_path).unwrap().len() > 0 {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("daemon never acquired watch.lock");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Clean up: signal the spawned daemon to stop.
    std::fs::write(project_dir.path().join(".infigraph").join("watch.stop"), "").unwrap();
}

/// Independent, freshly-opened read-only check of what actually landed on
/// disk -- `GraphStore::open_read_only`'s doc comment states this is safe
/// for concurrent access while a watcher is writing, and a fresh open per
/// call (rather than a held connection) is required to observe a writer's
/// later commits, matching `DaemonKuzuBackend`'s own read path.
fn file_present_in_graph(project_dir: &std::path::Path, file: &str) -> bool {
    let backend = infigraph_core::graph::KuzuBackend::open_read_only(
        &project_dir.join(".infigraph").join("graph"),
    )
    .unwrap();
    !backend
        .raw_query(&format!("MATCH (f:File) WHERE f.id = '{file}' RETURN f.id"))
        .unwrap()
        .is_empty()
}

/// Closes a pre-existing gap identified while designing the daemon
/// `IndexWorkQueue`: `WatchEventKind::Removed` handling used to call
/// `prism.remove_file`/`remove_files_by_prefix` directly, with no
/// `begin_index_op` call at all -- unlike every other write path in the
/// watch loop, a watch-triggered file removal could mutate the graph while
/// another operation (e.g. a concurrent `infigraph index --full`) believed
/// it held exclusive access via `index.lock`.
///
/// This drives `watch_project_with_periodic` directly in a background
/// thread (the same in-process pattern
/// `daemon_protocol_watcher_wiring.rs`'s sibling tests already use for this
/// exact function -- no real daemon subprocess is needed to exercise the
/// loop's internal locking behavior) rather than spawning a real `infigraph
/// daemon` process, since the assertion is about `watch_project_with_
/// periodic`'s own locking discipline, not process boundaries. Proves the
/// fix by observing the real, externally-visible effect rather than a log
/// line: while `index.lock` is held externally, a watch-triggered removal
/// must not touch the graph at all (the shared drain step's
/// `begin_index_op` call fails to acquire and the queued removal stays
/// queued for the next tick); once the lock is released, the deferred
/// removal completes.
#[test]
fn watch_triggered_file_removal_contends_with_a_held_index_lock() {
    // A custom, non-dot-leading prefix: `tempfile::tempdir()`'s default
    // prefix starts with a dot, and `should_ignore` (this file's watcher)
    // treats ANY path component starting with '.' as ignored -- including
    // the root directory's own name -- which would silently drop every
    // filesystem event this test depends on. Same gotcha this file's
    // `watch_project_detects_changes_through_symlinked_root` test already
    // documents and works around.
    let project = tempfile::Builder::new()
        .prefix("infigraph-watch-lock-test-")
        .tempdir()
        .unwrap();
    let file_path = project.path().join("doomed.py");
    std::fs::write(&file_path, "def doomed():\n    pass\n").unwrap();

    // Bootstrap-index directly (no watcher yet) so doomed.py exists in the
    // graph before the watcher opens its own connection.
    {
        let registry = infigraph_languages::bundled_registry().unwrap();
        let mut boot = infigraph_core::Infigraph::open(project.path(), registry).unwrap();
        boot.init().unwrap();
        boot.index().unwrap();
    }
    assert!(
        file_present_in_graph(project.path(), "doomed.py"),
        "bootstrap must have indexed doomed.py before the removal race starts"
    );

    // Hold index.lock externally, simulating another in-flight operation
    // (e.g. a concurrent `infigraph index --full`).
    let held = infigraph_core::ops::begin_index_op(
        project.path(),
        "test-holder",
        std::time::Duration::ZERO,
    )
    .unwrap();
    let held_guard = match held {
        infigraph_core::ops::IndexOpOutcome::Acquired(g) => g,
        _ => panic!("expected to acquire index.lock in this fresh test dir"),
    };

    // Independent detection signal, decoupled from whether the graph write
    // has actually happened: `on_event` fires unconditionally right after
    // the removal is queued (both before and after this fix -- see
    // `WatchEventKind::Removed` handling in `watch/mod.rs`), so counting it
    // proves the watcher noticed the removal without assuming anything
    // about write timing. Checking graph state only makes sense once this
    // has actually fired -- otherwise "file still present" could just mean
    // the event hasn't arrived yet (this was tried first, using a fixed
    // sleep-then-check window, and it produced a false pass against the
    // pre-fix code: FSEvents delivery latency for a freshly spawned test
    // binary process varies widely enough, several seconds observed, that
    // a fixed window can't reliably tell "not yet detected" apart from
    // "detected and correctly deferred").
    let removed_event_count = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let removed_event_count_clone = removed_event_count.clone();

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let root = project.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::watch::watch_project_with_periodic(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            move |evt| {
                if evt.kind == infigraph_core::watch::WatchEventKind::Removed {
                    *removed_event_count_clone.lock().unwrap() += 1;
                }
            },
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            false, // serve_requests
            None,
        )
    });

    // Let the watcher register before triggering the removal. A freshly
    // spawned process's FSEvents subscription (as opposed to one already
    // running inside a warm `--lib` unit-test binary) can take noticeably
    // longer than a few hundred ms to start reliably delivering events --
    // empirically confirmed while writing this test.
    std::thread::sleep(std::time::Duration::from_millis(3000));
    std::fs::remove_file(&file_path).unwrap();

    // Bounded wait for detection -- generous, since FSEvents delivery
    // latency in a freshly spawned process was observed to reach several
    // seconds in this environment.
    let mut detected = false;
    for _ in 0..150 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if *removed_event_count.lock().unwrap() > 0 {
            detected = true;
            break;
        }
    }
    assert!(
        detected,
        "watcher never detected the file removal at all -- can't exercise \
         lock contention without this"
    );

    assert!(
        file_present_in_graph(project.path(), "doomed.py"),
        "removal must be deferred while index.lock is held externally -- if this \
         fails, watch-triggered removal is once again bypassing the lock"
    );

    // Release the external hold; the next tick's drain should now succeed.
    drop(held_guard);

    let mut removed = false;
    for _ in 0..150 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !file_present_in_graph(project.path(), "doomed.py") {
            removed = true;
            break;
        }
    }
    assert!(
        removed,
        "expected the watcher to complete the deferred removal once index.lock was released"
    );

    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
}

/// Regression test for the Task 3 review finding: `route_or_serve_request`'s
/// fallback paths (the 9 out-of-scope `WriteRequest` variants, plus
/// malformed/corrupt requests) used to call `serve_one_request` directly
/// with no `index.lock` acquisition at all, unlike every other write path in
/// the daemon. Proves the fix using `WriteRequest::UpsertRepo` -- one of the
/// 9 out-of-scope variants, chosen because it's the simplest to construct
/// (a single string field) and its handler (`GraphBackend::upsert_repo`)
/// writes to the graph: while `index.lock` is held externally, the request
/// must NOT be served (no `.result` file appears, and the `.request` file
/// stays in place so it's retried); once the lock is released, it is
/// served.
#[test]
fn out_of_scope_write_request_contends_with_a_held_index_lock() {
    let project = tempfile::Builder::new()
        .prefix("infigraph-watch-lock-oos-test-")
        .tempdir()
        .unwrap();

    // Bootstrap so the graph exists before the watcher opens its own connection.
    {
        let registry = infigraph_languages::bundled_registry().unwrap();
        let mut boot = infigraph_core::Infigraph::open(project.path(), registry).unwrap();
        boot.init().unwrap();
    }

    // Hold index.lock externally, simulating another in-flight operation
    // (e.g. a concurrent `infigraph index --full`).
    let held = infigraph_core::ops::begin_index_op(
        project.path(),
        "test-holder",
        std::time::Duration::ZERO,
    )
    .unwrap();
    let held_guard = match held {
        infigraph_core::ops::IndexOpOutcome::Acquired(g) => g,
        _ => panic!("expected to acquire index.lock in this fresh test dir"),
    };

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let root = project.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::watch::watch_project_with_periodic(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
        )
    });

    // Let the loop start ticking before dropping a request file.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let requests_dir = project.path().join(".infigraph").join("requests");
    let request_path = requests_dir.join("out-of-scope-test.request");
    let result_path = requests_dir.join("out-of-scope-test.result");
    let request = infigraph_core::daemon_protocol::WriteRequest::UpsertRepo {
        namespace: "test-repo".to_string(),
    };
    infigraph_core::daemon_protocol::write_atomic(
        &request_path,
        &serde_json::to_string(&request).unwrap(),
    )
    .unwrap();

    // Poll rather than a single fixed-sleep-then-check: a one-shot check
    // after a fixed delay can false-pass against the pre-fix (unlocked) bug
    // if cold-start latency (registry construction, DB open) happens to
    // push the wrongful serve past the checkpoint -- the same false-negative
    // trap documented on the sibling
    // `watch_triggered_file_removal_contends_with_a_held_index_lock` test
    // above. Polling for up to 10s means any premature serve is caught
    // whenever it actually happens, not just at one sampled instant.
    for i in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !result_path.exists(),
            "out-of-scope request was served after ~{}ms while index.lock is held \
             externally -- route_or_serve_request's fallback path is bypassing the lock",
            (i + 1) * 100
        );
    }
    assert!(
        request_path.exists(),
        ".request file must remain in place (not deleted) while contended, so it's retried \
         on a later tick"
    );

    // Release the external hold; the next tick's serve_request_locked
    // attempt should now succeed.
    drop(held_guard);

    let mut served = false;
    for _ in 0..150 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if result_path.exists() {
            served = true;
            break;
        }
    }
    assert!(
        served,
        "expected the watcher to serve the deferred out-of-scope request once \
         index.lock was released"
    );

    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
}
