//! #124, supervisor half: the worker (and what it started) goes down with its
//! supervisor, promptly, and nothing else does. Driven through real
//! processes: a real supervisor and worker, a debug-build hook that keeps the
//! worker busy, and a fake `infigraph` beside a hard link of the MCP binary
//! so a tool call leaves the worker with a child of its own.
//!
//! Before this the worker shared the client's process group and noticed a
//! dead supervisor only on its 5s poll; whatever it had spawned (an
//! `infigraph index-docs`) ran on to the end of its job.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::json;

mod support;

use support::{registered_worker, start_supervisor, start_supervisor_with, Server};

const STALLED: &str = "get_stats";

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn signal(pid: u32, sig: i32) {
    // SAFETY: a plain kill(2) to a pid this test spawned or discovered.
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

fn gone_within(pid: u32, budget: Duration) -> bool {
    let until = Instant::now() + budget;
    while alive(pid) {
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    true
}

/// Kills the pids it was given when dropped, so a failing (red) test does not
/// leave a 600s sleeper behind. Only pids this test learned about.
struct Cleanup(Vec<u32>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for pid in &self.0 {
            signal(*pid, libc::SIGKILL);
        }
    }
}

fn busy_server() -> Server {
    start_supervisor(&[("INFIGRAPH_MCP_DEBUG_STALL_TOOL", STALLED)])
}

/// A supervisor whose worker, on `index_docs`, runs a fake `infigraph` that
/// records its own pid and its `sleep` child's, then waits.
struct WithChild {
    server: Server,
    pids: PathBuf,
    _bin: tempfile::TempDir,
}

fn with_cli_child() -> WithChild {
    let bin = tempfile::tempdir().unwrap();
    let mcp = bin.path().join("infigraph-mcp");
    std::fs::hard_link(env!("CARGO_BIN_EXE_infigraph-mcp"), &mcp)
        .or_else(|_| std::fs::copy(env!("CARGO_BIN_EXE_infigraph-mcp"), &mcp).map(|_| ()))
        .unwrap();
    let pids = bin.path().join("pids");
    let fake = bin.path().join("infigraph");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho $$ >> \"$FAKE_CLI_PIDS\"\nsleep 600 &\necho $! >> \"$FAKE_CLI_PIDS\"\nwait\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let server = start_supervisor_with(
        Some(&mcp),
        &["--mcp"],
        &[("FAKE_CLI_PIDS", pids.to_str().unwrap())],
    );
    WithChild {
        server,
        pids,
        _bin: bin,
    }
}

impl WithChild {
    /// Has the worker call the fake CLI, and returns the worker's pid and the
    /// fake's two pids once they exist.
    fn start_call(&mut self, project: &Path) -> (u32, Vec<u32>) {
        let worker = registered_worker(&self.server.instances, None);
        self.server
            .send(json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "index_docs", "arguments": {"path": project}}}));
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            let pids: Vec<u32> = std::fs::read_to_string(&self.pids)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect();
            if pids.len() >= 2 {
                return (worker, pids);
            }
            assert!(Instant::now() < until, "the fake CLI never started");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// A worker busy in a call goes down within a couple of seconds of its
/// supervisor being terminated. It used to take its 5s poll.
#[test]
fn a_busy_worker_goes_down_promptly_with_a_terminated_supervisor() {
    let mut s = busy_server();
    let worker = registered_worker(&s.instances, None);
    let _cleanup = Cleanup(vec![worker]);
    s.call(10, STALLED);
    std::thread::sleep(Duration::from_millis(500));

    signal(s.child.id(), libc::SIGTERM);

    assert!(
        gone_within(worker, Duration::from_millis(2500)),
        "the busy worker outlived its terminated supervisor"
    );
}

/// What the worker started goes with it: the CLI child and its own child.
#[test]
fn what_a_busy_worker_started_goes_down_with_a_terminated_supervisor() {
    let mut w = with_cli_child();
    let project = tempfile::tempdir().unwrap();
    let (worker, fake) = w.start_call(project.path());
    let mut all = vec![worker];
    all.extend(&fake);
    let _cleanup = Cleanup(all.clone());

    signal(w.server.child.id(), libc::SIGTERM);

    for pid in &all {
        assert!(
            gone_within(*pid, Duration::from_millis(2500)),
            "pid {pid} (worker {worker}, cli {fake:?}) outlived the supervisor"
        );
    }
}

/// SIGKILL runs no code in the supervisor, so the worker's own monitor is the
/// backstop: it takes its group down with it, within the poll interval.
#[test]
fn what_a_busy_worker_started_goes_down_when_its_supervisor_is_killed() {
    let mut w = with_cli_child();
    let project = tempfile::tempdir().unwrap();
    let (worker, fake) = w.start_call(project.path());
    let mut all = vec![worker];
    all.extend(&fake);
    let _cleanup = Cleanup(all.clone());

    signal(w.server.child.id(), libc::SIGKILL);

    for pid in &all {
        assert!(
            gone_within(*pid, Duration::from_secs(9)),
            "pid {pid} (worker {worker}, cli {fake:?}) outlived a SIGKILLed supervisor"
        );
    }
}

