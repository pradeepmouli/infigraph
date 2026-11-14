//! The routed backend answers the generations it holds, so the decisions made
//! through it are right. Before this, `current_scip_generation` answered the
//! trait default 0 ("never enriched") on the daemon backend, which did two
//! things wrong:
//!
//! * the import ledger recorded 0 for a `scip-enrich` run, while the graph held
//!   the real stamp;
//! * `infigraph index` read 0 as "never enriched" and started an enrichment on
//!   every run, whatever the graph's real state.
//!
//! The daemon runs with enrichment off, so only the explicit `scip-enrich` makes
//! an import. The fake `scip-python` writes one document and logs each run, so
//! the tests can count how often the indexer ran.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use infigraph_core::scip::SCIP_IMPORT_REVISION;
use support::{assert_ok, run, Daemon};

/// A `scip-python` that logs each run, then exits 0 having written one
/// document, `hello.py`, at `--output`.
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
             printf '\\012\\000\\022\\012\\012\\010hello.py' > \"$out\"\n\
             exit 0\n",
            home.join("indexer.runs").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// How many times the fake indexer has run.
fn indexer_runs(home: &Path) -> usize {
    std::fs::read_to_string(home.join("indexer.runs"))
        .unwrap_or_default()
        .lines()
        .count()
}

/// The CLI with `backend`, the fake indexer first on PATH, and `enrich` deciding
/// whether implicit enrichment is on.
fn cli(root: &Path, home: &Path, backend: &str, enrich: bool) -> Command {
    let path = std::env::join_paths(std::iter::once(home.join("fakebin")).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_infigraph"));
    command
        .current_dir(root)
        .env("PATH", path)
        .env("HOME", home)
        .env(infigraph_core::BACKEND_ENV, backend)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .env_remove("INFIGRAPH_DOCS_ENABLED")
        .env(infigraph_core::scip_switch::background_env_name(), "0");
    if enrich {
        command.env_remove(infigraph_core::scip_switch::enabled_env_name());
    } else {
        command.env(infigraph_core::scip_switch::enabled_env_name(), "0");
    }
    command
}

/// An indexed project with the daemon serving it, enrichment off for the
/// daemon itself, and one routed `scip-enrich python` run done: the graph is
/// enriched, and the import is in the ledger.
fn enriched_through_the_daemon(root: &Path, home: &Path) -> Daemon {
    std::fs::write(root.join("hello.py"), "def hello():\n    pass\n").unwrap();
    fake_scip_python(home);

    assert_ok(
        &run(
            root,
            home,
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "bootstrap index",
    );

    let daemon = Daemon(
        cli(root, home, infigraph_core::DAEMON_BACKEND, false)
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

    let enrich = cli(root, home, infigraph_core::DAEMON_BACKEND, true)
        .args(["scip-enrich", "python"])
        .output()
        .unwrap();
    assert_ok(&enrich, "scip-enrich python under the daemon backend");
    assert_eq!(indexer_runs(home), 1, "the enrichment ran the indexer once");
    daemon
}

#[test]
fn a_routed_scip_enrich_records_the_generation_it_stamped() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home_path) = (project.path(), home.path());
    let _daemon = enriched_through_the_daemon(root, home_path);

    assert_ok(
        &run(root, home_path, "daemon", &["daemon-stop", "--wait"]),
        "daemon-stop --wait",
    );

    let ig = root.join(".infigraph");
    let text = std::fs::read_to_string(ig.join("scip-imports.json"))
        .expect("the routed import wrote a ledger entry");
    let ledger: serde_json::Value = serde_json::from_str(&text).unwrap();
    let entry = &ledger["scip-python"];
    assert_eq!(
        entry["importer_revision"].as_u64(),
        Some(SCIP_IMPORT_REVISION),
        "{ledger}"
    );
    let recorded = entry["scip_generation"]
        .as_i64()
        .unwrap_or_else(|| panic!("no scip_generation in {ledger}"));
    assert!(
        recorded > 0,
        "the record holds the never-enriched 0, not the generation the import stamped: {ledger}"
    );

    let graph =
        infigraph_core::graph::GraphStore::open(&ig.join(infigraph_core::graph::names::STEM))
            .unwrap()
            .current_scip_generation()
            .unwrap();
    assert_eq!(
        recorded, graph,
        "the record disagrees with the graph's own stamp: {ledger}"
    );
}

/// An incremental `index` of an enriched graph, served through the daemon,
/// starts no enrichment. Before the routed override, the generation it read was
/// the default 0, so `index` treated every enriched graph as never enriched and
/// ran the indexer on each run.
#[test]
fn a_routed_index_of_an_enriched_graph_starts_no_enrichment() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home_path) = (project.path(), home.path());
    let _daemon = enriched_through_the_daemon(root, home_path);

    // A file that changed since the last index: `index` only returns the
    // extractions `auto_scip` judges by when there is something to index. With
    // nothing changed the enrichment decision is never reached, and the test
    // would pass whatever generation the routed backend reads.
    std::fs::write(root.join("world.py"), "def world():\n    pass\n").unwrap();

    let index = cli(root, home_path, infigraph_core::DAEMON_BACKEND, true)
        .args(["index", "--no-embed"])
        .output()
        .unwrap();
    assert_ok(&index, "index --no-embed under the daemon backend");
    assert_eq!(
        indexer_runs(home_path),
        1,
        "an index of an enriched graph ran the indexer again: the routed generation read as never enriched"
    );

    assert_ok(
        &run(root, home_path, "daemon", &["daemon-stop", "--wait"]),
        "daemon-stop --wait",
    );
}
