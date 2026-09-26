//! #38/#124: a real `infigraph daemon` exits once nobody holds a lease on it
//! and nothing has touched it for its idle grace -- and not before.

use std::process::{Command, Stdio};
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

fn cli_binary() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    let deps_dir = exe.parent().unwrap();
    let candidate = deps_dir.join("infigraph");
    if candidate.exists() {
        return candidate;
    }
    deps_dir.parent().unwrap().join("infigraph")
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
    let st = Command::new(cli_binary())
        .arg("index")
        .current_dir(dir.path())
        .env("INFIGRAPH_BACKEND", "kuzu")
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success(), "indexing the fixture failed");
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
    let mut holder = Command::new(std::env::current_exe().unwrap())
        .args(["lease_holder_helper", "--exact", "--ignored", "--nocapture"])
        .env("LEASE_HOLDER_ROOT", project.path())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
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
