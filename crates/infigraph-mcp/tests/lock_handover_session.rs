//! R2.3.2 (I-15 precondition): handing `mcp.lock` to a newer build must not
//! cost the incumbent's session its MCP server. The lock holder and the
//! session's worker are different roles that share a process; honoring a
//! handover used to exit that worker with status 0, which the supervisor
//! read as a deliberate shutdown and followed -- dropping the client's
//! connection. Driven through a real supervisor and worker; this test
//! process plays the newer-build challenger.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

mod support;

struct Server {
    _tmp: tempfile::TempDir,
    lock: PathBuf,
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

fn start() -> Server {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let startup = root.join("startup");
    std::fs::create_dir_all(&startup).unwrap();

    let mut cmd = support::isolated_mcp_command(&root);
    cmd.arg("--mcp")
        .current_dir(&startup)
        .env("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS", "1")
        .env("CI", "true")
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
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
        lock: root.join("mcp.lock"),
        instances: root.join("instances"),
        _tmp: tmp,
        child,
        stdin,
        replies,
    }
}

impl Server {
    fn send(&mut self, msg: Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn reply(&self, id: i64, budget: Duration) -> Value {
        let until = Instant::now() + budget;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.replies.recv_timeout(left) {
                Ok(v) if v["id"] == json!(id) => return v,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => panic!("no reply to {id} within {budget:?}"),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the supervisor closed stdout -- the session lost its server")
                }
            }
        }
    }
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
fn honoring_a_handover_keeps_the_incumbents_session_served() {
    let mut s = start();
    s.send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2024-11-05", "capabilities": {},
        "clientInfo": {"name": "test", "version": "0"}}}),
    );
    s.reply(1, Duration::from_secs(60));
    let first = registered_worker(&s.instances, None);
    assert_eq!(
        infigraph_core::lockfile::read_holder(&s.lock).map(|h| h.pid),
        Some(first),
        "the only worker must be primary"
    );

    // What a newer build's `acquire_with_takeover` writes, then polls for.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        s.lock.with_file_name("mcp.lock.handover"),
        json!({"pid": std::process::id(), "build_hash": "newer-build", "requested_at": now})
            .to_string(),
    )
    .unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    let _challenger = loop {
        if let Ok(Some(lock)) = infigraph_core::lockfile::try_acquire(&s.lock, "mcp-primary") {
            break lock;
        }
        assert!(
            Instant::now() < until,
            "the incumbent never released mcp.lock"
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    let replacement = registered_worker(&s.instances, Some(first));
    assert_ne!(replacement, first);
    assert!(
        s.child.try_wait().unwrap().is_none(),
        "the supervisor followed its worker out -- the session lost its server"
    );
    s.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let listed = s.reply(2, Duration::from_secs(60));
    assert!(
        listed["result"]["tools"].is_array(),
        "the session must still be served after the handover: {listed}"
    );
}
