//! Shared by the CLI integration tests that spawn the real binary: each
//! declares `mod support;`.

#![allow(dead_code)] // every test file compiles its own copy, and uses some of it

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The `infigraph` binary under test, with implicit SCIP enrichment off.
///
/// `infigraph index` otherwise leaves a detached `scip-enrich` child running
/// after it returns, and that child keeps writing into the throwaway
/// project's `.infigraph/` -- racing the tempdir's (or a `git worktree
/// remove`'s) deletion of it, and, for a language with an indexer, spending
/// minutes of CPU per fixture. The env name comes from the settings
/// definition, never a string literal here. A test that is about enrichment
/// builds its own command instead (`scip_enrich_teardown.rs`).
pub fn infigraph() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_infigraph"));
    command.env(infigraph_core::scip_switch::enabled_env_name(), "0");
    command
}

// --- shared by the tests that drive a real project through the CLI and a daemon ---

/// Kills and reaps the daemon on every exit path, panics included.
pub struct Daemon(pub std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Run one CLI command. `INFIGRAPH_NO_WATCH` keeps the pre-dispatch
/// auto-watch from starting a daemon the test did not ask for.
pub fn run(root: &Path, home: &Path, backend: &str, args: &[&str]) -> Output {
    infigraph()
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env(infigraph_core::BACKEND_ENV, backend)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_DOCS_ENABLED")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .unwrap()
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

pub fn assert_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed:\nstdout={}\nstderr={}",
        stdout(out),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Index the code locally, then start a real daemon with a fast doc poll
/// and wait until it holds `watch.lock`.
pub fn start_daemon(root: &Path, home: &Path) -> Daemon {
    let bootstrap = run(
        root,
        home,
        infigraph_core::LOCAL_BACKEND,
        &["index", "--no-embed"],
    );
    assert_ok(&bootstrap, "bootstrap index");
    let daemon = Daemon(
        infigraph()
            .args(["daemon", "--debounce", "50"])
            .current_dir(root)
            .env("HOME", home)
            .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
            .env("INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS", "50")
            .env_remove("INFIGRAPH_DOCS_ENABLED")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(
            &root.join(".infigraph").join("watch.lock"),
            Duration::from_secs(30)
        ),
        "the daemon never took watch.lock"
    );
    daemon
}

/// Poll `check` for up to `budget`.
pub fn eventually(budget: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check()
}
