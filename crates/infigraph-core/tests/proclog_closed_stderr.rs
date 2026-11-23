//! Logging and panic hooks must survive a stderr nobody reads any more.
//!
//! `eprintln!` panics when its write fails. An MCP server or daemon whose launching client has
//! closed the read end of its stderr pipe therefore died inside its own logging, and a panic
//! inside a panic hook aborts the process (SIGABRT). These tests run the code in a child process
//! whose stderr is a pipe with the read end already closed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHILD: &str = "INFIGRAPH_PROCLOG_CHILD";

/// The child side: wait until the parent has closed its end of our stderr pipe, then do `what`.
fn wait_for_go(dir: &Path) {
    let go = dir.join("go");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !go.exists() {
        assert!(Instant::now() < deadline, "the parent never said go");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Run this test binary again, as `test_name` only, with stderr a pipe whose reader is gone.
fn run_child(test_name: &str, dir: &Path) -> std::process::ExitStatus {
    let mut child = Command::new(std::env::current_exe().unwrap())
        // `--nocapture`: libtest otherwise captures a test thread's `eprintln!` and the pipe is never written.
        .args(["--exact", test_name, "--test-threads=1", "--nocapture"])
        .env(CHILD, dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stderr.take());
    std::fs::write(dir.join("go"), "go").unwrap();
    child.wait().unwrap()
}

fn child_dir() -> Option<PathBuf> {
    std::env::var_os(CHILD).map(PathBuf::from)
}

#[test]
fn child_writes_a_log_line() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    infigraph_core::proclog::write_log_line(&dir.join("proc.log"), "INFO", "line from the child");
}

#[test]
fn a_log_line_survives_a_stderr_whose_reader_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let status = run_child("child_writes_a_log_line", dir.path());
    assert!(
        status.success(),
        "the process died logging to a closed stderr pipe: {status:?}"
    );
    let log = std::fs::read_to_string(dir.path().join("proc.log")).unwrap_or_default();
    assert!(
        log.contains("INFO: line from the child"),
        "the line never reached the log file: {log:?}"
    );
}

#[test]
fn child_panics_with_a_hook_that_writes_to_stderr() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    // The MCP server's hook callback: a log line, then a stderr line.
    let log = dir.join("proc.log");
    infigraph_core::proclog::install_panic_hook(move |desc, bt| {
        infigraph_core::proclog::write_log_line(&log, "PANIC", &format!("{desc}\n{bt}"));
        infigraph_core::proclog::stderr_line(&format!("PANIC: {desc}"));
    });
    let caught = std::panic::catch_unwind(|| panic!("boom"));
    assert!(caught.is_err());
    std::fs::write(dir.join("survived"), "yes").unwrap();
}

#[test]
fn a_panic_hook_cannot_abort_the_process_on_a_closed_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let status = run_child("child_panics_with_a_hook_that_writes_to_stderr", dir.path());
    assert!(
        dir.path().join("survived").exists(),
        "the process died inside its panic hook: {status:?}"
    );
    assert!(status.success(), "unexpected exit: {status:?}");
    let log = std::fs::read_to_string(dir.path().join("proc.log")).unwrap_or_default();
    assert!(
        log.contains("PANIC: boom"),
        "the panic never reached the log file: {log:?}"
    );
}
