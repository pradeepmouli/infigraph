use infigraph_core::daemon::lifecycle::{
    is_ci_env, is_remote_backend, watch_daemon_mode_enabled, CI_ENV_VARS,
};
use infigraph_core::graph::GraphBackend;

/// Serializes tests that mutate process-global env vars — cargo runs this
/// binary's tests on parallel threads, so a lowered override in one test
/// must not leak into another test's window (same lesson as PR6's lockfile
/// tests and PR5's idle-decision tests).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard suppressing the CI/`INFIGRAPH_NO_WATCH` opt-out that
/// `ensure_daemon_running` honours, restoring every variable it removed on
/// drop. A test asserting `DaemonStartOutcome::Spawned` exercises the
/// *opportunistic* auto-start path, whose documented precondition is "not
/// under CI" -- and GitHub Actions sets both `CI` and `GITHUB_ACTIONS` on
/// every runner, so without this the assertion is unreachable under CI on
/// every platform.
///
/// The `Spawned` tests below used to pass in CI only by accident: this
/// binary's `is_ci_env_detects_any_known_ci_var` removed `CI` and
/// `GITHUB_ACTIONS` without restoring them, de-CI-ifying the whole process
/// for whichever tests happened to run afterwards. That made correctness
/// depend on test *ordering*, and left the identical bug unmasked (and
/// failing) in `daemon_kuzu_e2e.rs`, a binary with no such test. Restoring
/// on drop is what removes the ordering dependency.
///
/// Driven by `CI_ENV_VARS` rather than a hand-copied list, so a variable
/// added there is covered here automatically. Callers must already hold
/// `ENV_LOCK` -- process env is global. Mirrors `daemon_kuzu_e2e.rs`'s copy
/// (a different test binary: cargo compiles each `tests/*.rs` as its own
/// crate, the same precedent `KillPidOnDrop` below already follows).
struct CiOptOutSuppressed(Vec<(&'static str, std::ffi::OsString)>);

impl CiOptOutSuppressed {
    fn new() -> Self {
        let saved = CI_ENV_VARS
            .iter()
            .filter_map(|v| std::env::var_os(v).map(|old| (*v, old)))
            .collect();
        for v in CI_ENV_VARS {
            std::env::remove_var(v);
        }
        Self(saved)
    }
}

impl Drop for CiOptOutSuppressed {
    fn drop(&mut self) {
        for (v, old) in self.0.drain(..) {
            std::env::set_var(v, old);
        }
    }
}

/// RAII guard killing a daemon spawned via `ensure_daemon_running` by PID --
/// that function hands back no `Child` at all (production's "opportunistic
/// auto-start" wants a detached, independent daemon, not a handle to
/// babysit). Without this, a panic between the spawn and a test's own
/// `watch.stop` sentinel write leaks a real `infigraph daemon` process
/// indefinitely -- exactly how 19 leaked daemon processes (some running for
/// days) were found and killed during a disk-space incident on 2026-08-31.
/// Mirrors `daemon_kuzu_e2e.rs`'s own copy (kept separate: a different test
/// binary, same small-helper-duplication precedent as `KillOnDrop` already
/// uses across crates). `kill_infigraph_process` refuses anything that
/// isn't verifiably an infigraph binary, so this can't kill an unrelated
/// process even if the PID were somehow stale/recycled.
/// Only the `cfg(unix)` tests below construct this.
#[cfg(unix)]
struct KillPidOnDrop(u32);

#[cfg(unix)]
impl Drop for KillPidOnDrop {
    fn drop(&mut self) {
        let _ = infigraph_core::ps::kill_infigraph_process(self.0, false);
    }
}

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
    // Restores the ambient CI vars on drop -- under real CI this test used to
    // strip them from the process permanently, silently changing the
    // environment every later test in this binary observed.
    let _ci = CiOptOutSuppressed::new();
    assert!(!is_ci_env());
    std::env::set_var("INFIGRAPH_NO_WATCH", "1");
    assert!(is_ci_env());
    std::env::remove_var("INFIGRAPH_NO_WATCH");
}

#[test]
fn watch_daemon_mode_is_opt_in_off_by_default() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert!(!watch_daemon_mode_enabled());
    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    assert!(watch_daemon_mode_enabled());
    std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
    assert!(!watch_daemon_mode_enabled());
    std::env::remove_var("INFIGRAPH_BACKEND");
}

