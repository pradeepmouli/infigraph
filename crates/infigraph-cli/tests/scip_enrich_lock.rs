//! #209 item 15: one SCIP enrichment of a project at a time.
//!
//! Real daemon, real `scip-enrich` processes, a fake `scip-python` that logs
//! every run. The first run of the fake is slow and the later ones fast, and it
//! writes its output only at the very end -- the shape that exposes the shared
//! `scip-tmp/` directory: a duplicate that finishes first cleans the directory
//! out from under the slow one's output.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use infigraph_core::last_run::{self, Kind};
use support::{assert_ok, eventually, run, Daemon};

/// How long the first fake run takes. Long enough for the second enrichment to
/// start (and, without the lock, to finish) inside it.
const SLOW_SECS: u32 = 14;

/// A `scip-python` that logs `run`, sleeps (the first run long, the others
/// short) and then writes a one-document index at `--output`.
fn fake_scip_python(home: &Path) {
    let bin = home.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("scip-python");
    let runs = home.join("indexer.runs");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             echo run >> '{runs}'\n\
             n=$(wc -l < '{runs}')\n\
             if [ \"$n\" -le 1 ]; then sleep {SLOW_SECS}; else sleep 1; fi\n\
             out=\"\"\n\
             while [ $# -gt 0 ]; do\n\
             \x20 if [ \"$1\" = \"--output\" ]; then out=\"$2\"; fi\n\
             \x20 shift\n\
             done\n\
             printf '\\012\\000\\022\\012\\012\\010hello.py' > \"$out\"\n\
             exit 0\n",
            runs = runs.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn runs(home: &Path) -> usize {
    std::fs::read_to_string(home.join("indexer.runs"))
        .unwrap_or_default()
        .lines()
        .count()
}

/// The CLI with the fake indexer first on PATH, routed through the daemon.
fn enriching(root: &Path, home: &Path) -> Command {
    let path = std::env::join_paths(std::iter::once(home.join("fakebin")).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_infigraph"));
    command
        .current_dir(root)
        .env("PATH", path)
        .env("HOME", home)
        .env(infigraph_core::BACKEND_ENV, infigraph_core::DAEMON_BACKEND)
        .env_remove(infigraph_core::scip_switch::enabled_env_name())
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env_remove("INFIGRAPH_DOCS_ENABLED");
    command
}

/// A project with a graph but no enrichment, and a daemon serving it.
fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Daemon) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, h) = (project.path(), home.path());
    std::fs::write(root.join("hello.py"), "def hello():\n    pass\n").unwrap();
    fake_scip_python(h);
    assert_ok(
        &run(
            root,
            h,
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "bootstrap index",
    );
    let daemon = Daemon(
        enriching(root, h)
            .args(["daemon", "--debounce", "50"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(
            &root.join(".infigraph").join("watch.lock"),
            Duration::from_secs(30)
        ),
        "the daemon never took watch.lock"
    );
    (project, home, daemon)
}

/// One detached-style enrichment, its stderr captured.
fn spawn_enrichment(root: &Path, home: &Path, tag: &str) -> (Child, std::path::PathBuf) {
    let log = home.join(format!("enrich-{tag}.log"));
    let child = enriching(root, home)
        .args(["scip-enrich", "python"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&log).unwrap()))
        .spawn()
        .unwrap();
    (child, log)
}

fn wait_for_run_to_start(home: &Path, n: usize) {
    assert!(
        eventually(Duration::from_secs(60), || runs(home) >= n),
        "indexer run {n} never started"
    );
}

fn stop_daemon(root: &Path, home: &Path) {
    assert_ok(
        &run(root, home, "daemon", &["daemon-stop", "--wait"]),
        "daemon-stop --wait",
    );
}

#[test]
fn two_overlapping_enrichments_of_one_project_run_the_indexer_once() {
    let (project, home, _daemon) = fixture();
    let (root, h) = (project.path(), home.path());

    let (mut first, _) = spawn_enrichment(root, h, "a");
    wait_for_run_to_start(h, 1);
    let (mut second, second_log) = spawn_enrichment(root, h, "b");
    first.wait().unwrap();
    second.wait().unwrap();
    stop_daemon(root, h);

    assert_eq!(
        runs(h),
        1,
        "the second enrichment ran the indexers again while the first was running"
    );
    let said = std::fs::read_to_string(second_log).unwrap();
    assert!(
        said.contains(infigraph_core::scip::ENRICHMENT_SKIPPED),
        "the loser should say it skipped:\n{said}"
    );
}

#[test]
fn a_duplicate_enrichment_does_not_destroy_the_running_ones_output() {
    let (project, home, _daemon) = fixture();
    let (root, h) = (project.path(), home.path());

    let (mut first, _) = spawn_enrichment(root, h, "a");
    wait_for_run_to_start(h, 1);
    // Without the lock this one runs and finishes (1 s) well inside the first's
    // 6 s, and its cleanup removes `scip-tmp/` under the first's output.
    let (mut second, _) = spawn_enrichment(root, h, "b");
    first.wait().unwrap();
    second.wait().unwrap();
    stop_daemon(root, h);

    let ig = root.join(".infigraph");
    let record = last_run::read(&ig, Kind::Scip).expect("a scip run was recorded");
    let last = record.last.expect("a last run");
    assert!(
        last.ok && last.losses.is_empty(),
        "the first enrichment's output was lost: {last:?}"
    );
    assert!(
        last.summary.contains("imported 1"),
        "nothing was imported: {last:?}"
    );
    assert!(
        record.last_problem.is_none(),
        "a skipped duplicate is not a problem: {:?}",
        record.last_problem
    );
}

#[test]
fn a_holder_killed_mid_run_frees_the_project_for_the_next_enrichment() {
    let (project, home, _daemon) = fixture();
    let (root, h) = (project.path(), home.path());

    let (mut first, _) = spawn_enrichment(root, h, "a");
    wait_for_run_to_start(h, 1);
    first.kill().unwrap(); // SIGKILL: no destructor runs
    first.wait().unwrap();

    let (mut second, second_log) = spawn_enrichment(root, h, "b");
    second.wait().unwrap();
    stop_daemon(root, h);

    let said = std::fs::read_to_string(second_log).unwrap();
    assert!(
        !said.contains(infigraph_core::scip::ENRICHMENT_SKIPPED),
        "a dead holder's lock must not block the next enrichment:\n{said}"
    );
    assert!(runs(h) >= 2, "the next enrichment never ran its indexers");
}

/// The foreground `index --no-embed` pass is an enrichment entry point too: it
/// is turned away while another enrichment holds the project, and runs once the
/// project is free. (Local backend: no daemon is needed to see the skip.)
#[test]
fn the_foreground_pass_skips_while_an_enrichment_holds_the_project() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, h) = (project.path(), home.path());
    std::fs::write(root.join("hello.py"), "def hello():\n    pass\n").unwrap();
    fake_scip_python(h);
    assert_ok(
        &run(
            root,
            h,
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "bootstrap index",
    );

    let foreground = |edit: &str| {
        std::fs::write(
            root.join("hello.py"),
            format!("def hello():\n    pass\n{edit}\n"),
        )
        .unwrap();
        enriching(root, h)
            .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
            .env("INFIGRAPH_NO_WATCH", "1")
            .args(["index", "--no-embed"])
            .output()
            .unwrap()
    };

    let held = infigraph_core::scip::try_begin_enrichment(root, "test").expect("the lock was free");
    let out = foreground("# one");
    assert_ok(
        &out,
        "index --no-embed while an enrichment holds the project",
    );
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        said.contains(infigraph_core::scip::ENRICHMENT_SKIPPED),
        "the foreground pass should say it skipped:\n{said}"
    );
    assert_eq!(runs(h), 0, "the foreground pass ran an indexer anyway");

    drop(held);
    assert_ok(
        &foreground("# two"),
        "index --no-embed once the project is free",
    );
    assert_eq!(
        runs(h),
        1,
        "the foreground pass should run once the lock is free"
    );
}
