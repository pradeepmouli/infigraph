//! #18: no startup phase may block the MCP handshake indefinitely. The
//! startup true-up (an incremental reindex, minutes on a large repo) runs in
//! the background; every phase that must finish before serving has a time
//! limit, and a phase that exceeds it fails startup naming the phase.
//!
//! `INFIGRAPH_MCP_DEBUG_STALL_STARTUP_PHASE=<name>` makes the named phase
//! hang, the same way `INFIGRAPH_MCP_DEBUG_STALL_TOOL` stalls a tool.

mod support;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};

fn spawn(scratch: &std::path::Path, stalled_phase: &str) -> std::process::Child {
    let startup = scratch.join("startup");
    std::fs::create_dir_all(&startup).unwrap();
    let mut cmd = support::isolated_mcp_command(scratch);
    cmd.arg("--mcp")
        .current_dir(&startup)
        .env("CI", "true")
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .env("INFIGRAPH_MCP_DEBUG_STALL_STARTUP_PHASE", stalled_phase)
        .env("INFIGRAPH_MCP_STARTUP_PHASE_SECS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd.spawn().expect("spawn infigraph-mcp")
}

#[test]
fn a_slow_startup_true_up_does_not_delay_the_handshake() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn(tmp.path(), "startup_true_up");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                let _ = tx.send(v);
            }
        }
    });
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
    writeln!(stdin, "{request}").unwrap();
    let reply = rx.recv_timeout(Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    let reply = reply.expect("initialize must be answered while the true-up is still running");
    assert_eq!(reply["id"], 1, "{reply}");
    // A result, not the error the supervisor sends when a worker dies with
    // the request pending (#21) -- that one carries id 1 as well.
    assert!(
        reply.get("result").is_some() && reply.get("error").is_none(),
        "the worker itself must answer: {reply}"
    );
}

#[test]
fn a_hung_startup_phase_fails_startup_naming_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = spawn(tmp.path(), "register_instance");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(20) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = child.kill();
    let _ = child.wait();
    let status = status.expect("a hung startup phase must end startup, not hang it");
    assert!(!status.success(), "{status:?}");
    let log = std::fs::read_to_string(tmp.path().join("mcp.log")).unwrap_or_default();
    assert!(
        log.contains("register_instance"),
        "the log must name the phase that hung:\n{log}"
    );
}