#[test]
fn ensure_daemon_running_noops_under_ci() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Take the guard first so the `remove_var` below restores whatever the
    // ambient environment had, rather than unsetting a real runner's `CI`.
    let _ci = CiOptOutSuppressed::new();
    std::env::set_var("CI", "1");
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();
    let outcome = infigraph_core::daemon::lifecycle::ensure_daemon_running(
        tmp.path(),
        std::path::Path::new("/nonexistent/infigraph"),
    );
    assert_eq!(
        outcome,
        infigraph_core::daemon::lifecycle::DaemonStartOutcome::AlreadyRunning
    );
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
    let _ci = CiOptOutSuppressed::new();
    std::env::remove_var("INFIGRAPH_BACKEND");
    let tmp = tempfile::tempdir().unwrap();
    assert!(!tmp.path().join(".infigraph").exists());

    let outcome = infigraph_core::daemon::lifecycle::ensure_daemon_running(
        tmp.path(),
        std::path::Path::new("/nonexistent/infigraph"),
    );
    assert_eq!(
        outcome,
        infigraph_core::daemon::lifecycle::DaemonStartOutcome::AlreadyRunning,
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
    let _ci = CiOptOutSuppressed::new();
    std::env::remove_var("INFIGRAPH_BACKEND");

    // init()'s daemon arm re-execs the CLI binary; skip rather than fail if
    // this test binary was built without it (infigraph-core has no
    // dev-dependency on infigraph-cli, so cargo won't build it for us).
    let Ok(_cli) = infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
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

    let cmd = infigraph_core::daemon::lifecycle::build_daemon_command(
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
    let _ci = CiOptOutSuppressed::new();
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project_dir.path().join(".infigraph")).unwrap();

    // infigraph-core has no dev-dependency on infigraph-cli, so
    // `env!("CARGO_BIN_EXE_infigraph")` isn't available here (cargo only
    // sets that var for a test binary's own crate-graph binaries). Fall
    // back to the same sibling-binary resolution MCP uses, matching the
    // pattern in crates/infigraph-cli/tests/watch_daemon_docs.rs's
    // `cli_binary` helper.
    let cli_binary = infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    )
    .expect("infigraph CLI binary must already be built (shared target dir)");

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let outcome =
        infigraph_core::daemon::lifecycle::ensure_daemon_running(project_dir.path(), &cli_binary);
    std::env::remove_var("INFIGRAPH_BACKEND");

    assert_eq!(
        outcome,
        infigraph_core::daemon::lifecycle::DaemonStartOutcome::Spawned,
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

    // Acquired immediately once the daemon is known to be running, as a
    // fallback in case the sentinel write below doesn't get noticed -- see
    // KillPidOnDrop's doc.
    let holder = infigraph_core::lockfile::read_holder(&lock_path)
        .expect("watch.lock must have a readable holder payload once held");
    let _kill_guard = KillPidOnDrop(holder.pid);

    // Clean up: `watch.stop` only stops the watch *thread*, leaving the
    // daemon process itself alive -- ending the whole process needs a
    // `WatchControl { role: Daemon, action: Stop }` request instead, the
    // same mechanism `cmd_daemon_stop` uses. Best-effort: KillPidOnDrop
    // above is the real cleanup guarantee.
    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    let _ = infigraph_core::daemon_protocol::submit_write_request(
        &staging_dir,
        &infigraph_core::daemon_protocol::WriteRequest::WatchControl {
            role: infigraph_core::daemon_protocol::WatchRole::Daemon,
            action: infigraph_core::daemon_protocol::WatchAction::Stop,
        },
        std::time::Duration::from_secs(10),
    );
}

