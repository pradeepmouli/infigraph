//! A daemon that dies by a fatal signal while serving a rebuild must make
//! `infigraph rebuild` fail: a script, a hook or CI reads the exit status,
//! not the "Error:" line.
#![cfg(unix)]

mod support;

use std::process::Command;
use std::time::Duration;

#[test]
fn rebuild_exits_non_zero_when_the_daemon_dies_serving_it() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n").unwrap();
    // The daemon dies by SIGSEGV inside its next graph open while this file
    // exists (`recovery::TEST_DIE_IN_OPEN_WINDOW_ENV`): absent now, so the
    // bootstrap and the daemon's own start are healthy.
    let die = project.path().join("die-in-open");

    let env = |cmd: &mut Command| {
        cmd.current_dir(project.path())
            .env("HOME", home.path())
            .env(infigraph_core::BACKEND_ENV, "daemon")
            .env(infigraph_core::recovery::TEST_DIE_IN_OPEN_WINDOW_ENV, &die)
            .env_remove("CI")
            .env_remove("GITHUB_ACTIONS");
    };
    let mut bootstrap = support::infigraph();
    bootstrap.args(["index", "--no-embed"]);
    env(&mut bootstrap);
    assert!(bootstrap.status().unwrap().success(), "bootstrap index");

    // Persistent: the retry's daemon dies on its start as well.
    std::fs::write(&die, b"").unwrap();
    let mut rebuild = support::infigraph();
    rebuild.args(["rebuild", "--no-embed"]);
    env(&mut rebuild);
    let output = rebuild.output().unwrap();

    let _ = std::fs::remove_file(&die);
    if let Some(holder) =
        infigraph_core::lockfile::read_holder(&project.path().join(".infigraph/watch.lock"))
    {
        let _ = Command::new("kill").arg(holder.pid.to_string()).status();
    }
    std::thread::sleep(Duration::from_millis(200));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("the daemon exited while serving this request"),
        "the daemon was expected to die serving the rebuild:\n{stderr}"
    );
    assert!(
        stderr.contains("retrying once"),
        "the sentinel the dead daemon left is the evidence to retry on:\n{stderr}"
    );
    assert!(
        !output.status.success(),
        "rebuild must exit non-zero when it prints that error:\n{stderr}"
    );
}

#[test]
fn a_bare_rebuild_heals_a_daemon_that_dies_opening_the_graph_on_the_first_command() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n").unwrap();
    let die = project.path().join("die-in-open");

    let env = |cmd: &mut Command| {
        cmd.current_dir(project.path())
            .env("HOME", home.path())
            .env(infigraph_core::BACKEND_ENV, "daemon")
            .env(infigraph_core::recovery::TEST_DIE_IN_OPEN_WINDOW_ENV, &die)
            .env_remove("CI")
            .env_remove("GITHUB_ACTIONS");
    };
    let mut bootstrap = support::infigraph();
    bootstrap.args(["index", "--no-embed"]);
    env(&mut bootstrap);
    assert!(bootstrap.status().unwrap().success(), "bootstrap index");
    let mut stop = support::infigraph();
    stop.args(["daemon-stop", "--wait"]);
    env(&mut stop);
    assert!(
        stop.status().unwrap().success(),
        "stop the bootstrap daemon"
    );

    // The next daemon dies once inside its open, as one over a frozen WAL does.
    std::fs::write(&die, b"once").unwrap();
    let mut rebuild = support::infigraph();
    rebuild.args(["rebuild", "--no-embed"]);
    env(&mut rebuild);
    let output = rebuild.output().unwrap();

    let mut stop = support::infigraph();
    stop.args(["daemon-stop", "--wait"]);
    env(&mut stop);
    let _ = stop.status();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("daemon died opening the graph"),
        "the retry must be announced, not silent:\n{stderr}"
    );
    assert!(
        output.status.success(),
        "the first bare rebuild must succeed:\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
}
