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

/// A second worker started while a stale (dead-PID) instance file already
/// exists in the shared registry dir must reap it on startup — proving the
/// scan-and-reap wiring runs, not just the pure logic it's built from.
#[test]
fn worker_reaps_stale_instance_file_on_startup() {
    let exe = env!("CARGO_BIN_EXE_infigraph-mcp");
    let dir = tempfile::tempdir().expect("tempdir");

    // A PID essentially guaranteed not to be a running process, with an
    // arbitrary recorded start time — current_process_start_time(999999)
    // will return None, so is_stale is unconditionally true regardless of
    // which PID the OS actually assigns.
    std::fs::write(
        dir.path().join("999999.json"),
        r#"{"pid":999999,"started_at":1,"project_path":"/dead","transport":"stdio","host_agent_hint":null}"#,
    )
    .unwrap();

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

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let has_dead_entry = dir.path().join("999999.json").exists();
        if !has_dead_entry {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stale instance file was never reaped"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    drop(child.stdin.take());
    let _ = child.wait();
}