/// End-to-end proof that `ensure_daemon_running` prunes a stale watch.lock
/// (here: a dead holder PID from a build that's no longer installed) and
/// spawns a fresh daemon, instead of reporting `AlreadyRunning` forever —
/// the gap that let a genuinely-dead-but-locked project sit unwatched until
/// someone noticed via `infigraph doctor` and killed the stale holder by
/// hand.
#[test]
#[cfg(unix)]
fn ensure_daemon_running_prunes_a_dead_stale_holder_and_spawns_fresh() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ci = CiOptOutSuppressed::new();
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project_dir.path().join(".infigraph")).unwrap();

    let cli_binary = infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    )
    .expect("infigraph CLI binary must already be built (shared target dir)");

    // Plant a lock payload naming a PID that isn't running and a build_hash
    // that isn't the one installed here -- simulating a watcher left behind
    // by an old binary, the exact scenario `infigraph doctor` flags as
    // "predates the currently installed binary; restart it to pick up the
    // new build."
    let lock_path = project_dir.path().join(".infigraph").join("watch.lock");
    let stale_payload = serde_json::json!({
        "pid": std::process::id() + 1_000_000,
        "role": "cli-watch",
        "build_hash": "some-old-build-that-no-longer-exists",
        "acquired_at": 0,
        "last_heartbeat": 0
    });
    std::fs::write(&lock_path, stale_payload.to_string()).unwrap();

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    let outcome =
        infigraph_core::daemon::lifecycle::ensure_daemon_running(project_dir.path(), &cli_binary);
    std::env::remove_var("INFIGRAPH_BACKEND");

    assert_eq!(
        outcome,
        infigraph_core::daemon::lifecycle::DaemonStartOutcome::Spawned,
        "a dead stale holder must be pruned and a fresh daemon spawned, not reported as AlreadyRunning"
    );

    // Confirm a real, current-build daemon now holds the lock.
    let start = std::time::Instant::now();
    let mut holder = None;
    while start.elapsed() < std::time::Duration::from_secs(5) {
        if let Some(h) = infigraph_core::lockfile::read_holder(&lock_path) {
            holder = Some(h);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Note: this doesn't compare against `infigraph_core::build_hash()` --
    // the spawned `infigraph` CLI binary comes from whatever last built it
    // (e.g. a prior `cargo build -p infigraph-cli`), which cargo does not
    // keep in lockstep with `cargo test -p infigraph-core` (infigraph-core
    // has no dev-dependency on infigraph-cli). What matters here is that a
    // *real* daemon took over -- proven by a live PID distinct from the
    // fake one, on a build_hash distinct from the fake stale payload.
    let holder = holder.expect("expected a fresh daemon to have acquired watch.lock");
    // Acquired immediately, before the assertions below that could panic --
    // see KillPidOnDrop's doc.
    let _kill_guard = KillPidOnDrop(holder.pid);
    assert_ne!(
        holder.build_hash, "some-old-build-that-no-longer-exists",
        "the fake stale payload must have been replaced by a real daemon's own identity"
    );
    assert_ne!(
        holder.pid,
        std::process::id() + 1_000_000,
        "the lock must now be held by a real spawned process, not the fake stale PID"
    );

    // Clean up: `watch.stop` only stops the watch *thread*, leaving the
    // daemon process itself alive -- ending the whole process needs a
    // `WatchControl { role: Daemon, action: Stop }` request instead, the
    // same mechanism `cmd_daemon_stop` uses. Best-effort: KillPidOnDrop
    // above is the real cleanup guarantee.
    let staging_dir = project_dir.path().join(".infigraph").join("requests");
    let _ = infigraph_core::daemon_protocol::submit_write_request(
        &staging_dir,
        &infigraph_core::daemon_protocol::WriteRequest::WatchControl {
            role: infigraph_core::daemon_protocol::WatchRole::Daemon,
            action: infigraph_core::daemon_protocol::WatchAction::Stop,
        },
        std::time::Duration::from_secs(10),
    );
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
/// This drives `run_write_coordinator` directly in a background
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
    #[derive(Default)]
    struct EventCounts {
        /// Any event at all -- proof the watcher is actually subscribed.
        any: usize,
        /// Removal events specifically, the thing under test.
        removed: usize,
    }
    let removed_event_count = std::sync::Arc::new(std::sync::Mutex::new(EventCounts::default()));
    let removed_event_count_clone = removed_event_count.clone();

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let root = project.path().to_path_buf();
    let daemon_token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = daemon_token.clone();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            move |evt| {
                let mut counts = removed_event_count_clone.lock().unwrap();
                counts.any += 1;
                if evt.kind == infigraph_core::watch::WatchEventKind::Removed {
                    counts.removed += 1;
                }
            },
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            false, // serve_requests
            None,
            &token_for_thread,
            None,
        )
    });

    // Wait for the watcher to be demonstrably subscribed before triggering
    // the removal, rather than assuming a fixed delay is enough.
    //
    // A fixed 3s sleep used to stand here, and it was the reason this test
    // failed on both macOS and ubuntu in every CI run: the removal landed
    // before the watcher existed, so no event was ever generated and the
    // 15s detection wait below could not recover one. Subscription is not
    // instant -- `run_write_coordinator` builds the entire bundled language
    // registry before it starts watching, which its own comment notes costs
    // seconds in a debug build, and FSEvents in a freshly spawned process
    // then takes its own time on top.
    //
    // Touching a probe file until an event comes back proves the watcher is
    // live. It is the same trick the detection loop below already uses for
    // delivery latency, applied to the subscription that has to precede it.
    // Probe with a DELETION, not a write.
    //
    // `on_event` is only ever called for `Removed` file events and for
    // watcher lifecycle events (see producer.rs) -- the `Created`/`Modified`
    // arm marks the path dirty and adds it to the debounce batch without
    // notifying anyone. So a probe that merely writes a file cannot produce
    // a callback on any platform.
    //
    // An earlier version of this probe did exactly that and still passed on
    // macOS, which is what made the mistake hard to see: FSEvents coalescing
    // reports a rewrite of an existing path with a Remove flag, so repeatedly
    // writing the SAME file happens to yield `Removed` there. inotify emits
    // MODIFY and never does, so on Linux that probe could not succeed no
    // matter how long it waited -- and its failure message blamed the
    // watcher for never subscribing when the watcher was fine.
    //
    // Creating and then removing a throwaway file produces a real removal on
    // both backends, and is the same kind of event this test goes on to
    // depend on.
    let probe_path = project.path().join("probe.py");
    let subscribe_deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut subscribed = false;
    while std::time::Instant::now() < subscribe_deadline {
        std::fs::write(&probe_path, "# probe\n").unwrap();
        // Let the create land before the remove, so the two are not coalesced
        // into a single event that reports only the creation.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = std::fs::remove_file(&probe_path);
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if removed_event_count.lock().unwrap().any > 0 {
                subscribed = true;
                break;
            }
        }
        if subscribed {
            break;
        }
    }
    assert!(
        subscribed,
        "watcher delivered no event for a probe file created and deleted under the watched \
         root within 60s -- it is not receiving filesystem events, so the removal below \
         could not be observed either"
    );

    // Only now hold index.lock externally, simulating another in-flight
    // operation (e.g. a concurrent `infigraph index --full`). It must be
    // held across the removal, which is what this test asserts gets
    // deferred -- but NOT before the subscription probe above, because a
    // newly-created file can only reach `on_event` through the indexing
    // path, and that path is exactly what a held lock blocks. Acquiring it
    // first (as this test used to) left no way to tell "the watcher is not
    // subscribed yet" apart from "the watcher is subscribed and correctly
    // deferring" -- the very distinction the test rests on.
    //
    // For the same reason, this waits rather than passing `Duration::ZERO`.
    // The probe drives the indexing path, that path takes this very lock,
    // and demanding it with no patience raced the watcher's still-draining
    // probe work -- failing on Linux as "expected to acquire index.lock in
    // this fresh test dir" when the directory was fresh and the lock simply
    // was not free *yet*. This test needs to hold the lock across the
    // removal below, which is the deferral it asserts; it never needed the
    // lock to be free instantly. (The sibling test that keeps
    // `Duration::ZERO` takes its lock before any watcher starts, where an
    // occupied lock really would be a defect.)
    let held_guard = match infigraph_core::ops::begin_index_op(
        project.path(),
        "test-holder",
        std::time::Duration::from_secs(30),
    ) {
        Ok(infigraph_core::ops::IndexOpOutcome::Acquired(g)) => g,
        // Unreachable while a non-zero wait blocks instead of reporting the
        // holder -- matched so a later change to `begin_index_op` cannot
        // quietly turn contention into a skipped assertion.
        Ok(infigraph_core::ops::IndexOpOutcome::AlreadyRunning(holder)) => {
            panic!("index.lock still held after 30s by {holder:?}")
        }
        Err(e) => panic!("could not acquire index.lock within 30s: {e}"),
    };

    std::fs::remove_file(&file_path).unwrap();

    // Bounded wait for detection -- generous, since FSEvents delivery
    // latency in a freshly spawned process was observed to reach several
    // seconds in this environment.
    let mut detected = false;
    for _ in 0..150 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if removed_event_count.lock().unwrap().removed > 0 {
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

/// Regression test: a daemon whose watched root got deleted out from under
/// it (a test's tempdir, a project someone `rm -rf`'d without `infigraph
/// delete`) used to run forever, watching nothing -- `prune_stale_holder`
/// only reaps a *dead* lock holder, and this process is very much alive.
/// The watch loop must now notice its root is gone and shut itself down.
#[test]
fn watch_loop_shuts_down_when_its_root_directory_is_deleted() {
    let project = tempfile::Builder::new()
        .prefix("infigraph-watch-root-gone-test-")
        .tempdir()
        .unwrap();
    std::fs::write(project.path().join("a.py"), "def a():\n    pass\n").unwrap();

    {
        let registry = infigraph_languages::bundled_registry().unwrap();
        let mut boot = infigraph_core::Infigraph::open(project.path(), registry).unwrap();
        boot.init().unwrap();
        boot.index().unwrap();
    }

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let root = project.path().to_path_buf();
    let daemon_token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = daemon_token.clone();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            false, // serve_requests
            None,
            &token_for_thread,
            None,
        )
    });

    // Give the watcher a moment to finish setup before pulling the rug --
    // same generous window this file's other watch-loop tests use for a
    // freshly spawned process.
    std::thread::sleep(std::time::Duration::from_millis(1000));

    // Keep the TempDir guard itself alive (so this test doesn't race its own
    // Drop impl) but remove everything under it right now, from outside the
    // watch loop.
    std::fs::remove_dir_all(project.path()).unwrap();

    // Bounded wait for self-shutdown -- the loop ticks at least every 200ms
    // via its recv_timeout, so this should be fast; generous only to absorb
    // scheduling noise under a loaded test run.
    let mut exited = false;
    for _ in 0..100 {
        if handle.is_finished() {
            exited = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        exited,
        "watch loop must shut itself down once its root directory is deleted, \
         instead of running forever watching nothing"
    );
    // A clean shutdown that already returned, rather than one still
    // blocked -- the send below would otherwise hang this test on a closed
    // channel with no other signal of what went wrong.
    let _ = stop_tx.send(());
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
    let daemon_token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = daemon_token.clone();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
            &token_for_thread,
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

/// Regression coverage for R2.4.5 (docs/DESIGN-hardening.md): the full-reindex
/// build phase must be a genuinely cancellable `Task<T>`, not a bare
/// `JoinHandle` with no token. Cancelling the daemon's token before the loop
/// even reaps the request means `try_start_full_reindex`'s spawned task
/// inherits an already-cancelled child token, so `build_full_reindex`'s
/// checkpoint fires immediately after its cheap cleanup step -- before it
/// ever opens Kuzu on `graph.rebuilding` -- which proves cancellation this
/// early can never reach the swap. Two things are checked: the live graph's
/// mtime is unchanged (the swap never ran), and the loop still replies to the
/// request (with an error) instead of hanging while reaping the cancelled
/// task -- mirrors `out_of_scope_write_request_contends_with_a_held_index_lock`'s
/// setup/drive/assert shape for driving `run_write_coordinator` against
/// a real temp project with a real request file.
#[test]
fn full_reindex_build_task_can_be_cancelled_before_it_starts_the_swap() {
    let project = tempfile::Builder::new()
        .prefix("infigraph-full-reindex-cancel-test-")
        .tempdir()
        .unwrap();
    std::fs::write(project.path().join("main.py"), "def main():\n    pass\n").unwrap();

    // Bootstrap so the live graph exists before the watcher opens its own connection.
    {
        let registry = infigraph_languages::bundled_registry().unwrap();
        let mut boot = infigraph_core::Infigraph::open(project.path(), registry).unwrap();
        boot.init().unwrap();
        boot.index().unwrap();
    }

    let live_graph = project.path().join(".infigraph").join("graph");
    let mtime_before = std::fs::metadata(&live_graph).unwrap().modified().unwrap();

    // Cancelled up front, before the loop even starts: by the time
    // `try_start_full_reindex` spawns the build task, its child token is
    // already cancelled, so the build's checkpoint fires at its earliest
    // possible point -- this is the strictest version of "cancel before the
    // swap," since the build never even reaches `Infigraph::open_local_kuzu_at`.
    let daemon_token = tokio_util::sync::CancellationToken::new();
    daemon_token.cancel();
    let token_for_thread = daemon_token.clone();

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let root = project.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
            &token_for_thread,
            None,
        )
    });

    // Let the loop start ticking before dropping a request file.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let requests_dir = project.path().join(".infigraph").join("requests");
    let request_path = requests_dir.join("full-reindex-cancel-test.request");
    let result_path = requests_dir.join("full-reindex-cancel-test.result");
    infigraph_core::daemon_protocol::write_atomic(
        &request_path,
        &serde_json::to_string(&infigraph_core::daemon_protocol::WriteRequest::FullReindex)
            .unwrap(),
    )
    .unwrap();

    // Bounded wait for a reply -- proves the loop didn't hang reaping the
    // cancelled build task (the same false-negative trap the sibling
    // out-of-scope-request test above documents applies here too: a single
    // fixed-delay check could false-pass if cold-start latency happened to
    // land after the sampled instant, so this polls instead).
    let mut replied = false;
    for _ in 0..150 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if result_path.exists() {
            replied = true;
            break;
        }
    }
    assert!(
        replied,
        "expected a reply (even an error one) once the cancelled full-reindex build was \
         reaped -- a missing reply means the loop hung"
    );

    let reply_contents = std::fs::read_to_string(&result_path).unwrap();
    let reply: infigraph_core::daemon_protocol::WriteResult =
        serde_json::from_str(&reply_contents).unwrap();
    assert!(
        matches!(
            reply,
            infigraph_core::daemon_protocol::WriteResult::Err { .. }
        ),
        "a cancelled build must reply with an error, not FullReindexOk: {reply:?}"
    );

    let mtime_after = std::fs::metadata(&live_graph).unwrap().modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "cancelling before the swap must leave the live graph untouched"
    );

    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
}

