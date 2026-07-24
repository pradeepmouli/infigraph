use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Spawns the real infigraph-mcp binary and asserts it writes its own
/// instance file under INFIGRAPH_INSTANCES_DIR while running, and removes
/// it again after a clean stdin-close shutdown (the same idle-grace path
/// R2.2.3 already exercises, with a short grace so this test stays fast).
#[test]
fn worker_registers_and_deregisters_instance_file() {
    let exe = env!("CARGO_BIN_EXE_infigraph-mcp");
    let dir = tempfile::tempdir().expect("tempdir");

    let mut child = Command::new(exe)
        .args(["--worker", "--ui", "--mcp", "--port=0"])
        .env("INFIGRAPH_INSTANCES_DIR", dir.path())
        .env("INFIGRAPH_MCP_IDLE_GRACE_SECS", "2")
        .env("INFIGRAPH_MCP_IDLE_POLL_SECS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infigraph-mcp");

    // Give it a moment to reach the registration point.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count = std::fs::read_dir(dir.path())
            .map(|d| d.flatten().count())
            .unwrap_or(0);
        if count == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "instance file was never written");
        std::thread::sleep(Duration::from_millis(100));
    }

    drop(child.stdin.take());

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(Instant::now() < deadline, "process did not self-terminate");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(status.success());

    let remaining = std::fs::read_dir(dir.path())
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert_eq!(
        remaining, 0,
        "instance file must be removed on clean shutdown"
    );
}
