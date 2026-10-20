//! #38/#124: a real `infigraph daemon` exits once nobody holds a lease on it
//! and nothing has touched it for its idle grace -- and not before.

mod support;

#[cfg(unix)]
use std::process::Command;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Polls `still_alive` until it reports false, or the budget runs out.
/// Returns how long the wait took, or `None` if it never went away.
/// Copied from `watch_daemon_docs.rs` (each test file is its own crate).
fn wait_until_gone(what: &str, mut still_alive: impl FnMut() -> bool) -> Option<Duration> {
    const BUDGET: Duration = Duration::from_secs(30);
    const STEP: Duration = Duration::from_millis(100);
    let start = Instant::now();
    loop {
        if !still_alive() {
            let waited = start.elapsed();
            if waited > Duration::from_secs(5) {
                eprintln!("[test] {what} took {waited:?} to go away (over the old 5s budget)");
            }
            return Some(waited);
        }
        if start.elapsed() >= BUDGET {
            return None;
        }
        std::thread::sleep(STEP);
    }
}

/// Kills and reaps the daemon on drop, so a failing assertion never leaves a
/// real daemon running against an abandoned tempdir.
struct KillOnDrop(std::process::Child);

impl std::ops::Deref for KillOnDrop {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn indexed_project() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("infigraph-idle-exit-")
        .tempdir()
        .unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join("a.py"), "def a():\n    pass\n").unwrap();
    let out = support::infigraph()
        .arg("index")
        .current_dir(dir.path())
        .env("INFIGRAPH_BACKEND", "kuzu")
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "indexing the fixture failed ({}):\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

fn spawn_daemon(root: &std::path::Path, grace: &str) -> KillOnDrop {
    let child = support::infigraph()
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
    let guard = KillOnDrop(child);
    let lock = root.join(".infigraph").join("watch.lock");
    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(&lock, Duration::from_secs(30)),
        "daemon never acquired watch.lock"
    );
    guard
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
    let daemon_pid = daemon.0.id();
    assert!(
        wait_until_gone("idle daemon", || daemon_alive(project.path())).is_some(),
        "a daemon with no lease and no requests must exit after its grace"
    );
    assert!(
        daemon.wait().unwrap().success(),
        "idle exit must be a clean exit"
    );
    assert!(
        infigraph_core::daemon::read_endpoint::ReadEndpoint::for_root(project.path())
            .connect()
            .is_err(),
        "the read endpoint must be released on idle exit"
    );

    // It left on purpose, and says so, until a daemon is running again.
    let exit = infigraph_core::daemon::idle_exit::last(project.path())
        .expect("an idle exit leaves its breadcrumb");
    assert_eq!(exit.pid, daemon_pid);
    let status = infigraph_core::daemon::control::describe_status(
        project.path(),
        &infigraph_core::daemon::control::query_status(project.path()),
    );
    assert!(
        status.contains("as designed"),
        "no watcher running reads as a death, not an exit: {status}"
    );
    let _restarted = spawn_daemon(project.path(), "0");
    assert!(
        infigraph_core::daemon::idle_exit::last(project.path()).is_none(),
        "a running daemon clears the last one's idle-exit breadcrumb"
    );
}

#[test]
fn a_leased_daemon_stays_up_and_exits_once_released() {
    let project = indexed_project();
    let _daemon = spawn_daemon(project.path(), "2");
    let lease = attach_lease(project.path());
    std::thread::sleep(Duration::from_secs(6)); // 3x the grace
    assert!(
        daemon_alive(project.path()),
        "a held lease must keep the daemon up"
    );
    drop(lease);
    assert!(wait_until_gone("released daemon", || daemon_alive(project.path())).is_some());
}

