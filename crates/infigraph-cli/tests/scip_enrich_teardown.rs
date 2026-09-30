//! A worktree's detached `scip-enrich` must not outlive the removal of its
//! worktree: `infigraph index` leaves it running after it returns, it keeps
//! writing into `.infigraph/`, and a `git worktree remove` racing it fails
//! half-done ("Directory not empty"). The hook stops the worktree's daemon
//! before the remove (`daemon-stop --wait`) and tears down after it
//! (`worktree teardown`); both now stop the enrichment too.
//!
//! The chain is real: `index` spawns the real child, which runs a fake
//! `scip-python` found first on PATH. The fake records its
//! pid and sleeps, standing in for an indexer that runs for minutes.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn cli() -> &'static str {
    env!("CARGO_BIN_EXE_infigraph")
}

/// Kills what a failed assertion would otherwise leave sleeping for ten
/// minutes: the fake indexer and the `scip-enrich` that parents it.
struct Reap(Vec<u32>);

impl Drop for Reap {
    fn drop(&mut self) {
        for pid in &self.0 {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
    }
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn parent_of(pid: u32) -> Option<u32> {
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn wait_until(budget: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// An indexed Python project whose enrichment is running: the fake indexer
/// is alive, and so is the `scip-enrich` child that started it.
struct Running {
    home: tempfile::TempDir,
    project: tempfile::TempDir,
    indexer: u32,
    child: u32,
    _reap: Reap,
}

fn cmd(home: &Path, root: &Path) -> Command {
    // `ensure_indexer` looks on PATH before anything else, and a developer's
    // machine may have the real `scip-python` there: the fake goes first.
    let path = std::env::join_paths(std::iter::once(home.join("fakebin")).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let mut c = Command::new(cli());
    c.current_dir(root)
        .env("PATH", path)
        .env("HOME", home)
        .env(infigraph_core::BACKEND_ENV, "kuzu")
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env_remove("INFIGRAPH_DOCS_ENABLED");
    c
}

fn start_enrichment() -> Running {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def foo():\n    pass\n").unwrap();

    // The script records its pid, then becomes `sleep`.
    let bin = home.path().join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let pid_file: PathBuf = home.path().join("indexer.pid");
    let script = bin.join("scip-python");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 600\n",
            pid_file.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = cmd(home.path(), project.path())
        .arg("index")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(90), || pid_file.exists()
            && !std::fs::read_to_string(&pid_file)
                .unwrap_or_default()
                .trim()
                .is_empty()),
        "the detached scip-enrich never started the fake indexer"
    );
    let indexer: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let child = parent_of(indexer).expect("the fake indexer has no parent");
    let reap = Reap(vec![indexer, child]);
    assert!(alive(indexer) && alive(child));
    Running {
        home,
        project,
        indexer,
        child,
        _reap: reap,
    }
}

fn gone(pid: u32) -> bool {
    wait_until(Duration::from_secs(15), || !alive(pid))
}

/// The hook's PreToolUse step, run before git touches the directory.
#[test]
fn daemon_stop_wait_stops_a_running_enrichment() {
    let run = start_enrichment();
    let out = cmd(run.home.path(), run.project.path())
        .args(["daemon-stop", "--wait"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "daemon-stop --wait failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        gone(run.child),
        "the scip-enrich child outlived daemon-stop --wait"
    );
    assert!(
        gone(run.indexer),
        "the indexer it started outlived daemon-stop --wait"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("scip-enrich"),
        "daemon-stop should say it stopped an enrichment: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// The hook's PostToolUse step: the directory is already gone, so nothing on
/// disk can point at the child; only the live process can.
#[test]
fn teardown_stops_a_running_enrichment_after_the_directory_is_gone() {
    let run = start_enrichment();
    let root = run.project.path().canonicalize().unwrap();
    std::fs::remove_dir_all(&root).unwrap();

    let out = cmd(run.home.path(), run.home.path())
        .args(["worktree", "teardown"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "teardown failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(gone(run.child), "the scip-enrich child outlived teardown");
    assert!(gone(run.indexer), "the indexer outlived teardown");
}

/// Another worktree's enrichment is not this one's to stop.
#[test]
fn stopping_one_root_leaves_another_roots_enrichment_alone() {
    let run = start_enrichment();
    let elsewhere = tempfile::tempdir().unwrap();
    let out = cmd(run.home.path(), elsewhere.path())
        .args(["daemon-stop", "--wait"])
        .output()
        .unwrap();
    assert!(out.status.success());
    std::thread::sleep(Duration::from_millis(500));
    assert!(alive(run.child) && alive(run.indexer));
}

/// What `index` printed and left behind, for a project with one Python file.
fn index_python_project(configure: impl FnOnce(&Path, &mut Command)) -> (String, bool) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("a.py"), "def foo():\n    pass\n").unwrap();
    let mut index = cmd(home.path(), project.path());
    configure(project.path(), &mut index);
    let out = index.arg("index").output().unwrap();
    assert!(
        out.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        project.path().join(".infigraph/scip-enrich.log").exists(),
    )
}

/// `INFIGRAPH_SCIP_ENABLED=0` (the env name comes from the settings
/// definition): no detached child, so nothing outlives a throwaway project.
#[test]
fn index_with_scip_off_by_env_spawns_no_enrichment() {
    let (stdout, log) = index_python_project(|_, c| {
        c.env(infigraph_core::scip_switch::enabled_env_name(), "0");
    });
    assert!(!stdout.contains("SCIP enrichment starting"), "{stdout}");
    assert!(!log, "the scip-enrich child was spawned anyway");
}

/// The same switch from the project's own config. The first `index` of a
/// project whose `.infigraph/` already exists may rebuild from scratch; the
/// rebuild keeps `config.toml`.
#[test]
fn index_with_scip_off_in_the_project_config_spawns_no_enrichment() {
    let (stdout, log) = index_python_project(|root, _| {
        infigraph_core::settings_file::set_project_setting(
            root,
            "scip",
            "enabled",
            toml_edit::value(false),
        )
        .unwrap();
    });
    assert!(!stdout.contains("SCIP enrichment starting"), "{stdout}");
    assert!(!log, "the scip-enrich child was spawned anyway");
}