/// Regression coverage for R2.4.5 (docs/DESIGN-hardening.md): SCIP enrichment
/// (scheduled via `on_full_reindex` right after a successful full-reindex
/// swap) must be tracked as a `Task<()>` tied into `daemon_token`'s
/// hierarchy, not a bare `tokio::task::JoinHandle` with no cancellation
/// vocabulary at all -- mirrors
/// `full_reindex_build_task_can_be_cancelled_before_it_starts_the_swap`'s
/// setup/drive/assert shape (real temp project, a real `.request` file
/// driving `run_write_coordinator` on its own thread), adapted for the
/// SCIP path: unlike the full-reindex build, `daemon_token` can't be
/// cancelled up front here, since `scip_in_flight`'s task is only scheduled
/// *after* a successful full-reindex swap -- an already-cancelled
/// `daemon_token` would instead cancel that swap itself (Task 4's own
/// checkpoint in `build_full_reindex`) and SCIP would never get scheduled at
/// all. So this test cancels `daemon_token` only once the SCIP-enrichment
/// callback has demonstrably started running -- the earliest point the SCIP
/// path can be reached -- and then confirms two things: the callback still
/// completes exactly once (Task 5 wires the SCIP task through
/// `daemon_token`'s hierarchy but doesn't add a cooperative checkpoint inside
/// the callback itself -- that lands in a later task -- so a cancellation
/// here must not corrupt or duplicate the enrichment), and the watch loop
/// still shuts down cleanly afterward instead of hanging, which is what
/// would happen if the reap block's `is_finished()`/`join()` conversion from
/// `InFlightScip`'s old struct-field shape were wired incorrectly.
#[test]
fn scip_enrichment_task_is_cancellable_via_daemon_token() {
    let project = tempfile::Builder::new()
        .prefix("infigraph-scip-cancel-test-")
        .tempdir()
        .unwrap();
    std::fs::write(project.path().join("main.py"), "def main():\n    pass\n").unwrap();

    // Bootstrap so the live graph exists before the watcher opens its own connection.
    {
        let registry = infigraph_languages::bundled_registry().unwrap();
        let mut boot = infigraph_core::Infigraph::open(project.path(), registry).unwrap();
        boot.init().unwrap();
        boot.index().unwrap();
    }

    // Stand-in for real SCIP enrichment's Kuzu-import step: incremented by
    // the `on_full_reindex` callback the same way real enrichment marks the
    // graph as freshly enriched, but observable from the test without
    // needing a real SCIP indexer binary on PATH.
    let scip_generation = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let scip_generation_for_cb = std::sync::Arc::clone(&scip_generation);
    let on_full_reindex: std::sync::Arc<infigraph_core::daemon::FullReindexCallback> =
        std::sync::Arc::new(move |_prism, _languages, _token| {
            scip_generation_for_cb.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });

    let daemon_token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = daemon_token.clone();

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let root = project.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            Some(on_full_reindex),
            &token_for_thread,
            None,
        )
    });

    // Let the loop start ticking before dropping a request file.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let requests_dir = project.path().join(".infigraph").join("requests");
    let request_path = requests_dir.join("scip-cancel-test.request");
    infigraph_core::daemon_protocol::write_atomic(
        &request_path,
        &serde_json::to_string(&infigraph_core::daemon_protocol::WriteRequest::FullReindex)
            .unwrap(),
    )
    .unwrap();

    // Poll for the SCIP-enrichment callback to have run -- this proves the
    // full-reindex swap landed and `on_full_reindex` was scheduled as a
    // background `Task<()>` and actually executed (not merely that the reap
    // block compiles). Only once this fires can `daemon_token` be cancelled
    // without instead cancelling the full-reindex build itself.
    // Budget generously, because this test pays for TWO bundled-registry
    // builds before the callback can possibly run: `run_write_coordinator`
    // builds one before entering its loop, and `build_full_reindex` keeps
    // its own fresh `make_registry()` call (daemon/mod.rs:341 explains why).
    // Each costs seconds in a debug build -- CLAUDE.md notes registry
    // construction alone can approach several seconds on a loaded machine.
    //
    // The old budget was 300ms + 150 * 100ms = 15.3s for all of that plus
    // the parse, write, swap and callback. It failed on ubuntu with the
    // second `Parsing: 1/1 (100%)` visible in the captured output but no
    // `Writing:` line -- i.e. the reindex had genuinely started and simply
    // ran out of clock, which is the signature of a budget that never
    // accounted for the registry builds rather than of anything broken.
    let wait_start = std::time::Instant::now();
    let deadline = wait_start + std::time::Duration::from_secs(120);
    let mut ran = false;
    while std::time::Instant::now() < deadline {
        if scip_generation.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
            ran = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let waited = wait_start.elapsed();
    if waited > std::time::Duration::from_secs(15) {
        // Surfaces the real cost on whichever machine ran this, so the next
        // budget decision is made from a measurement rather than a guess.
        eprintln!("[test] SCIP-enrichment callback took {waited:?} to run");
    }
    assert!(
        ran,
        "expected the SCIP-enrichment task scheduled by on_full_reindex to run after a \
         successful full-reindex swap (waited {waited:?})"
    );

    // Cancel now that the SCIP task has run -- proves cancelling
    // `daemon_token` afterward doesn't re-trigger, duplicate, or corrupt the
    // already-completed enrichment.
    daemon_token.cancel();
    assert_eq!(
        scip_generation.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "SCIP enrichment must have run exactly once"
    );

    stop_tx.send(()).unwrap();
    // If the reap block's conversion from `InFlightScip`'s `.handle` field to
    // `Task<()>`'s `is_finished()`/`join()` were wired incorrectly (e.g. the
    // shutdown-path reap left referencing a field that no longer exists in
    // the right shape, or never actually joined the task), this would hang
    // instead of returning.
    handle.join().unwrap().unwrap();
}

