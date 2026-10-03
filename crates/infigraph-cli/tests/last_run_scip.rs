//! #209 item 9: an automatic SCIP enrichment that an indexer's empty output
//! defeats leaves a durable record of why, instead of a line in `daemon.log`
//! or `scip-enrich.log`.
//!
//! The chain is real: a daemon in the daemon backend, `index --full` served by
//! it, and the daemon's own enrichment after the rebuild, which runs the fake
//! `scip-python` first on PATH. That one exits 0 having written a
//! metadata-only index, what the real one does for a project with none of its
//! language. Every run of the fake is logged, so a failure says how many
//! enrichments actually ran.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use infigraph_core::last_run::{self, Kind};
use support::{assert_ok, eventually, run, Daemon};

/// A `scip-python` that exits 0 with an index holding a metadata message and
/// no documents, at the path given by `--output`, after logging its run.
fn fake_scip_python(home: &Path) {
    let bin = home.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("scip-python");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             echo run >> '{}'\n\
             out=\"\"\n\
             while [ $# -gt 0 ]; do\n\
             \x20 if [ \"$1\" = \"--output\" ]; then out=\"$2\"; fi\n\
             \x20 shift\n\
             done\n\
             printf '\\012\\000' > \"$out\"\n\
             exit 0\n",
            home.join("indexer.runs").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The CLI with implicit SCIP enrichment ON and the fake indexer first on PATH
/// (`support::infigraph()` turns enrichment off).
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

#[test]
fn a_refused_automatic_enrichment_is_recorded_with_its_reason() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home) = (project.path(), home.path());
    std::fs::write(root.join("hello.py"), "def hello():\n    pass\n").unwrap();
    fake_scip_python(home);

    // A graph with no enrichment: the local index runs with SCIP off.
    assert_ok(
        &run(
            root,
            home,
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "bootstrap index",
    );

    let _daemon = Daemon(
        enriching(root, home)
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

    // The daemon rebuilds, then enriches in the background.
    let full = enriching(root, home)
        .args(["index", "--full", "--no-embed"])
        .output()
        .unwrap();
    assert_ok(&full, "index --full through the daemon");

    let ig = root.join(".infigraph");
    let recorded = eventually(Duration::from_secs(120), || {
        last_run::read(&ig, Kind::Scip).is_some_and(|r| r.last_problem.is_some())
    });
    // Stop the daemon and anything it started, so the graph is closed before
    // it is read and nothing writes into the tempdir after.
    assert_ok(
        &run(root, home, "daemon", &["daemon-stop", "--wait"]),
        "daemon-stop --wait",
    );
    let runs = std::fs::read_to_string(home.join("indexer.runs"))
        .unwrap_or_default()
        .lines()
        .count();
    assert!(
        recorded,
        "no scip problem was recorded after {runs} indexer run(s); daemon.log:\n{}",
        std::fs::read_to_string(ig.join("daemon.log")).unwrap_or_default()
    );

    let record = last_run::read(&ig, Kind::Scip).unwrap();
    let problem = record.last_problem.unwrap();
    assert!(!problem.ok, "{problem:?} after {runs} indexer run(s)");
    let loss = problem
        .losses
        .iter()
        .find(|l| l.what.contains("scip-python"))
        .unwrap_or_else(|| panic!("no loss names the indexer: {problem:?}"));
    assert!(loss.first_reason.contains("no documents"), "{loss:?}");
    assert!(
        !problem.overlapped,
        "one run per process was expected: {problem:?}"
    );

    // The record explains why nothing was enriched; it must not have stamped.
    let generation =
        infigraph_core::graph::GraphStore::open(&root.join(".infigraph").join("graph"))
            .unwrap()
            .current_scip_generation()
            .unwrap();
    assert_eq!(generation, 0, "a refused output stamped the graph");
}
