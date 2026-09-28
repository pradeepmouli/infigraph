//! #155: `infigraph daemon-stop` falls back to the watch.stop sentinel when
//! the daemon cannot parse a Control frame.

use std::time::Duration;

#[test]
fn daemon_stop_against_an_incompatible_daemon_writes_the_sentinel() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let lock = root.join(".infigraph").join("watch.lock");
    let _held = infigraph_core::lockfile::try_acquire(&lock, "old-daemon")
        .unwrap()
        .unwrap();
    let listener = infigraph_core::daemon::read_endpoint::ReadEndpoint::for_root(&root)
        .bind()
        .unwrap();
    let fake = std::thread::spawn(move || {
        // Accept one connection, read its frame as a real daemon's
        // `serve_one` would, and close it unanswered: a build that cannot
        // parse the frame.
        if let Ok(Some(mut s)) = listener.accept_timeout(Duration::from_secs(20)) {
            use std::io::Read as _;
            let mut len = [0u8; 4];
            if s.read_exact(&mut len).is_ok() {
                let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
                let _ = s.read_exact(&mut body);
            }
            drop(s);
        }
    });

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .arg("daemon-stop")
        .current_dir(&root)
        .env("INFIGRAPH_BACKEND", "kuzu")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .unwrap();
    fake.join().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(root.join(".infigraph").join("watch.stop").exists());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("incompatible"), "{stdout}");
}

/// A wedged coordinator answers a stop with a refusal (queue full, or its
/// own 30s timeout) rather than hanging up. That is the case the sentinel
/// fallback exists for, so `daemon-stop` must use it there too.
#[test]
fn daemon_stop_refused_by_a_wedged_coordinator_writes_the_sentinel() {
    use infigraph_core::daemon::read_protocol::{write_reply, OpReply};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let lock = root.join(".infigraph").join("watch.lock");
    let _held = infigraph_core::lockfile::try_acquire(&lock, "wedged-daemon")
        .unwrap()
        .unwrap();
    let listener = infigraph_core::daemon::read_endpoint::ReadEndpoint::for_root(&root)
        .bind()
        .unwrap();
    let fake = std::thread::spawn(move || {
        if let Ok(Some(mut s)) = listener.accept_timeout(Duration::from_secs(20)) {
            use std::io::Read as _;
            let mut len = [0u8; 4];
            if s.read_exact(&mut len).is_ok() {
                let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
                let _ = s.read_exact(&mut body);
            }
            let _ = write_reply::<_, ()>(
                &mut s,
                &OpReply::Err(infigraph_core::daemon::coordinator_port::BUSY.to_string()),
            );
        }
    });

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .arg("daemon-stop")
        .current_dir(&root)
        .env("INFIGRAPH_BACKEND", "kuzu")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .unwrap();
    fake.join().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(root.join(".infigraph").join("watch.stop").exists());
}
