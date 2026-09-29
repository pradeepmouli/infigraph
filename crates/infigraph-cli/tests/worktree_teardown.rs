use std::process::Command;

fn git(args: &[&str], cwd: &std::path::Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn run_index(root: &std::path::Path, fake_home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["--root", root.to_str().unwrap(), "index"])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .output()
        .unwrap()
}

fn run_worktree(
    action: &str,
    path: &std::path::Path,
    fake_home: &std::path::Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["worktree", action, path.to_str().unwrap()])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .output()
        .unwrap()
}

#[test]
fn worktree_teardown_evicts_registry_entry_but_keeps_infigraph_dir() {
    let fake_home = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    git(&["init"], main.path());
    git(&["config", "user.email", "t@t.com"], main.path());
    git(&["config", "user.name", "t"], main.path());
    std::fs::write(main.path().join("a.py"), "def foo():\n    pass\n").unwrap();
    git(&["add", "a.py"], main.path());
    git(&["commit", "-m", "init"], main.path());

    let parent = tempfile::tempdir().unwrap();
    let wt_path = parent.path().join("wt1");
    git(
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            wt_path.to_str().unwrap(),
        ],
        main.path(),
    );

    // Register the worktree the honest way: index it for real.
    let out = run_index(&wt_path, fake_home.path());
    assert!(out.status.success());
    let registry_path = fake_home.path().join(".infigraph/registry.json");
    let before = std::fs::read_to_string(&registry_path).unwrap();
    assert!(before.contains(wt_path.file_name().unwrap().to_str().unwrap()));

    let out = run_worktree("teardown", &wt_path, fake_home.path());
    assert!(
        out.status.success(),
        "teardown failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let after = std::fs::read_to_string(&registry_path).unwrap();
    assert!(
        !after.contains(&wt_path.to_string_lossy().to_string()),
        "registry.json should no longer reference {}:\n{after}",
        wt_path.display()
    );
    assert!(
        wt_path.join(".infigraph/embeddings.bin").exists(),
        ".infigraph/ must survive teardown"
    );
}

/// Teardown reaches a daemon over its socket, which lives outside the
/// worktree, so it works after `git worktree remove` deleted the directory
/// -- and through the path's raw spelling, which on macOS (`/var` for
/// `/private/var`) differs from the canonical root the daemon bound under.
/// A stand-in daemon, because a real one exits on its own once its root is
/// gone (#136) and would race the request this test is about.
#[test]
fn worktree_teardown_stops_the_daemon_after_the_directory_is_gone() {
    use infigraph_core::daemon::read_protocol::{write_reply, OpReply};
    use std::time::Duration;

    let fake_home = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let wt_path = parent.path().join("wt1");
    std::fs::create_dir_all(wt_path.join(".infigraph")).unwrap();
    let listener = infigraph_core::daemon::read_endpoint::ReadEndpoint::for_root(
        &wt_path.canonicalize().unwrap(),
    )
    .bind()
    .unwrap();
    let daemon = std::thread::spawn(move || {
        let mut s = listener
            .accept_timeout(Duration::from_secs(20))
            .ok()
            .flatten()?;
        use std::io::Read as _;
        let mut len = [0u8; 4];
        s.read_exact(&mut len).ok()?;
        let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
        s.read_exact(&mut body).ok()?;
        write_reply::<_, ()>(&mut s, &OpReply::Ok(())).ok()?;
        Some(String::from_utf8_lossy(&body).into_owned())
    });

    std::fs::remove_dir_all(&wt_path).unwrap();
    let out = run_worktree("teardown", &wt_path, fake_home.path());
    let frame = daemon.join().unwrap();

    assert!(
        out.status.success(),
        "teardown failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let frame = frame.expect("teardown never reached the daemon's socket");
    assert!(
        frame.contains("control") && frame.contains("Stop"),
        "teardown must send a control stop, got {frame}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Stopped the daemon"), "{stdout}");
}

/// End to end against a real daemon on a real worktree: after `git worktree
/// remove`, teardown succeeds, evicts the entry, and the daemon is gone.
#[test]
/// The removal the worktree hook drives: `daemon-stop --wait` inside the
/// worktree (PreToolUse), `git worktree remove`, then `worktree teardown`
/// (PostToolUse). Removing with the daemon still running is not a flow any
/// more: a daemon writing into .infigraph/ while git deletes it makes the
/// remove fail half-done ("Directory not empty"), which is how this test
/// flaked on macOS CI before it followed the hook.
fn the_hook_flow_removes_a_worktree_with_a_live_daemon_cleanly() {
    use std::time::{Duration, Instant};

    let fake_home = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    git(&["init", "-q"], main.path());
    git(&["config", "user.email", "t@t.com"], main.path());
    git(&["config", "user.name", "t"], main.path());
    std::fs::write(main.path().join("a.py"), "def foo():\n    pass\n").unwrap();
    git(&["add", "a.py"], main.path());
    git(&["commit", "-q", "-m", "init"], main.path());
    let parent = tempfile::tempdir().unwrap();
    let wt_path = parent.path().join("wt1");
    git(
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt_path.to_str().unwrap(),
        ],
        main.path(),
    );
    assert!(run_index(&wt_path, fake_home.path()).status.success());

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["daemon", "--debounce", "50"])
        .current_dir(&wt_path)
        .env("HOME", fake_home.path())
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let reachable = || {
        infigraph_core::daemon::read_endpoint::ReadEndpoint::for_root(&wt_path)
            .connect()
            .is_ok()
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !reachable() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let listening = reachable();
    assert!(listening, "the daemon never bound its read endpoint");

    let stop = Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["daemon-stop", "--wait"])
        .current_dir(&wt_path)
        .env("HOME", fake_home.path())
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "daemon-stop --wait failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    // `--wait` returned, so the process has exited: reaping it is immediate.
    let deadline = Instant::now() + Duration::from_secs(5);
    let exited = loop {
        if daemon.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if !exited {
        let _ = daemon.kill();
        let _ = daemon.wait();
    }
    assert!(
        exited,
        "daemon-stop --wait returned before the daemon exited"
    );

    git(
        &["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        main.path(),
    );
    assert!(!wt_path.exists(), "the worktree directory must be gone");
    let out = run_worktree("teardown", &wt_path, fake_home.path());
    assert!(
        out.status.success(),
        "teardown failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let registry =
        std::fs::read_to_string(fake_home.path().join(".infigraph/registry.json")).unwrap();
    assert!(
        !registry.contains("wt1"),
        "teardown must evict the removed worktree:\n{registry}"
    );
}
