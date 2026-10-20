//! A daemon that dies by SIGSEGV inside lbug's `Database::new` (WAL replay)
//! left no record: the SIGABRT breadcrumb never fired and the rebuild breaker
//! counts rebuilds. The open window records the death, and the next start
//! sets a torn WAL aside instead of repeating it.
#![cfg(unix)]

use infigraph_core::graph::{GraphCorruption, GraphStore};
use infigraph_core::quarantine::{try_recover_by_setting_wal_aside_with, WalRecovery};
use infigraph_core::recovery;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const HELPER_GRAPH_ENV: &str = "OPEN_CRASH_LOOP_HELPER_GRAPH";

/// Not a test on its own: the child the tests re-exec. It opens the graph
/// with the die-in-window hook set by its parent, so it never returns.
#[test]
fn open_helper() {
    let Some(graph) = std::env::var_os(HELPER_GRAPH_ENV) else {
        return;
    };
    // Recovery is stubbed to "healthy" so a second helper reaches the open
    // window too, as a daemon restarted by launchd would.
    let _ = GraphStore::open_with_recovery(Path::new(&graph), Duration::from_secs(30), &|_, _| {
        WalRecovery::AlreadyHealthy
    });
    unreachable!("the open window kills this process");
}

fn die_inside_the_open(graph: &Path) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "open_helper", "--nocapture", "--test-threads=1"])
        .env(HELPER_GRAPH_ENV, graph)
        .env(recovery::TEST_DIE_IN_OPEN_WINDOW_ENV, graph)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert_eq!(
        status.signal(),
        Some(libc::SIGSEGV),
        "the helper must die by SIGSEGV inside the open, got {status:?}"
    );
}

/// A graph with one file node, and a frozen WAL beside it that is not one.
fn graph_with_a_frozen_wal() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join(infigraph_core::graph::names::STEM);
    {
        let store = GraphStore::open(&graph).unwrap();
        store
            .connection()
            .unwrap()
            .query(
                "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
                 language: 'rust', symbol_count: 0})",
            )
            .unwrap();
    }
    std::fs::write(
        dir.path()
            .join(infigraph_core::graph::names::sibling(".wal.checkpoint")),
        b"frozen",
    )
    .unwrap();
    (dir, graph)
}

fn in_process_probe(dir: &Path, name: &str) -> WalRecovery {
    try_recover_by_setting_wal_aside_with(dir, name, |graph| {
        GraphStore::open_read_only(graph).is_ok()
    })
}

fn open_with(
    graph: &Path,
    recover: &dyn Fn(&Path, &str) -> WalRecovery,
) -> anyhow::Result<GraphStore> {
    GraphStore::open_with_recovery(graph, Duration::from_secs(30), recover)
}

fn names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn after_an_opener_dies_by_sigsegv_the_next_start_sets_the_wal_aside_and_opens() {
    let (dir, graph) = graph_with_a_frozen_wal();
    die_inside_the_open(&graph);
    assert!(
        names(dir.path())
            .iter()
            .any(|n| n.starts_with(infigraph_core::graph::names::OPEN_SENTINEL_PREFIX)),
        "the dead opener's sentinel is still there: {:?}",
        names(dir.path())
    );

    let store = open_with(&graph, &in_process_probe).expect("the second start must open");

    let names = names(dir.path());
    assert!(
        names.iter().any(|n| n
            .starts_with(infigraph_core::graph::names::sibling(".torn-wal.").as_str())
            && n.ends_with(".wal.checkpoint")),
        "the frozen WAL is filed as evidence: {names:?}"
    );
    assert!(
        !names.contains(&infigraph_core::graph::names::sibling(".wal.checkpoint")),
        "{names:?}"
    );
    assert!(
        !names.iter().any(
            |n| n.starts_with(infigraph_core::graph::names::OPEN_SENTINEL_PREFIX)
                || n == "open-deaths.log"
        ),
        "a completed open leaves no trace of the loop: {names:?}"
    );
    let rows = store
        .connection()
        .unwrap()
        .query("MATCH (f:File) RETURN f.id")
        .unwrap()
        .count();
    assert_eq!(rows, 1, "the graph is intact");
}

#[test]
fn a_graph_no_probe_can_open_is_corruption_for_the_existing_rebuild_not_a_loop() {
    let (dir, graph) = graph_with_a_frozen_wal();
    die_inside_the_open(&graph);

    let err = open_with(&graph, &|_, _| WalRecovery::NotRecoverable)
        .err()
        .expect("must refuse");
    assert!(
        err.downcast_ref::<GraphCorruption>().is_some(),
        "routes to CodeIndex::init's quarantine and rebuild: {err:#}"
    );
    assert!(
        !names(dir.path())
            .iter()
            .any(|n| n.starts_with(infigraph_core::graph::names::sibling(".torn-wal.").as_str())),
        "nothing was set aside when the probe could not help"
    );
}

#[test]
fn a_graph_a_probe_opens_but_that_keeps_killing_its_opener_trips_the_crash_loop_marker() {
    let (dir, graph) = graph_with_a_frozen_wal();
    die_inside_the_open(&graph);
    die_inside_the_open(&graph);

    let err = open_with(&graph, &|_, _| WalRecovery::AlreadyHealthy)
        .err()
        .expect("must refuse");
    assert!(
        format!("{err:#}").contains("crash-loop detected"),
        "{err:#}"
    );
    assert!(recovery::crash_loop_detected(dir.path()).is_some());
    assert!(
        !names(dir.path()).contains(&"open-deaths.log".to_string()),
        "deleting the marker alone resets the breaker"
    );
}

#[test]
fn a_clean_open_leaves_no_sentinel_and_one_death_that_a_probe_clears_is_not_a_loop() {
    let (dir, graph) = graph_with_a_frozen_wal();
    std::fs::remove_file(
        dir.path()
            .join(infigraph_core::graph::names::sibling(".wal.checkpoint")),
    )
    .unwrap();
    drop(open_with(&graph, &in_process_probe).unwrap());
    assert!(
        !names(dir.path())
            .iter()
            .any(|n| n.starts_with(infigraph_core::graph::names::OPEN_SENTINEL_PREFIX)),
        "{:?}",
        names(dir.path())
    );

    die_inside_the_open(&graph);
    drop(
        open_with(&graph, &|_, _| WalRecovery::AlreadyHealthy)
            .expect("one death alone is not a loop"),
    );
    assert!(recovery::crash_loop_detected(dir.path()).is_none());
}

#[test]
fn an_opener_that_is_still_alive_is_not_a_death() {
    let (dir, _graph) = graph_with_a_frozen_wal();
    let mut sleeper = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    std::fs::write(
        dir.path().join(format!(
            "{}{}.0",
            infigraph_core::graph::names::OPEN_SENTINEL_PREFIX,
            sleeper.id()
        )),
        format!("{{\"pid\":{},\"started_at\":0,\"at\":0}}", sleeper.id()),
    )
    .unwrap();
    assert!(recovery::open_deaths(dir.path()).is_empty());
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
    assert_eq!(recovery::open_deaths(dir.path()).len(), 1, "gone now");
}
