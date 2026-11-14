//! An indexer that exits 0 with an index holding no documents is settled in the
//! SCIP import ledger on every local enrichment path, at the current
//! `SCIP_IMPORT_REVISION`. That is what clears `doctor`'s "ledger behind"
//! warning for the remedy `infigraph index --full`: a language settled this way
//! is no longer replayed for every AST generation.
//!
//! The fake `scip-python` writes the metadata-only index a real one writes for a
//! language it finds nothing of (`printf '\012\000'`). Each run is logged, so a
//! failure says how many enrichments ran.
//!
//! Written without a compile or run (the machine had no room for cargo when it
//! was written); the first run is the one that proves it.

#![cfg(unix)]

mod support;

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use infigraph_core::scip::{ledger_entries, SCIP_IMPORT_REVISION};
use infigraph_core::scip_switch::replay_languages;
use support::{assert_ok, eventually, run};

/// A `scip-python` that exits `exit_code` after logging its run, writing the
/// metadata-only index (no documents) at `--output` when it exits 0.
fn fake_scip_python(home: &Path, exit_code: u8) {
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
             if [ {exit_code} -ne 0 ]; then exit {exit_code}; fi\n\
             printf '\\012\\000' > \"$out\"\n\
             exit 0\n",
            home.join("indexer.runs").display()
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

/// The local-backend CLI with SCIP enrichment ON and the fake first on PATH.
/// `support::run` turns enrichment off, so this builds its own command.
fn enriching_local(root: &Path, home: &Path) -> Command {
    let path = std::env::join_paths(std::iter::once(home.join("fakebin")).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_infigraph"));
    command
        .current_dir(root)
        .env("PATH", path)
        .env("HOME", home)
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .env_remove(infigraph_core::scip_switch::enabled_env_name())
        // These tests wait on the fake indexer against their own clocks.
        .env(infigraph_core::scip_switch::background_env_name(), "0")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env_remove("INFIGRAPH_DOCS_ENABLED");
    command
}

fn project(root: &Path) {
    std::fs::write(root.join("hello.py"), "def hello():\n    pass\n").unwrap();
}

fn detected_python() -> HashSet<String> {
    ["python".to_string()].into_iter().collect()
}

/// The ledger's `scip-python` entry, if it has one.
fn python_entry(ig: &Path) -> Option<infigraph_core::scip::LedgerEntry> {
    ledger_entries(ig)?
        .into_iter()
        .find(|e| e.label == "scip-python")
}

/// The assertions that make the remedy work: the label is settled at the
/// current revision, and nothing is replayed for python.
fn assert_settled(root: &Path) {
    let ig = root.join(".infigraph");
    let entry = python_entry(&ig).expect("the empty index is recorded in the ledger");
    assert_eq!(
        entry.revision,
        Some(SCIP_IMPORT_REVISION),
        "settled at the current revision"
    );
    let entries = ledger_entries(&ig).unwrap();
    let replayed = replay_languages(root, 5, &entries, &detected_python());
    assert!(
        !replayed.iter().any(|l| *l == "python"),
        "python is still replayed after the settle: {replayed:?}"
    );
}

/// The remedy on a local graph: `index --full` runs the enrichment, and the
/// empty output is settled rather than left behind.
#[test]
fn local_index_full_settles_an_empty_python_index() {
    let project_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home_path) = (project_dir.path(), home.path());
    project(root);
    fake_scip_python(home_path, 0);

    // A graph with no enrichment, so the full rebuild is the first enrichment.
    assert_ok(
        &run(
            root,
            home_path,
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "bootstrap index",
    );

    let out = enriching_local(root, home_path)
        .args(["index", "--full", "--no-embed"])
        .output()
        .unwrap();
    assert_ok(&out, "index --full with the fake indexer");

    // The enrichment may run detached; wait for the ledger to be written.
    let ig = root.join(".infigraph");
    let settled = eventually(Duration::from_secs(120), || python_entry(&ig).is_some());
    assert!(
        settled,
        "no ledger entry after {} indexer run(s)",
        runs(home_path)
    );
    assert_settled(root);
}

/// The first index of a never-enriched graph also enriches, and the same
/// settle applies: the local path, not the rebuild, is what is under test.
#[test]
fn first_local_index_settles_an_empty_python_index() {
    let project_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home_path) = (project_dir.path(), home.path());
    project(root);
    fake_scip_python(home_path, 0);

    let out = enriching_local(root, home_path)
        .args(["index", "--no-embed"])
        .output()
        .unwrap();
    assert_ok(&out, "first index with the fake indexer");

    let ig = root.join(".infigraph");
    let settled = eventually(Duration::from_secs(120), || python_entry(&ig).is_some());
    assert!(
        settled,
        "no ledger entry after {} indexer run(s)",
        runs(home_path)
    );
    assert_settled(root);
}

/// The detached enrich path (`scip-enrich`) settles an empty index the same way.
/// It runs in the foreground when invoked directly, so the ledger is read at once.
#[test]
fn scip_enrich_settles_an_empty_python_index() {
    let project_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home_path) = (project_dir.path(), home.path());
    project(root);
    fake_scip_python(home_path, 0);

    assert_ok(
        &run(
            root,
            home_path,
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "bootstrap index",
    );

    let out = enriching_local(root, home_path)
        .args(["scip-enrich", "python"])
        .output()
        .unwrap();
    assert_ok(&out, "scip-enrich python with the fake indexer");
    assert_eq!(runs(home_path), 1, "the fake indexer ran once");
    assert_settled(root);
}

/// The negative: an indexer that exits non-zero records nothing. Only a clean
/// exit with no documents is settled.
#[test]
fn a_failed_indexer_leaves_the_ledger_alone() {
    let project_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home_path) = (project_dir.path(), home.path());
    project(root);
    fake_scip_python(home_path, 3);

    assert_ok(
        &run(
            root,
            home_path,
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "bootstrap index",
    );

    let _ = enriching_local(root, home_path)
        .args(["scip-enrich", "python"])
        .output()
        .unwrap();
    assert_eq!(runs(home_path), 1, "the fake indexer ran once");
    let ig = root.join(".infigraph");
    assert!(
        python_entry(&ig).is_none(),
        "a failed run must not be settled in the ledger"
    );
}
