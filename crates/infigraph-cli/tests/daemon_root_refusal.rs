//! #207: a daemon at a root that is not a project refuses before it does any
//! work. One started in `$HOME` used to attach a doc watcher to the whole home
//! directory first, and only the code watcher's later check turned it away.

use std::time::{Duration, Instant};

#[test]
fn a_daemon_at_the_global_store_root_refuses_before_watching_anything() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();
    std::fs::write(tmp.path().join(".infigraph").join("registry.json"), "{}").unwrap();
    // The global store has a documents store, and the doc watcher attaches
    // to any root whose `docs.kuzu` exists.
    std::fs::write(tmp.path().join(".infigraph").join("docs.kuzu"), b"").unwrap();
    std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
    std::fs::write(tmp.path().join("docs").join("readme.md"), "# Hi\n").unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .current_dir(tmp.path())
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .arg("daemon")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the daemon must refuse, not keep running");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("home directory"), "{stderr}");
    // Refused before any startup work: `cmd_daemon` starts its doc watcher
    // before the write coordinator ever checks the root, and in `$HOME` that
    // left long enough for the watcher to attach to the whole home directory.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("Watching"),
        "started watching first: {stdout}"
    );
    assert!(
        !stderr.contains("[read]"),
        "started the read service first: {stderr}"
    );
    assert!(
        !stderr.contains("doc watcher"),
        "attached a doc watcher first: {stderr}"
    );
}
