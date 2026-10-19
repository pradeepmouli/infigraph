//! When `infigraph index` starts SCIP enrichment: on a full reindex, and on
//! any index of a graph that has never been enriched (the daemon's
//! staleness check skips such a graph, so nothing else would ever do it) --
//! never on an incremental index of an enriched one, which the daemon's
//! staleness trigger keeps current. `[scip] enabled = false` starts none.
//!
//! The foreground `--no-embed` pass runs the fake `scip-python` first on
//! PATH synchronously and logs each run, so the count of runs is the answer.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use support::{assert_ok, run};

fn fake_scip_python(home: &Path) {
    let bin = home.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("scip-python");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho run >> '{}'\nexit 1\n",
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

/// `infigraph index --no-embed` with enrichment on (unless `enabled` is
/// false) and the fake indexer first on PATH, opening the graph itself.
fn index(root: &Path, home: &Path, full: bool, enabled: bool) {
    let path = std::env::join_paths(std::iter::once(home.join("fakebin")).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_infigraph"));
    command
        .current_dir(root)
        .arg("index")
        .arg("--no-embed")
        .env("PATH", path)
        .env("HOME", home)
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env_remove("INFIGRAPH_DOCS_ENABLED");
    if full {
        command.arg("--full");
    }
    command.env(
        infigraph_core::scip_switch::enabled_env_name(),
        if enabled { "1" } else { "0" },
    );
    assert_ok(&command.output().unwrap(), "index");
}

/// A project indexed once with enrichment off, so its graph exists and was
/// never enriched.
fn never_enriched_project() -> (tempfile::TempDir, tempfile::TempDir) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n").unwrap();
    fake_scip_python(home.path());
    assert_ok(
        &run(
            project.path(),
            home.path(),
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "bootstrap index",
    );
    assert_eq!(runs(home.path()), 0, "the bootstrap ran an indexer");
    (project, home)
}

/// Stamp the graph as enriched once, as a successful import does.
fn mark_enriched(root: &Path) {
    let store = infigraph_core::graph::GraphStore::open(&root.join(".infigraph/graph")).unwrap();
    let lock = store.write_lock().unwrap();
    let conn = store.connection().unwrap();
    store.bump_ast_generation_conn(&conn, &lock).unwrap();
    store
        .stamp_scip_generation_conn(&conn, &lock, None)
        .unwrap();
}

fn edit(root: &Path) {
    std::fs::write(
        root.join("hello.py"),
        "def hello():\n    pass\n\ndef changed():\n    pass\n",
    )
    .unwrap();
}

#[test]
fn an_incremental_index_enriches_a_graph_that_never_was() {
    let (project, home) = never_enriched_project();
    edit(project.path());
    index(project.path(), home.path(), false, true);
    assert_eq!(runs(home.path()), 1);
}

#[test]
fn an_incremental_index_leaves_an_enriched_graph_to_the_daemon() {
    let (project, home) = never_enriched_project();
    mark_enriched(project.path());
    edit(project.path());
    index(project.path(), home.path(), false, true);
    assert_eq!(runs(home.path()), 0, "an incremental index ran an indexer");
}

#[test]
fn a_full_index_enriches_even_an_enriched_graph() {
    let (project, home) = never_enriched_project();
    mark_enriched(project.path());
    index(project.path(), home.path(), true, true);
    assert_eq!(runs(home.path()), 1);
}

#[test]
fn enrichment_switched_off_runs_no_indexer_on_any_index() {
    let (project, home) = never_enriched_project();
    edit(project.path());
    index(project.path(), home.path(), false, false);
    index(project.path(), home.path(), true, false);
    assert_eq!(runs(home.path()), 0);
}
