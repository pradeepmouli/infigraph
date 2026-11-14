//! A `scip-enrich` run under the daemon backend records the generation its
//! import stamped, not 0. The import is routed to the daemon, and the generation
//! it is recorded with is read back through the same routed backend. The routed
//! backend used to answer the trait default (0, "never enriched"), so the ledger
//! carried 0 for every such run while the graph held the real stamp.
//!
//! The daemon runs with enrichment off, so only the explicit `scip-enrich` makes
//! an import. The fake `scip-python` writes one document, so the import stamps.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use infigraph_core::scip::SCIP_IMPORT_REVISION;
use support::{assert_ok, run, Daemon};

/// A `scip-python` that exits 0 having written one document, `hello.py`, at
/// `--output`.
fn fake_scip_python(home: &Path) {
    let bin = home.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("scip-python");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         out=\"\"\n\
         while [ $# -gt 0 ]; do\n\
         \x20 if [ \"$1\" = \"--output\" ]; then out=\"$2\"; fi\n\
         \x20 shift\n\
         done\n\
         printf '\\012\\000\\022\\012\\012\\010hello.py' > \"$out\"\n\
         exit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
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

#[test]
fn a_routed_scip_enrich_records_the_generation_it_stamped() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home_path) = (project.path(), home.path());
    std::fs::write(root.join("hello.py"), "def hello():\n    pass\n").unwrap();
    fake_scip_python(home_path);

    // A graph, indexed locally, with no enrichment.
    assert_ok(
        &run(
            root,
            home_path,
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "bootstrap index",
    );

    // The daemon serves the routed backend. Enrichment is off for it, so the
    // only import is the explicit scip-enrich below.
    let _daemon = Daemon(
        cli(root, home_path, infigraph_core::DAEMON_BACKEND, false)
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

    // The routed run: the import goes to the daemon, and the generation the
    // record carries is read back through the same routed backend.
    let enrich = cli(root, home_path, infigraph_core::DAEMON_BACKEND, true)
        .args(["scip-enrich", "python"])
        .output()
        .unwrap();
    assert_ok(&enrich, "scip-enrich python under the daemon backend");

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
