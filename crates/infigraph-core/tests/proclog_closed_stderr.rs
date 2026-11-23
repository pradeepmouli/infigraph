//! Logging and panic hooks must survive a stderr nobody reads any more, and the stderr shield
//! (`proclog::shield_stderr`) must keep every plain `eprintln!` from failing or waiting on one.
//!
//! `eprintln!` panics when its write fails. An MCP server or daemon whose launching client has
//! closed the read end of its stderr therefore died inside its own logging, and a panic inside a
//! panic hook aborts the process (SIGABRT). These tests run the code in a child process whose
//! stderr is the write end of a pipe or of a unix socket (real clients use both) whose other end
//! the parent closes, reads, or holds without reading.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `Transport` and `channel`: the pipe or socket that becomes a child's stderr (shared with infigraph-mcp's tests).
#[path = "common/stderr_channel.rs"]
mod stderr_channel;
use stderr_channel::{channel, Transport};

const CHILD: &str = "INFIGRAPH_PROCLOG_CHILD";

/// The child side: wait until the parent has set up our stderr, then do `what`.
fn wait_for_go(dir: &Path) {
    let go = dir.join("go");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !go.exists() {
        assert!(Instant::now() < deadline, "the parent never said go");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What the parent does with its end of the child's stderr.
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

/// Run this test binary again, as `test_name` only, with stderr a `transport` the parent treats as `reader` says.
fn run_child(test_name: &str, dir: &Path, reader: Reader, transport: Transport) -> Outcome {
    let (parent_end, child_end) = channel(transport);
    let mut child = Command::new(std::env::current_exe().unwrap())
        // `--nocapture`: libtest otherwise captures a test thread's `eprintln!` and nothing is ever written.
        .args(["--exact", test_name, "--test-threads=1", "--nocapture"])
        .env(CHILD, dir)
        .stdout(Stdio::null())
        .stderr(Stdio::from(child_end))
        .spawn()
        .unwrap();
    let mut held = None;
    let reading = match reader {
        Reader::Closed => {
            drop(parent_end);
            None
        }
        Reader::Reads => Some(std::thread::spawn(move || {
            use std::io::Read as _;
            let mut parent_end = parent_end;
            let mut bytes = Vec::new();
            let _ = parent_end.read_to_end(&mut bytes);
            (String::from_utf8_lossy(&bytes).into_owned(), Instant::now())
        })),
        Reader::Silent => {
            held = Some(parent_end);
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
    drop(held);
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

/// `#[test]`s for one body over both transports.
macro_rules! over_transports {
    ($body:ident => $pipe:ident, $socket:ident) => {
        #[test]
        fn $pipe() {
            $body(Transport::Pipe)
        }
        #[test]
        fn $socket() {
            $body(Transport::Socket)
        }
    };
}

// ---- logging and the panic hook, without the shield

#[test]
fn child_writes_a_log_line() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    infigraph_core::proclog::write_log_line(&dir.join("proc.log"), "INFO", "line from the child");
}

fn a_log_line_survives_a_stderr_whose_reader_is_gone(transport: Transport) {
    let dir = tempfile::tempdir().unwrap();
    let status = run_child(
        "child_writes_a_log_line",
        dir.path(),
        Reader::Closed,
        transport,
    )
    .status;
    assert!(
        status.success(),
        "the process died logging to a closed stderr ({transport:?}): {status:?}"
    );
    let log = std::fs::read_to_string(dir.path().join("proc.log")).unwrap_or_default();
    assert!(
        log.contains("INFO: line from the child"),
        "the line never reached the log file: {log:?}"
    );
}
over_transports!(a_log_line_survives_a_stderr_whose_reader_is_gone => log_line_survives_on_a_pipe, log_line_survives_on_a_socket);

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

fn a_panic_hook_cannot_abort_the_process_on_a_closed_stderr(transport: Transport) {
    let dir = tempfile::tempdir().unwrap();
    let status = run_child(
        "child_panics_with_a_hook_that_writes_to_stderr",
        dir.path(),
        Reader::Closed,
        transport,
    )
    .status;
    assert!(
        dir.path().join("survived").exists(),
        "the process died inside its panic hook ({transport:?}): {status:?}"
    );
    assert!(status.success(), "unexpected exit: {status:?}");
    let log = std::fs::read_to_string(dir.path().join("proc.log")).unwrap_or_default();
    assert!(
        log.contains("PANIC: boom"),
        "the panic never reached the log file: {log:?}"
    );
}
over_transports!(a_panic_hook_cannot_abort_the_process_on_a_closed_stderr => hook_survives_on_a_pipe, hook_survives_on_a_socket);

// ---- the stderr shield (`proclog::shield_stderr`): what the real stderr sees, and what the process survives.

/// A plain `eprintln!` on a closed stderr, with no shield: the premise (it panics).
#[test]
fn child_writes_plain_lines_unshielded() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    eprintln!("a plain line");
    std::fs::write(dir.join("survived"), "yes").unwrap();
}

fn an_unshielded_plain_eprintln_dies_on_a_closed_reader(transport: Transport) {
    let dir = tempfile::tempdir().unwrap();
    let out = run_child(
        "child_writes_plain_lines_unshielded",
        dir.path(),
        Reader::Closed,
        transport,
    );
    assert!(
        !out.status.success(),
        "the premise changed ({transport:?}): {:?}",
        out.status
    );
    assert!(!dir.path().join("survived").exists());
}
over_transports!(an_unshielded_plain_eprintln_dies_on_a_closed_reader => unshielded_dies_on_a_pipe, unshielded_dies_on_a_socket);

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

fn shielded_run(
    reader: Reader,
    transport: Transport,
    lines: usize,
) -> (tempfile::TempDir, Outcome) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lines"), lines.to_string()).unwrap();
    let out = run_child(
        "child_writes_plain_lines_shielded",
        dir.path(),
        reader,
        transport,
    );
    (dir, out)
}

fn a_shielded_plain_eprintln_survives_a_closed_reader(transport: Transport) {
    let (dir, out) = shielded_run(Reader::Closed, transport, 5000);
    assert!(
        out.status.success(),
        "died on a closed stderr ({transport:?}): {:?}",
        out.status
    );
    assert!(dir.path().join("survived").exists());
}
over_transports!(a_shielded_plain_eprintln_survives_a_closed_reader => shielded_survives_closed_pipe, shielded_survives_closed_socket);

/// A short burst (well under any capacity, so nothing may be dropped) and an immediate `exit`: the exit
/// must wait for the forwarder to hand the burst over. Run many times, since the forwarder usually wins the
/// race by itself and only a missing drain loses lines now and then.
fn a_reading_client_receives_every_line_even_when_the_process_exits_at_once(transport: Transport) {
    for round in 0..20 {
        let (_dir, out) = shielded_run(Reader::Reads, transport, 40);
        assert!(out.status.success(), "round {round}: {:?}", out.status);
        for i in 0..40 {
            assert!(
                out.stderr.contains(&format!("shielded line {i:06} ")),
                "{transport:?} round {round}: line {i} never reached the client ({} bytes arrived)",
                out.stderr.len()
            );
        }
    }
}
over_transports!(a_reading_client_receives_every_line_even_when_the_process_exits_at_once => reading_pipe_gets_every_line, reading_socket_gets_every_line);

fn a_client_that_never_reads_cannot_stall_the_process(transport: Transport) {
    // 20,000 lines of ~120 bytes is 2.4 MB, far past any pipe's or socket's capacity.
    let (dir, out) = shielded_run(Reader::Silent, transport, 20_000);
    assert!(out.status.success(), "{transport:?}: {:?}", out.status);
    assert!(
        out.elapsed < Duration::from_secs(20),
        "the writer waited on a reader that never reads ({transport:?}): {:?}",
        out.elapsed
    );
    let dropped: u64 = std::fs::read_to_string(dir.path().join("dropped"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        dropped > 1_000_000,
        "the shield should have dropped what could not be written ({transport:?}), dropped {dropped}"
    );
}
over_transports!(a_client_that_never_reads_cannot_stall_the_process => never_reading_pipe_cannot_stall, never_reading_socket_cannot_stall);

/// Many threads writing at once through every path (plain `eprintln!` into the shield, the panic hook's
/// `stderr_line`, the breadcrumb's `write_stderr_async_safe`) to a client that never reads. The forwarder
/// and the direct writers all poll-then-write the same descriptor; without one lock around that pair two of
/// them can both see room for one more piece and the second would block, stalling fd 2 for the whole process.
#[test]
fn child_hammers_stderr_from_many_threads() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    infigraph_core::proclog::shield_stderr().unwrap();
    let threads: Vec<_> = (0..8)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..3000 {
                    match i % 3 {
                        0 => eprintln!("hammer {t} {i} {}", "y".repeat(90)),
                        1 => infigraph_core::proclog::stderr_line(&format!(
                            "direct {t} {i} {}",
                            "z".repeat(90)
                        )),
                        _ => infigraph_core::proclog::write_stderr_async_safe(
                            format!("signal-safe {t} {i} {}\n", "w".repeat(90)).as_bytes(),
                        ),
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    // The forwarder must still be draining fd 2: one more plain line cannot block.
    eprintln!("after the hammering");
    std::fs::write(dir.join("survived"), "yes").unwrap();
    std::process::exit(0);
}

fn many_threads_writing_to_a_client_that_never_reads_finish(transport: Transport) {
    let dir = tempfile::tempdir().unwrap();
    let out = run_child(
        "child_hammers_stderr_from_many_threads",
        dir.path(),
        Reader::Silent,
        transport,
    );
    assert!(out.status.success(), "{transport:?}: {:?}", out.status);
    assert!(dir.path().join("survived").exists());
    assert!(
        out.elapsed < Duration::from_secs(20),
        "{transport:?}: writers stalled behind a reader that never reads: {:?}",
        out.elapsed
    );
}
over_transports!(many_threads_writing_to_a_client_that_never_reads_finish => hammer_a_silent_pipe, hammer_a_silent_socket);

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

fn a_direct_write_survives_a_process_that_dies_at_once(transport: Transport) {
    for round in 0..10 {
        let dir = tempfile::tempdir().unwrap();
        let out = run_child(
            "child_writes_straight_to_the_real_stderr_then_dies_at_once",
            dir.path(),
            Reader::Reads,
            transport,
        );
        assert!(out.status.success(), "{:?}", out.status);
        for i in 0..100 {
            assert!(
                out.stderr.contains(&format!("direct line {i:03}\n")),
                "{transport:?} round {round}: direct line {i} lost"
            );
        }
    }
}
over_transports!(a_direct_write_survives_a_process_that_dies_at_once => direct_write_survives_on_a_pipe, direct_write_survives_on_a_socket);

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
    // SAFETY: abort is the point.
    unsafe { libc::abort() };
}

fn an_abort_behind_the_shield_still_leaves_the_breadcrumb_and_the_hook_line(transport: Transport) {
    use std::os::unix::process::ExitStatusExt;
    let dir = tempfile::tempdir().unwrap();
    let out = run_child(
        "child_aborts_inside_a_phase_behind_the_shield",
        dir.path(),
        Reader::Reads,
        transport,
    );
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGABRT),
        "{transport:?}: {:?}",
        out.status
    );
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
over_transports!(an_abort_behind_the_shield_still_leaves_the_breadcrumb_and_the_hook_line => abort_leaves_breadcrumb_on_a_pipe, abort_leaves_breadcrumb_on_a_socket);

/// The same abort with the client's end already closed: the signal handler's write to a closed socket must
/// not raise SIGPIPE (which would replace the SIGABRT the process must still die by).
fn a_closed_peer_cannot_turn_the_abort_into_a_sigpipe(transport: Transport) {
    use std::os::unix::process::ExitStatusExt;
    let dir = tempfile::tempdir().unwrap();
    let out = run_child(
        "child_aborts_inside_a_phase_behind_the_shield",
        dir.path(),
        Reader::Closed,
        transport,
    );
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGABRT),
        "{transport:?}: the handler's write to a closed peer changed how the process dies: {:?}",
        out.status
    );
}
over_transports!(a_closed_peer_cannot_turn_the_abort_into_a_sigpipe => closed_pipe_keeps_sigabrt, closed_socket_keeps_sigabrt);

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

