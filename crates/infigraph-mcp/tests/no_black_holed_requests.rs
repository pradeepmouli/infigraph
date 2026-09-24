//! R5.6 (#21): no request is ever left unanswered. The supervisor answers
//! a call the worker hung on once its deadline passes -- restarting the
//! worker and answering what queued behind it -- and answers a call in
//! flight when the worker crashes, naming the crash. Driven through a real
//! supervisor and worker, with a debug-build hook that makes one tool hang.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

mod support;

const STALLED: &str = "get_stats";

struct Server {
    _tmp: tempfile::TempDir,
    instances: PathBuf,
    child: Child,
    stdin: ChildStdin,
    replies: Receiver<Value>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A supervisor whose worker hangs on every `get_stats` call.
fn start(extra_env: &[(&str, &str)]) -> Server {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let startup = root.join("startup");
    std::fs::create_dir_all(&startup).unwrap();
    let instances = root.join("instances");

    let mut cmd = support::isolated_mcp_command(&root);
    cmd.arg("--mcp")
        .current_dir(&startup)
        .env("INFIGRAPH_MCP_DEBUG_STALL_TOOL", STALLED)
        .env("CI", "true")
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn infigraph-mcp supervisor");
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, replies) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    return;
                }
            }
        }
    });
    Server {
        _tmp: tmp,
        instances,
        child,
        stdin,
        replies,
    }
}

impl Server {
    fn call(&mut self, id: i64, tool: &str) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                         "params": {"name": tool, "arguments": {}}}));
    }

    /// `tools/list`: answered instantly even by a freshly started debug
    /// worker, unlike any tool that builds the language registry.
    fn list_tools(&mut self, id: i64) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/list"}));
    }

    fn send(&mut self, msg: Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// The reply to `id`, within `budget`.
    fn reply(&self, id: i64, budget: Duration) -> Value {
        let until = Instant::now() + budget;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.replies.recv_timeout(left) {
                Ok(v) if v["id"] == json!(id) => return v,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => panic!("no reply to {id} within {budget:?}"),
                Err(RecvTimeoutError::Disconnected) => panic!("the supervisor closed stdout"),
            }
        }
    }
}

fn error_message(reply: &Value) -> &str {
    reply["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an error reply, got {reply}"))
}

/// A registered worker other than `not`, waiting up to a minute for one.
fn registered_worker(instances: &Path, not: Option<u32>) -> u32 {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let found = std::fs::read_dir(instances)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
            .filter_map(|v| v["pid"].as_u64().map(|p| p as u32))
            .find(|p| Some(*p) != not);
        if let Some(pid) = found {
            return pid;
        }
        assert!(Instant::now() < until, "no worker registered within 60s");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_call_past_its_deadline_is_answered_and_the_worker_restarted() {
    let mut s = start(&[("INFIGRAPH_MCP_CALL_TIMEOUT_SECS", "2")]);
    let first = registered_worker(&s.instances, None);
    s.call(1, STALLED);
    s.list_tools(2); // queued behind the hung call

    let started = Instant::now();
    let late = s.reply(1, Duration::from_secs(30));
    assert!(
        error_message(&late).contains("no reply within 2s"),
        "{late}"
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    let queued = s.reply(2, Duration::from_secs(5));
    assert!(
        error_message(&queued).contains("tools/list was queued behind a get_stats call"),
        "{queued}"
    );

    // The restarted worker serves the next request. Waiting for it to
    // register first: a debug-build worker can take longer than this test's
    // 2s deadline just to start.
    registered_worker(&s.instances, Some(first));
    s.list_tools(3);
    let served = s.reply(3, Duration::from_secs(60));
    assert!(served.get("result").is_some(), "{served}");
}

#[test]
fn a_call_in_flight_when_the_worker_crashes_is_answered_naming_the_crash() {
    let mut s = start(&[]);
    // A served call first: the worker is up and registered after it.
    s.call(1, "list_languages");
    assert!(s.reply(1, Duration::from_secs(60)).get("result").is_some());
    let worker = registered_worker(&s.instances, None);

    s.call(2, STALLED);
    std::thread::sleep(Duration::from_millis(500));
    // As crash_recovery_scope.rs explains, the first kill(2)'d SIGSEGV can
    // be swallowed by std's stack-overflow handler; repeat until it lands.
    for _ in 0..5 {
        // SAFETY: plain kill(2) on the worker this test's supervisor spawned.
        if unsafe { libc::kill(worker as libc::pid_t, libc::SIGSEGV) } != 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }

    let crashed = s.reply(2, Duration::from_secs(30));
    let msg = error_message(&crashed);
    assert!(
        msg.contains("crashed (SIGSEGV)") && msg.contains("get_stats"),
        "{crashed}"
    );

    s.call(3, "list_languages");
    assert!(s.reply(3, Duration::from_secs(60)).get("result").is_some());
}

/// R5.2 (#19): a worker over its hard ceiling restarts itself between
/// calls, and the supervisor starts a fresh worker rather than exiting.
/// A thread ceiling of 1 is breached by any process.
#[test]
fn a_worker_over_its_hard_ceiling_is_replaced_and_the_supervisor_stays_up() {
    let mut s = start(&[
        ("INFIGRAPH_WATCHDOG_THREADS_SOFT", "1"),
        ("INFIGRAPH_WATCHDOG_THREADS_HARD", "1"),
        ("INFIGRAPH_WATCHDOG_INTERVAL_SECS", "1"),
    ]);
    let first = registered_worker(&s.instances, None);
    let second = registered_worker(&s.instances, Some(first));
    assert_ne!(first, second);
    assert!(
        s.child.try_wait().unwrap().is_none(),
        "a watchdog restart is not a reason for the supervisor to exit"
    );
}
