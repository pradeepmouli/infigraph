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

/// What the parent does with the read end of the child's stderr pipe.
#[derive(Clone, Copy, PartialEq)]
enum Reader {
    /// Closes it before the child writes: a client that went away.
    Closed,
    /// Reads it to the end.
    Reads,
    /// Holds it open and never reads: a client that stopped listening.
    Silent,
}

struct Outcome {
    status: std::process::ExitStatus,
    /// Everything the child's stderr delivered (`Reader::Reads` only).
    stderr: String,
    /// How long after the child exited its stderr reached end-of-file (`Reader::Reads` only).
    eof_after_exit: Duration,
    /// The child's whole run, from `go` to exit.
    elapsed: Duration,
}

/// Run this test binary again, as `test_name` only, with stderr a pipe the parent treats as `reader` says.
fn run_child(test_name: &str, dir: &Path, reader: Reader) -> Outcome {
    let mut child = Command::new(std::env::current_exe().unwrap())
        // `--nocapture`: libtest otherwise captures a test thread's `eprintln!` and the pipe is never written.
        .args(["--exact", test_name, "--test-threads=1", "--nocapture"])
        .env(CHILD, dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pipe = child.stderr.take().unwrap();
    let reading = match reader {
        Reader::Closed => {
            drop(pipe);
            None
        }
        Reader::Reads => Some(std::thread::spawn(move || {
            use std::io::Read as _;
            let mut pipe = pipe;
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            (String::from_utf8_lossy(&bytes).into_owned(), Instant::now())
        })),
        Reader::Silent => {
            // Kept open, never read; dropped when this function returns.
            std::mem::forget(pipe);
            None
        }
    };
    let started = Instant::now();
    std::fs::write(dir.join("go"), "go").unwrap();
    let deadline = started + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{test_name}: the child did not finish within 60s (a stalled writer?)");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let exited = Instant::now();
    let (stderr, eof_at) = match reading {
        Some(thread) => thread.join().unwrap(),
        None => (String::new(), exited),
    };
    Outcome {
        status,
        stderr,
        eof_after_exit: eof_at.saturating_duration_since(exited),
        elapsed: exited - started,
    }
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
    let status = run_child("child_writes_a_log_line", dir.path(), Reader::Closed).status;
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
    let status = run_child(
        "child_panics_with_a_hook_that_writes_to_stderr",
        dir.path(),
        Reader::Closed,
    )
    .status;
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

// ---- the stderr shield (`proclog::shield_stderr`): what the real stderr sees, and what the process survives.

/// A plain `eprintln!` on a closed pipe, with no shield: the premise (it panics).
#[test]
fn child_writes_plain_lines_unshielded() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    eprintln!("a plain line");
    std::fs::write(dir.join("survived"), "yes").unwrap();
}

#[test]
fn an_unshielded_plain_eprintln_dies_on_a_closed_reader() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_child(
        "child_writes_plain_lines_unshielded",
        dir.path(),
        Reader::Closed,
    );
    assert!(
        !out.status.success(),
        "the premise changed: {:?}",
        out.status
    );
    assert!(!dir.path().join("survived").exists());
}

/// `lines` plain `eprintln!`s behind the shield, then `exit` (not a return), which must drain the pipe.
#[test]
fn child_writes_plain_lines_shielded() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    infigraph_core::proclog::shield_stderr().unwrap();
    let lines: usize = std::fs::read_to_string(dir.join("lines"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    for i in 0..lines {
        eprintln!("shielded line {i:06} {}", "x".repeat(100));
    }
    std::fs::write(
        dir.join("dropped"),
        infigraph_core::proclog::dropped_stderr_bytes().to_string(),
    )
    .unwrap();
    std::fs::write(dir.join("survived"), "yes").unwrap();
    std::process::exit(0);
}

fn shielded_run(reader: Reader, lines: usize) -> (tempfile::TempDir, Outcome) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lines"), lines.to_string()).unwrap();
    let out = run_child("child_writes_plain_lines_shielded", dir.path(), reader);
    (dir, out)
}

#[test]
fn a_shielded_plain_eprintln_survives_a_closed_reader() {
    let (dir, out) = shielded_run(Reader::Closed, 5000);
    assert!(
        out.status.success(),
        "died on a closed stderr: {:?}",
        out.status
    );
    assert!(dir.path().join("survived").exists());
}

/// A short burst (well under any pipe's capacity, so nothing may be dropped) and an immediate `exit`: the
/// exit must wait for the forwarder to hand the burst over. Run many times, since the forwarder usually
/// wins the race by itself and only a missing drain loses lines now and then.
#[test]
fn a_reading_client_receives_every_line_even_when_the_process_exits_at_once() {
    for round in 0..40 {
        let (_dir, out) = shielded_run(Reader::Reads, 40);
        assert!(out.status.success(), "round {round}: {:?}", out.status);
        for i in 0..40 {
            assert!(
                out.stderr.contains(&format!("shielded line {i:06} ")),
                "round {round}: line {i} never reached the client ({} bytes arrived)",
                out.stderr.len()
            );
        }
    }
}

/// `write_stderr_async_safe` goes to the real stderr itself, so even a `_exit` right after it (no `atexit`, no
/// drain, the forwarder killed with the process) loses nothing.
#[test]
fn child_writes_straight_to_the_real_stderr_then_dies_at_once() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    infigraph_core::proclog::shield_stderr().unwrap();
    for i in 0..100 {
        infigraph_core::proclog::write_stderr_async_safe(
            format!("direct line {i:03}\n").as_bytes(),
        );
    }
    // SAFETY: `_exit` is the point: no handlers, no drain.
    unsafe { libc::_exit(0) };
}