/// Coverage for the other half of `WatchControl`'s dispatch: `role: Daemon,
/// action: Stop` must end the coordinator loop itself, not just the
/// code-watch producer (`watch_control.rs` covers `role: Code`, whose whole
/// point is the *opposite* -- the loop surviving).
///
/// Worth its own test because the loop-exit signal and the background-work
/// cancellation signal are deliberately separate: `daemon_token` is the root
/// of the task hierarchy and a caller may legitimately hand this loop an
/// already-cancelled one (see
/// `full_reindex_build_task_can_be_cancelled_before_it_starts_the_swap`), so
/// the loop must NOT read `daemon_token.is_cancelled()` as "time to exit".
/// Collapsing the two back into one signal would keep this test passing while
/// breaking that one, and vice versa -- only both together pin the behavior.
#[test]
fn watch_control_daemon_stop_ends_the_coordinator_loop() {
    let project = tempfile::Builder::new()
        .prefix("infigraph-daemon-stop-test-")
        .tempdir()
        .unwrap();
    std::fs::write(project.path().join("main.py"), "def main():\n    pass\n").unwrap();

    {
        let registry = infigraph_languages::bundled_registry().unwrap();
        let mut boot = infigraph_core::Infigraph::open(project.path(), registry).unwrap();
        boot.init().unwrap();
        boot.index().unwrap();
    }

    // Live, uncancelled: the request itself is the only thing that may end
    // this loop.
    let daemon_token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = daemon_token.clone();

    let (_stop_tx, stop_rx) = std::sync::mpsc::channel();
    let root = project.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
            &token_for_thread,
            None,
        )
    });

    std::thread::sleep(std::time::Duration::from_millis(300));

    let requests_dir = project.path().join(".infigraph").join("requests");
    let request_path = requests_dir.join("daemon-stop-test.request");
    let result_path = requests_dir.join("daemon-stop-test.result");
    infigraph_core::daemon_protocol::write_atomic(
        &request_path,
        &serde_json::to_string(
            &infigraph_core::daemon_protocol::WriteRequest::WatchControl {
                role: infigraph_core::daemon_protocol::WatchRole::Daemon,
                action: infigraph_core::daemon_protocol::WatchAction::Stop,
            },
        )
        .unwrap(),
    )
    .unwrap();

    // The reply is written before the loop breaks, so the caller learns the
    // stop was accepted rather than timing out against a process on its way
    // out.
    let mut replied = false;
    for _ in 0..150 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if result_path.exists() {
            replied = true;
            break;
        }
    }
    assert!(
        replied,
        "expected a reply to WatchControl {{ Daemon, Stop }} before the loop exits"
    );
    let reply: infigraph_core::daemon_protocol::WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(
            reply,
            infigraph_core::daemon_protocol::WriteResult::Ok { .. }
        ),
        "WatchControl {{ Daemon, Stop }} must succeed: {reply:?}"
    );

    // `_stop_tx` is deliberately still alive and never sent on: nothing but
    // the request is allowed to end this loop.
    for _ in 0..150 {
        if handle.is_finished() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        handle.is_finished(),
        "WatchControl {{ Daemon, Stop }} must end the coordinator loop"
    );
    handle.join().unwrap().unwrap();

    assert!(
        daemon_token.is_cancelled(),
        "a daemon stop must also cancel the background-work hierarchy"
    );
}

