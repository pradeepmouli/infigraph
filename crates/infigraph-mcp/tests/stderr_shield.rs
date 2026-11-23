//! A client that closes (or never reads) the stderr pipe it gave an MCP server must not be able to kill it.
//!
//! `eprintln!` panics when its write fails, and the server has hundreds of plain `eprintln!`s across its
//! crates. The supervisor prints one whenever it restarts a crashed worker, on its main thread, so killing
//! the worker of a server whose stderr reader is gone is the real code path that used to take the whole
//! server down (exit 101). Driven through the real `infigraph-mcp` binary.

#![cfg(unix)]

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod support;

use support::{registered_worker, start_supervisor_stderr, Server, Transport};

const TRANSPORTS: [Transport; 2] = [Transport::Pipe, Transport::Socket];

/// Kill `pid` with SIGSEGV as a memory fault would; see `crash_recovery_scope.rs` for why one signal is not
/// enough (std's stack-overflow handler swallows the first).
fn crash(worker: u32) {
    for _ in 0..5 {
        // SAFETY: plain kill(2) on the worker this test's supervisor spawned.
        if unsafe { libc::kill(worker as libc::pid_t, libc::SIGSEGV) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Crash the worker and show the supervisor restarts it and the new worker answers.
fn crash_and_serve(s: &mut Server) {
    let first = registered_worker(&s.instances, None);
    crash(first);
    // The next worker registering, or the supervisor dying instead, whichever comes first.
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = s.child.try_wait().unwrap() {
            panic!(
                "the supervisor exited ({status:?}) after it restarted a crashed worker: its restart notice \
                 (a plain eprintln!) had nowhere to go"
            );
        }
        if worker_other_than(&s.instances, first).is_some() {
            break;
        }
        assert!(Instant::now() < until, "no restarted worker within 60s");
        std::thread::sleep(Duration::from_millis(100));
    }
    s.list_tools(1);
    assert!(s.reply(1, Duration::from_secs(60)).get("result").is_some());
}

/// A registered worker pid other than `not`, if one is registered now.
fn worker_other_than(instances: &std::path::Path, not: u32) -> Option<u32> {
    std::fs::read_dir(instances)
        .ok()?
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter_map(|v| v["pid"].as_u64().map(|p| p as u32))
        .find(|p| *p != not)
}

#[test]
fn a_supervisor_whose_stderr_reader_is_gone_survives_restarting_a_crashed_worker() {
    for transport in TRANSPORTS {
        let mut s = start_supervisor_stderr(transport, &[]);
        drop(s.stderr.take());
        crash_and_serve(&mut s);
    }
}

#[test]
fn a_client_that_keeps_reading_still_receives_the_supervisors_notice() {
    for transport in TRANSPORTS {
        keeps_reading(transport);
    }
}

fn keeps_reading(transport: Transport) {
    let mut s = start_supervisor_stderr(transport, &[]);
    let seen = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&seen);
    let mut pipe = s.stderr.take().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                return;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    crash_and_serve(&mut s);
    let until = Instant::now() + Duration::from_secs(10);
    while !seen.lock().unwrap().contains("worker crashed") {
        assert!(
            Instant::now() < until,
            "the restart notice never reached the client ({transport:?}):\n{}",
            seen.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_client_that_never_reads_cannot_stall_a_request() {
    for transport in TRANSPORTS {
        let mut s = start_supervisor_stderr(transport, &[]);
        // Held open, never read.
        let _unread = s.stderr.take().unwrap();
        crash_and_serve(&mut s);
    }
}

/// Closing the server's stdin ends it; its stderr must then reach end-of-file for the client, even though the
/// worker (a child of the supervisor) is the last to leave.
#[test]
fn the_clients_stderr_reaches_end_of_file_once_the_server_exits() {
    for transport in TRANSPORTS {
        eof_once_the_server_exits(transport);
    }
}

fn eof_once_the_server_exits(transport: Transport) {
    let mut s = start_supervisor_stderr(transport, &[]);
    let mut pipe = s.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        Instant::now()
    });
    let worker = registered_worker(&s.instances, None);
    s.close_stdin();
    let deadline = Instant::now() + Duration::from_secs(60);
    let exited = loop {
        if s.child.try_wait().unwrap().is_some() {
            break Instant::now();
        }
        assert!(
            Instant::now() < deadline,
            "the supervisor did not exit on stdin EOF (worker {worker})"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let eof = reader.join().unwrap();
    assert!(
        eof.saturating_duration_since(exited) < Duration::from_secs(5),
        "the client's {transport:?} stayed open after the supervisor exited"
    );
}