/// An idle worker already went with its supervisor (its stdin is the
/// supervisor's pipe); the change must not make it slower.
#[test]
fn an_idle_worker_goes_down_at_once_with_a_terminated_supervisor() {
    let s = busy_server();
    let worker = registered_worker(&s.instances, None);
    let _cleanup = Cleanup(vec![worker]);

    signal(s.child.id(), libc::SIGTERM);

    assert!(gone_within(worker, Duration::from_secs(2)));
}

/// What a worker that exits by itself left behind is swept: here the worker is
/// killed from outside while its CLI child runs, and the supervisor clears
/// the group before it reaps the worker.
#[test]
fn what_a_worker_left_behind_when_it_dies_is_swept_by_the_supervisor() {
    let mut w = with_cli_child();
    let project = tempfile::tempdir().unwrap();
    let (worker, fake) = w.start_call(project.path());
    let _cleanup = Cleanup(fake.iter().copied().chain([worker]).collect());

    signal(worker, libc::SIGKILL);

    for pid in &fake {
        assert!(
            gone_within(*pid, Duration::from_millis(3000)),
            "cli pid {pid} was left behind by a dead worker"
        );
    }
}

/// A terminal Ctrl-C reaches only the foreground group. With the worker in a
/// group of its own, the supervisor has to take it down itself, also in the
/// `--serve` mode whose worker never reads stdin.
#[test]
fn sigint_to_a_serve_mode_supervisor_takes_the_worker_down() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut s = start_supervisor_with(None, &["--serve", &format!("--mcp-port={port}")], &[]);
    let worker = registered_worker(&s.instances, None);
    let _cleanup = Cleanup(vec![worker]);

    signal(s.child.id(), libc::SIGINT);

    assert!(
        gone_within(worker, Duration::from_secs(3)),
        "the --serve worker outlived a SIGINTed supervisor"
    );
    assert!(s.child.wait().is_ok());
}

/// The worker never signals a group it does not lead. Started the old way (a
/// member of someone else's group, as when an old-build supervisor spawns a
/// newer binary), it exits when its supervisor is gone, and a bystander in
/// the same group is untouched. A wrong `killpg` there takes out the MCP
/// client itself.
#[test]
fn a_worker_that_is_not_a_group_leader_exits_and_signals_nobody_else() {
    let tmp = tempfile::tempdir().unwrap();
    let scratch = tmp.path();
    let standin = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .unwrap();
    let standin_pid = standin.id();
    let mut standin = standin;

    // A leader whose group the worker and a bystander are mere members of.
    let mut worker_cmd = support::isolated_mcp_command(scratch);
    worker_cmd.args(["--worker", "--mcp"]);
    let worker_prog = worker_cmd.get_program().to_owned();
    let envs: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> = worker_cmd
        .get_envs()
        .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
        .collect();
    let mut leader = std::process::Command::new("sh");
    leader
        .arg("-c")
        // Stdin is saved on fd 3 first: a shell that backgrounds a command
        // with job control off points that command's fd 0 at /dev/null
        // *before* its own redirections run (dash, Ubuntu's /bin/sh), so a
        // plain `<&0` hands the worker /dev/null, whose EOF ends it at once.
        // bash keeps fd 0 there, which is the only reason this ever passed.
        .arg("exec 3<&0; \"$0\" --worker --mcp <&3 & sleep 600 & echo $! > \"$1\"; wait")
        .arg(&worker_prog)
        .arg(scratch.join("bystander.pid"))
        .current_dir(scratch)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in envs {
        match v {
            Some(v) => leader.env(k, v),
            None => leader.env_remove(k),
        };
    }
    leader
        .env("INFIGRAPH_SUPERVISOR_PID", standin_pid.to_string())
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND);
    std::os::unix::process::CommandExt::process_group(&mut leader, 0);
    let mut leader = leader.spawn().unwrap();
    let leader_pid = leader.id();

    let instances = scratch.join("instances");
    let worker = registered_worker(&instances, None);
    let until = Instant::now() + Duration::from_secs(10);
    while !scratch.join("bystander.pid").exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(100));
    let bystander: u32 = std::fs::read_to_string(scratch.join("bystander.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _cleanup = Cleanup(vec![worker, bystander, leader_pid, standin_pid]);
    // SAFETY: the worker is a member, not the leader, of the leader's group.
    assert_ne!(unsafe { libc::getpgid(worker as i32) }, worker as i32);

    // The stand-in supervisor dies and is reaped, so the worker's poll sees it gone.
    standin.kill().unwrap();
    standin.wait().unwrap();

    assert!(
        gone_within(worker, Duration::from_secs(9)),
        "the worker outlived its supervisor"
    );
    assert!(
        alive(bystander),
        "the worker signalled a group it did not lead"
    );
    assert!(
        alive(leader_pid),
        "the worker signalled a group it did not lead"
    );
    let _ = leader.kill();
    let _ = leader.wait();
}
