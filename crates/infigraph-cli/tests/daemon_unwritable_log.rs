//! A daemon whose log can no longer be written must keep serving.
//!
//! The daemon's stderr is `daemon.log`. std's `eprintln!` panics when a write fails, so one failed log line
//! (a full disk, a size limit) took the daemon down with exit 101, and the daemon the next client started
//! died the same way while the cause lasted: the read service went with it and nothing said why. The
//! `eprintln!` the crates shadow (`infigraph_core::eprintln`) ignores the failure.
//!
//! The failing write is made with `RLIMIT_FSIZE` rather than a full volume, which cannot be arranged
//! reliably: the daemon's log is pre-sized to just under a 10 MiB limit, so its first lines cross it and the
//! write returns EFBIG (SIGXFSZ ignored, so the write fails instead of killing the process). The failing call is the same one a
//! full disk fails.

#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

mod support;

const LIMIT_BYTES: u64 = 10 * 1024 * 1024;

#[test]
fn a_daemon_whose_log_cannot_be_written_stays_up_answers_reads_and_stops_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("app.py"),
        "def greet(name):\n    return 'hi ' + name\n\n\ndef main():\n    return greet('x')\n",
    )
    .unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let bootstrap = support::run(
        &root,
        &home,
        infigraph_core::LOCAL_BACKEND,
        &["index", "--no-embed"],
    );
    support::assert_ok(&bootstrap, "bootstrap index");

    // The daemon's stderr, a file already 100 bytes short of the limit.
    let log_path = dir.path().join("daemon-stderr.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap();
    log.set_len(LIMIT_BYTES - 100).unwrap();

    let mut command = support::infigraph();
    command
        .args(["daemon", "--debounce", "50"])
        .current_dir(&root)
        .env("HOME", &home)
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .env_remove("INFIGRAPH_DOCS_ENABLED")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    // SAFETY: only async-signal-safe calls (setrlimit, signal) between fork and exec.
    unsafe {
        use std::os::unix::process::CommandExt as _;
        command.pre_exec(|| {
            // A file may not grow past 10 MiB; the write that would is refused (EFBIG) instead of raising
            // SIGXFSZ, which would kill the process.
            let limit = libc::rlimit {
                rlim_cur: LIMIT_BYTES as libc::rlim_t,
                rlim_max: LIMIT_BYTES as libc::rlim_t,
            };
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut daemon = support::Daemon(command.spawn().unwrap());

    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(
            &root.join(".infigraph").join("watch.lock"),
            Duration::from_secs(30)
        ),
        "the daemon never took watch.lock (exit: {:?})",
        daemon.0.try_wait()
    );

    // Give it time to log past the limit and to die of it, the way it used to.
    std::thread::sleep(Duration::from_secs(4));
    if let Some(status) = daemon.0.try_wait().unwrap() {
        panic!("the daemon exited ({status:?}) when its log became unwritable");
    }
    let log_len = std::fs::metadata(&log_path).unwrap().len();
    assert_eq!(
        log_len,
        LIMIT_BYTES,
        "the log should have hit its size limit, or this test no longer makes the daemon's writes fail; its tail: {:?}",
        String::from_utf8_lossy(&std::fs::read(&log_path).unwrap()[(LIMIT_BYTES - 100) as usize..log_len as usize])
    );

    // It still answers a read through its socket.
    let stats = support::run(&root, &home, "daemon", &["stats"]);
    support::assert_ok(&stats, "stats through the daemon");
    assert!(
        support::stdout(&stats).to_lowercase().contains("symbol"),
        "{}",
        support::stdout(&stats)
    );
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "the daemon went away while answering"
    );

    // And stops when told to, with a clean exit.
    let stop = support::run(&root, &home, "daemon", &["daemon-stop", "--wait"]);
    support::assert_ok(&stop, "daemon-stop");
    let status = loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            break status;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "the daemon's exit after daemon-stop: {status:?}"
    );
}