/// The companion to the test above, for a role the daemon cannot honour:
/// `Start` has no meaning when you are already talking to a live daemon, so
/// it must come back as an explicit error rather than a silent no-op -- and
/// crucially must NOT end the loop.
#[test]
fn watch_control_daemon_start_is_rejected_without_stopping_the_loop() {
    let project = tempfile::Builder::new()
        .prefix("infigraph-daemon-start-reject-test-")
        .tempdir()
        .unwrap();
    std::fs::write(project.path().join("main.py"), "def main():\n    pass\n").unwrap();

    {
        let registry = infigraph_languages::bundled_registry().unwrap();
        let mut boot = infigraph_core::Infigraph::open(project.path(), registry).unwrap();
        boot.init().unwrap();
        boot.index().unwrap();
    }

    let daemon_token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = daemon_token.clone();

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let root = project.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
            &token_for_thread,
            None,
        )
    });

    std::thread::sleep(std::time::Duration::from_millis(300));

    let requests_dir = project.path().join(".infigraph").join("requests");
    let request_path = requests_dir.join("daemon-start-reject-test.request");
    let result_path = requests_dir.join("daemon-start-reject-test.result");
    infigraph_core::daemon_protocol::write_atomic(
        &request_path,
        &serde_json::to_string(
            &infigraph_core::daemon_protocol::WriteRequest::WatchControl {
                role: infigraph_core::daemon_protocol::WatchRole::Daemon,
                action: infigraph_core::daemon_protocol::WatchAction::Start,
            },
        )
        .unwrap(),
    )
    .unwrap();

    let mut replied = false;
    for _ in 0..150 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if result_path.exists() {
            replied = true;
            break;
        }
    }
    assert!(
        replied,
        "expected a reply to WatchControl {{ Daemon, Start }}"
    );
    let reply: infigraph_core::daemon_protocol::WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(
            reply,
            infigraph_core::daemon_protocol::WriteResult::Err { .. }
        ),
        "WatchControl {{ Daemon, Start }} is meaningless and must be an explicit error: {reply:?}"
    );

    assert!(
        !handle.is_finished(),
        "a rejected daemon-control request must not end the coordinator loop"
    );
    assert!(
        !daemon_token.is_cancelled(),
        "a rejected daemon-control request must not cancel background work"
    );

    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
}