fn a_surviving_child_does_not_keep_the_clients_stderr_open(transport: Transport) {
    let dir = tempfile::tempdir().unwrap();
    let out = run_child(
        "child_leaves_a_sleeper_holding_its_stderr",
        dir.path(),
        Reader::Reads,
        transport,
    );
    assert!(out.status.success(), "{:?}", out.status);
    assert!(out.stderr.contains("last words"), "{}", out.stderr);
    assert!(
        out.eof_after_exit < Duration::from_secs(3),
        "the client's {transport:?} stayed open {:?} after the server exited: a child holds it",
        out.eof_after_exit
    );
}
over_transports!(a_surviving_child_does_not_keep_the_clients_stderr_open => sleeper_does_not_hold_the_pipe, sleeper_does_not_hold_the_socket);

/// CHARACTERIZATION: a child that outlives its shielded parent inherits the shield's pipe, whose reader died
/// with the parent, so its next write to stderr fails. `std` puts SIGPIPE back to its default in every child
/// it spawns, so what the child does depends on how it writes. Pins what was measured.
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
        Transport::Pipe,
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

/// The forwarder cannot go on (its read end ends): fd 2 must stop being a pipe nobody drains. The child
/// finds the shield's read end (the one read-only FIFO it holds), keeps the pipe alive with a duplicate that
/// never reads, and replaces the original descriptor with /dev/null, whose read returns end-of-file: the
/// forwarder's next read fails the way a broken read end would.
#[test]
fn child_loses_its_forwarder() {
    let Some(dir) = child_dir() else { return };
    wait_for_go(&dir);
    infigraph_core::proclog::shield_stderr().unwrap();
    let mut read_end = None;
    for fd in 3..256 {
        // SAFETY: probing descriptors with fstat/fcntl is harmless.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut st) != 0 || (st.st_mode & libc::S_IFMT) != libc::S_IFIFO {
                continue;
            }
            let acc = libc::fcntl(fd, libc::F_GETFL) & libc::O_ACCMODE;
            if acc == libc::O_RDONLY {
                read_end = Some(fd);
            }
        }
    }
    let read_end = read_end.expect("the shield's read end");
    // SAFETY: duplicating, then replacing, a descriptor this process owns.
    unsafe {
        // Never closed: the pipe keeps a reader that never reads, as a stranded fd 2 would meet.
        libc::dup(read_end);
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        libc::dup2(null, read_end);
        libc::close(null);
    }
    // The forwarder sleeps in poll on the old pipe, which the kernel does not wake when the descriptor is
    // replaced; its tick makes it look again, read end-of-file from /dev/null and give up.
    std::thread::sleep(Duration::from_millis(2500));
    // fd 2 must now be /dev/null: this volume is a hundred times what a pipe takes before it blocks.
    for i in 0..2000 {
        eprintln!("after the forwarder died {i:05} {}", "q".repeat(100));
    }
    std::fs::write(dir.join("survived"), "yes").unwrap();
    std::process::exit(0);
}

fn a_dead_forwarder_does_not_strand_fd_2(transport: Transport) {
    let dir = tempfile::tempdir().unwrap();
    let out = run_child(
        "child_loses_its_forwarder",
        dir.path(),
        Reader::Silent,
        transport,
    );
    assert!(out.status.success(), "{transport:?}: {:?}", out.status);
    assert!(dir.path().join("survived").exists());
}
over_transports!(a_dead_forwarder_does_not_strand_fd_2 => dead_forwarder_with_a_pipe_client, dead_forwarder_with_a_socket_client);