#[test]
fn a_direct_write_survives_a_process_that_dies_at_once() {
    for round in 0..10 {
        let dir = tempfile::tempdir().unwrap();
        let out = run_child(
            "child_writes_straight_to_the_real_stderr_then_dies_at_once",
            dir.path(),
            Reader::Reads,
        );
        assert!(out.status.success(), "{:?}", out.status);
        for i in 0..100 {
            assert!(
                out.stderr.contains(&format!("direct line {i:03}\n")),
                "round {round}: direct line {i} lost"
            );
        }
    }
}

#[test]
fn a_client_that_never_reads_cannot_stall_the_process() {
    // 20,000 lines of ~120 bytes is 2.4 MB, far past any pipe's capacity.
    let (dir, out) = shielded_run(Reader::Silent, 20_000);
    assert!(out.status.success(), "{:?}", out.status);
    assert!(
        out.elapsed < Duration::from_secs(20),
        "the writer waited on a reader that never reads: {:?}",
        out.elapsed
    );
    let dropped: u64 = std::fs::read_to_string(dir.path().join("dropped"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        dropped > 1_000_000,
        "the shield should have dropped what could not be written, dropped {dropped}"
    );
}

/// The panic hook's line and a SIGABRT breadcrumb go straight to the real stderr, not into the shield's pipe
/// (which dies with the process).
#[test]
fn child_aborts_inside_a_phase_behind_the_shield() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    infigraph_core::proclog::shield_stderr().unwrap();
    infigraph_core::write_phase::install_abort_breadcrumb();
    let _phase = infigraph_core::write_phase::enter(&"shielded phase", 7);
    eprintln!("a plain line just before the abort");
    infigraph_core::proclog::install_panic_hook(|desc, _| {
        infigraph_core::proclog::stderr_line(&format!("HOOK: {desc}"));
    });
    // A panic whose hook line must survive the abort that follows at once.
    let _ = std::panic::catch_unwind(|| panic!("boom"));
    unsafe { libc::abort() };
}

#[test]
fn an_abort_behind_the_shield_still_leaves_the_breadcrumb_and_the_hook_line() {
    use std::os::unix::process::ExitStatusExt;
    let dir = tempfile::tempdir().unwrap();
    let out = run_child(
        "child_aborts_inside_a_phase_behind_the_shield",
        dir.path(),
        Reader::Reads,
    );
    assert_eq!(out.status.signal(), Some(libc::SIGABRT), "{:?}", out.status);
    assert!(
        out.stderr.contains("HOOK: boom at "),
        "hook line missing:\n{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("during write phase: shielded phase (n=7)"),
        "breadcrumb missing:\n{}",
        out.stderr
    );
}

/// A child of a shielded process inherits the shield's pipe, never the client's real stderr: once the process
/// exits the client sees end-of-file at once, even while that child lives on.
#[test]
fn child_leaves_a_sleeper_holding_its_stderr() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    infigraph_core::proclog::shield_stderr().unwrap();
    // `sleep` inherits fd 2 (the shield's pipe) and outlives this process.
    Command::new("sleep").arg("8").spawn().unwrap();
    eprintln!("last words");
    std::process::exit(0);
}

#[test]
fn a_surviving_child_does_not_keep_the_clients_stderr_open() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_child(
        "child_leaves_a_sleeper_holding_its_stderr",
        dir.path(),
        Reader::Reads,
    );
    assert!(out.status.success(), "{:?}", out.status);
    assert!(out.stderr.contains("last words"), "{}", out.stderr);
    assert!(
        out.eof_after_exit < Duration::from_secs(3),
        "the client's pipe stayed open {:?} after the server exited: a child holds it",
        out.eof_after_exit
    );
}

/// CHARACTERIZATION: a child that outlives its shielded parent inherits the shield's pipe, whose
/// reader died with the parent, so its next write to stderr fails. `std` puts SIGPIPE back to its default in
/// every child it spawns, so what the child does depends on how it writes. Pins what was measured.
#[test]
fn child_leaves_children_that_write_after_it_is_gone() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    infigraph_core::proclog::shield_stderr().unwrap();
    let d = dir.display();
    // A shell (SIGPIPE default): echo to stderr after the parent is gone.
    Command::new("sh")
        .args([
            "-c",
            &format!("sleep 1; echo late >&2; echo \"sh survived rc=$?\" > {d}/sh.out"),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // A shell that ignores SIGPIPE, as a Node or Python child does: the write fails with EPIPE instead.
    Command::new("sh")
        .args([
            "-c",
            &format!("trap '' PIPE; sleep 1; echo late >&2; echo \"sh(ignoring SIGPIPE) survived rc=$?\" > {d}/sh_ign.out"),
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::process::exit(0);
}

#[test]
fn what_a_child_that_outlives_the_shield_sees_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_child(
        "child_leaves_children_that_write_after_it_is_gone",
        dir.path(),
        Reader::Reads,
    );
    assert!(out.status.success(), "{:?}", out.status);
    std::thread::sleep(Duration::from_millis(2500));
    // Default SIGPIPE: killed by the signal on its first write, so it never got to leave its marker.
    assert!(
        !dir.path().join("sh.out").exists(),
        "a child writing to a shield pipe nobody reads any more is no longer killed by SIGPIPE"
    );
    // SIGPIPE ignored (as Node and Python do): the write fails (rc 1) and the child carries on.
    let ignoring = std::fs::read_to_string(dir.path().join("sh_ign.out")).unwrap_or_default();
    assert!(ignoring.contains("survived rc=1"), "{ignoring:?}");
}