/// Coverage for the `WatchRole::Docs` arm of `route_or_serve_request`'s
/// `WatchControl` dispatch, which had no test in either direction before
/// this: a request routes to the registered `docs_control` closure (passed
/// as `Some(...)` to `run_write_coordinator`) exactly once per request, with
/// the exact action requested, and the loop keeps running afterward -- Docs
/// actions, unlike `role: Daemon`, must never end the coordinator loop.
#[test]
fn watch_control_docs_role_dispatches_to_the_registered_docs_control() {
    let project = tempfile::Builder::new()
        .prefix("infigraph-watch-control-docs-dispatch-test-")
        .tempdir()
        .unwrap();
    std::fs::write(project.path().join("main.py"), "def main():\n    pass\n").unwrap();

    {
        let registry = infigraph_languages::bundled_registry().unwrap();
        let mut boot = infigraph_core::Infigraph::open(project.path(), registry).unwrap();
        boot.init().unwrap();
        boot.index().unwrap();
    }

    let received: std::sync::Arc<
        std::sync::Mutex<Vec<infigraph_core::daemon_protocol::WatchAction>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let received_for_control = std::sync::Arc::clone(&received);
    let docs_control: std::sync::Arc<infigraph_core::daemon::DocsControl> =
        std::sync::Arc::new(move |action| {
            received_for_control.lock().unwrap().push(action);
            Ok(())
        });

    let daemon_token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = daemon_token.clone();

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let root = project.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
            &token_for_thread,
            Some(docs_control),
        )
    });

    std::thread::sleep(std::time::Duration::from_millis(300));

    let requests_dir = project.path().join(".infigraph").join("requests");
    let request_path = requests_dir.join("watch-control-docs-dispatch-test.request");
    let result_path = requests_dir.join("watch-control-docs-dispatch-test.result");
    infigraph_core::daemon_protocol::write_atomic(
        &request_path,
        &serde_json::to_string(
            &infigraph_core::daemon_protocol::WriteRequest::WatchControl {
                role: infigraph_core::daemon_protocol::WatchRole::Docs,
                action: infigraph_core::daemon_protocol::WatchAction::Start,
            },
        )
        .unwrap(),
    )
    .unwrap();

    let mut replied = false;
    for _ in 0..150 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if result_path.exists() {
            replied = true;
            break;
        }
    }
    assert!(
        replied,
        "expected a reply to WatchControl {{ Docs, Start }}"
    );
    let reply: infigraph_core::daemon_protocol::WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    assert!(
        matches!(
            reply,
            infigraph_core::daemon_protocol::WriteResult::Ok { .. }
        ),
        "WatchControl {{ Docs, Start }} with a registered docs_control must succeed: {reply:?}"
    );
    assert_eq!(
        *received.lock().unwrap(),
        vec![infigraph_core::daemon_protocol::WatchAction::Start],
        "the registered docs_control closure must have been called exactly once, with Start"
    );

    assert!(
        !handle.is_finished(),
        "a Docs-role watch-control request must not end the coordinator loop"
    );

    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
}