/// Review Focus 3: the kernel closes a SIGKILLed holder's socket, and the
/// daemon must treat that as the lease ending.
#[cfg(unix)]
#[test]
fn a_killed_lease_holder_releases_its_lease() {
    let project = indexed_project();
    let _daemon = spawn_daemon(project.path(), "2");
    // A separate process holding a lease: this test binary, re-exec'd into
    // `lease_holder_helper`, which attaches and sleeps.
    // `KillOnDrop` so a failed assertion below cannot leave the helper
    // asleep for its full 600s (#203 M6).
    let mut holder = KillOnDrop(
        Command::new(std::env::current_exe().unwrap())
            .args(["lease_holder_helper", "--exact", "--ignored", "--nocapture"])
            .env("LEASE_HOLDER_ROOT", project.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_secs(6));
    assert!(
        daemon_alive(project.path()),
        "the child's lease must keep the daemon up"
    );
    holder.kill().unwrap(); // SIGKILL on unix
    let _ = holder.wait();
    assert!(
        wait_until_gone("daemon after holder SIGKILL", || daemon_alive(
            project.path()
        ))
        .is_some()
    );
}

#[test]
#[ignore = "helper for a_killed_lease_holder_releases_its_lease; runs only when re-exec'd"]
fn lease_holder_helper() {
    let Some(root) = std::env::var_os("LEASE_HOLDER_ROOT") else {
        return;
    };
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

/// Client-side idle release, end to end: a lease this process stops using is
/// released, so the daemon idles out; the next routed use respawns a daemon
/// and leases it.
#[cfg(unix)]
#[test]
fn a_released_lease_lets_the_daemon_exit_and_the_next_use_respawns_it() {
    use infigraph_core::daemon::lease;
    let project = indexed_project();
    let _daemon = spawn_daemon(project.path(), "2");
    lease::set_release_after_for_test(project.path(), Duration::from_secs(1));
    lease::hold(project.path());
    assert!(
        wait_until_gone("daemon after its lease was released", || daemon_alive(
            project.path()
        ))
        .is_some(),
        "an idle client must release its lease, letting the daemon exit"
    );
    assert!(!lease::is_held(project.path()));

    lease::pin(project.path()); // keep the respawned daemon's lease for the asserts
    infigraph_core::daemon::lifecycle::ensure_daemon_for_routed_access(project.path()).unwrap();
    assert!(
        daemon_alive(project.path()),
        "the next use must respawn a daemon"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let leased = loop {
        let leases = infigraph_core::daemon::control::query_status(project.path())
            .map(|s| s.leases)
            .unwrap_or(0);
        if leases == 1 || Instant::now() >= deadline {
            break leases == 1;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    lease::unpin(project.path());
    let _ = support::infigraph()
        .arg("daemon-stop")
        .current_dir(project.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    assert!(leased, "the respawned daemon must be leased");
}

/// A doc watcher whose trigger never stops (a formatter rewriting a document
/// with the same bytes) reindexes nothing each time, but a reindex counts as
/// work in flight, so a daemon that is busy with one nearly all the time
/// never finds itself idle (a sittir worktree daemon ran 1d16h at ~99% CPU
/// with no lease and never idled out). The gap after a reindex scales with
/// its cost, so most checks find the daemon idle.
#[test]
fn a_daemon_whose_doc_watcher_keeps_reindexing_nothing_still_idles_out() {
    let project = indexed_project();
    let root = project.path();
    std::fs::write(root.join("README.md"), "# readme\n").unwrap();
    infigraph_core::docs_switch::set_docs_enabled(root, true).unwrap();
    let child = support::infigraph()
        .args(["daemon", "--debounce", "50"])
        .current_dir(root)
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env("INFIGRAPH_DAEMON_IDLE_GRACE_SECS", "2")
        .env("INFIGRAPH_DAEMON_IDLE_CHECK_SECS", "1")
        // Each doc reindex takes 2s, as a large document set does.
        .env("INFIGRAPH_TEST_DOC_REINDEX_PAD_MS", "2000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut daemon = KillOnDrop(child);
    let lock = root.join(".infigraph").join("watch.lock");
    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(&lock, Duration::from_secs(30)),
        "daemon never acquired watch.lock"
    );

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let churn = {
        let (stop, readme) = (stop.clone(), root.join("README.md"));
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::write(&readme, "# readme\n");
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    let gone = wait_until_gone("daemon with a churning doc watcher", || daemon_alive(root));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    churn.join().unwrap();

    assert!(
        gone.is_some_and(|waited| waited < Duration::from_secs(15)),
        "a daemon with no lease whose doc watcher reindexes nothing in a loop must idle out \
         (waited {gone:?})"
    );
    assert!(daemon.wait().unwrap().success());
}

/// A stop (here the out-of-band `watch.stop` file, the one that works when
/// the daemon cannot take a control frame) must not wait for a doc reindex
/// that never ends: the join on the doc-watch thread is bounded.
#[test]
fn a_stop_is_not_held_up_by_a_doc_reindex_that_never_finishes() {
    let project = indexed_project();
    let root = project.path();
    std::fs::write(root.join("README.md"), "# readme\n").unwrap();
    infigraph_core::docs_switch::set_docs_enabled(root, true).unwrap();
    let child = support::infigraph()
        .args(["daemon", "--debounce", "50"])
        .current_dir(root)
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env("INFIGRAPH_DAEMON_IDLE_GRACE_SECS", "0")
        // The catch-up reindex takes ten minutes.
        .env("INFIGRAPH_TEST_DOC_REINDEX_PAD_MS", "600000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut daemon = KillOnDrop(child);
    let lock = root.join(".infigraph").join("watch.lock");
    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(&lock, Duration::from_secs(30)),
        "daemon never acquired watch.lock"
    );
    // Let the catch-up reindex start and get stuck in its padding.
    std::thread::sleep(Duration::from_secs(3));

    std::fs::write(root.join(".infigraph").join("watch.stop"), b"").unwrap();
    let gone = wait_until_gone("daemon stopped during a doc reindex", || daemon_alive(root));

    assert!(
        gone.is_some(),
        "a stop was held up by a doc reindex (waited {gone:?})"
    );
    let _ = daemon.wait();
}
