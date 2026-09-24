//! R5.2 (#19): a daemon over its hard ceiling shuts itself down cleanly
//! once nothing is in flight, saying why; the next request starts a fresh
//! one. A thread ceiling of 1 is breached by any process.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_daemon_over_its_hard_ceiling_shuts_down_and_says_why() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n").unwrap();
    let cli = env!("CARGO_BIN_EXE_infigraph");

    let bootstrap = Command::new(cli)
        .arg("index")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .status()
        .unwrap();
    assert!(bootstrap.success(), "bootstrap index must succeed");

    let mut daemon = Command::new(cli)
        .arg("daemon")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .env("INFIGRAPH_WATCHDOG_THREADS_SOFT", "1")
        .env("INFIGRAPH_WATCHDOG_THREADS_HARD", "1")
        .env("INFIGRAPH_WATCHDOG_INTERVAL_SECS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let until = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > until {
            let _ = daemon.kill();
            panic!("the daemon did not shut itself down within 60s");
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let mut log = String::new();
    daemon
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut log)
        .unwrap();
    assert!(
        status.success(),
        "a clean shutdown, not a crash: {status:?}\n{log}"
    );
    assert!(
        log.contains("watchdog:") && log.contains("hard ceiling"),
        "the daemon must say why it left:\n{log}"
    );
}