/// The other half of `WatchRole::Docs` coverage: with no `docs_control`
/// registered (`None` passed to `run_write_coordinator`, matching a caller
/// that never wired up doc-watching), the request must come back as an
/// explicit error rather than silently doing nothing -- and must not end
/// the loop either.
#[test]
fn watch_control_docs_role_without_a_registered_control_replies_with_an_error() {
    let project = tempfile::Builder::new()
        .prefix("infigraph-watch-control-docs-none-test-")
        .tempdir()
        .unwrap();
    std::fs::write(project.path().join("main.py"), "def main():\n    pass\n").unwrap();

    {
        let registry = infigraph_languages::bundled_registry().unwrap();
        let mut boot = infigraph_core::Infigraph::open(project.path(), registry).unwrap();
        boot.init().unwrap();
        boot.index().unwrap();
    }

    let daemon_token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = daemon_token.clone();

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let root = project.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50, // debounce_ms
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true, // serve_requests
            None,
            &token_for_thread,
            None, // no docs_control registered
        )
    });

    std::thread::sleep(std::time::Duration::from_millis(300));

    let requests_dir = project.path().join(".infigraph").join("requests");
    let request_path = requests_dir.join("watch-control-docs-none-test.request");
    let result_path = requests_dir.join("watch-control-docs-none-test.result");
    infigraph_core::daemon_protocol::write_atomic(
        &request_path,
        &serde_json::to_string(
            &infigraph_core::daemon_protocol::WriteRequest::WatchControl {
                role: infigraph_core::daemon_protocol::WatchRole::Docs,
                action: infigraph_core::daemon_protocol::WatchAction::Stop,
            },
        )
        .unwrap(),
    )
    .unwrap();

    let mut replied = false;
    for _ in 0..150 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if result_path.exists() {
            replied = true;
            break;
        }
    }
    assert!(replied, "expected a reply to WatchControl {{ Docs, Stop }}");
    let reply: infigraph_core::daemon_protocol::WriteResult =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    match &reply {
        infigraph_core::daemon_protocol::WriteResult::Err { message } => {
            assert!(
                message.contains("does not own a doc-watch loop"),
                "unexpected error message: {message}"
            );
        }
        other => panic!("expected WriteResult::Err with no docs_control registered, got {other:?}"),
    }

    assert!(
        !handle.is_finished(),
        "a Docs-role watch-control request with no registered control must not end the loop"
    );

    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();
}
