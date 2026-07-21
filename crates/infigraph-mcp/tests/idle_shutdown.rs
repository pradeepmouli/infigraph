use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Spawns the real infigraph-mcp binary in --worker --ui --mcp mode,
/// closes its stdin immediately (simulating the MCP client disconnecting
/// right after the handshake), and asserts the process exits on its own
/// within a bounded window once the idle grace period elapses. Uses env
/// overrides (grace=2s, poll=1s) instead of the 300s production default so
/// this test runs in seconds, not minutes.
#[test]
fn worker_exits_after_idle_grace_following_stdin_close() {
    let exe = env!("CARGO_BIN_EXE_infigraph-mcp");
    let mut child = Command::new(exe)
        .args(["--worker", "--ui", "--mcp", "--port=0"])
        .env("INFIGRAPH_MCP_IDLE_GRACE_SECS", "2")
        .env("INFIGRAPH_MCP_IDLE_POLL_SECS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infigraph-mcp");

    // Close stdin immediately — the equivalent of the MCP client
    // disconnecting right after the handshake.
    drop(child.stdin.take());

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "process did not self-terminate within the idle grace period + margin"
        );
        std::thread::sleep(Duration::from_millis(200));
    };

    assert!(status.success(), "expected clean exit, got {status:?}");
}

/// Negative control enforcing the plan's critical scoping constraint: a
/// pure --ui-only invocation (no --mcp flag) never enters the stdin read
/// loop at all — it's a legitimate standing daemon with no stdio client to
/// ever disconnect from, and must NOT be affected by the idle-grace logic
/// even though the same INFIGRAPH_MCP_IDLE_* env vars are set. The sleep
/// window must clear process startup overhead (instance-lock acquisition,
/// rayon threadpool init, TCP bind — observed up to ~3.5s on a loaded
/// machine) plus the grace period plus one poll cycle with real margin, or
/// the test can report "still running" for the wrong reason.
#[test]
fn ui_only_daemon_without_mcp_flag_is_unaffected_by_idle_grace() {
    let exe = env!("CARGO_BIN_EXE_infigraph-mcp");
    let mut child = Command::new(exe)
        .args(["--worker", "--ui", "--port=0"])
        .env("INFIGRAPH_MCP_IDLE_GRACE_SECS", "2")
        .env("INFIGRAPH_MCP_IDLE_POLL_SECS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infigraph-mcp");

    std::thread::sleep(Duration::from_secs(8));
    let status = child.try_wait().expect("try_wait");
    assert!(
        status.is_none(),
        "a --ui-only (no --mcp) daemon must keep running past the idle grace period \
         window — it never entered the stdin loop, so idle-grace logic must not apply"
    );

    child.kill().expect("cleanup: kill still-running daemon");
    let _ = child.wait();
}
