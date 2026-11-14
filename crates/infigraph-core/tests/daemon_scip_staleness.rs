//! The daemon's write coordinator re-runs SCIP enrichment on its own when
//! the graph's AST generation has drifted past its SCIP generation by more
//! than `INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD` (R3.3.4 follow-up). This
//! drives `run_write_coordinator` in-process with a fake `on_full_reindex`
//! callback that only records what it was asked to enrich.

use sha2::Digest;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use infigraph_core::graph::{GraphBackend, GraphStore, KuzuBackend};
use infigraph_core::model::FileExtraction;

/// A project whose graph has one Rust module, `scip_generation = 1`, and
/// `ast_generation` three bumps further along -- 3 generations behind.
fn stale_project() -> tempfile::TempDir {
    let project_dir = tempfile::tempdir().unwrap();
    let infigraph_dir = project_dir.path().join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();
    let graph_path = infigraph_dir.join(infigraph_core::graph::names::STEM);

    // The file is on disk and the graph holds its real hash: the daemon's
    // start-up true-up hash-diffs the tree, and a file it found missing
    // would be dropped from the graph, leaving no language to enrich.
    let source = b"pub fn f() {}\n";
    std::fs::create_dir_all(project_dir.path().join("src")).unwrap();
    std::fs::write(project_dir.path().join("src/lib.rs"), source).unwrap();
    let content_hash = format!("{:x}", sha2::Sha256::digest(source));

    {
        let backend = KuzuBackend::open(&graph_path).unwrap();
        backend
            .upsert_files_bulk(
                &[FileExtraction {
                    file: "src/lib.rs".to_string(),
                    language: "rust".to_string(),
                    content_hash: content_hash.clone(),
                    symbols: vec![],
                    relations: vec![],
                    statements: vec![],
                }],
                true,
            )
            .unwrap();
    }
    {
        let store = GraphStore::open(&graph_path).unwrap();
        let lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();
        store
            .stamp_scip_generation_conn(&conn, &lock, None)
            .unwrap();
        for _ in 0..3 {
            store.bump_ast_generation_conn(&conn, &lock).unwrap();
        }
    }
    project_dir
}

type Calls = Arc<Mutex<Vec<infigraph_core::daemon::ScipEnrichJob>>>;

/// Runs the write coordinator on `root` in its own thread, with a fake
/// `on_full_reindex` that only records what it was asked to enrich.
fn start_coordinator(
    root: &std::path::Path,
    calls: &Calls,
) -> (
    mpsc::Sender<()>,
    std::thread::JoinHandle<anyhow::Result<()>>,
) {
    let calls_for_cb = Arc::clone(calls);
    let on_full_reindex: Arc<infigraph_core::daemon::FullReindexCallback> =
        Arc::new(move |_root, job, _token, _submit| {
            calls_for_cb.lock().unwrap().push(job);
        });

    let (stop_tx, stop_rx) = mpsc::channel();
    let daemon_token = tokio_util::sync::CancellationToken::new();
    let root = root.to_path_buf();

    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &root,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50,
            stop_rx,
            |_evt| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true,
            Some(on_full_reindex),
            &daemon_token,
            None,
            None,
        )
    });
    (stop_tx, handle)
}

/// The tests below set process-wide environment variables.
static ENV: Mutex<()> = Mutex::new(());

#[test]
fn coordinator_triggers_scip_enrichment_once_when_staleness_exceeds_threshold() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let project_dir = stale_project();

    // Precondition: the fixture really is what the coordinator will see.
    {
        let graph_path = project_dir
            .path()
            .join(".infigraph")
            .join(infigraph_core::graph::names::STEM);
        let backend = KuzuBackend::open_read_only(&graph_path).unwrap();
        assert_eq!(
            backend.distinct_languages().unwrap(),
            vec!["rust".to_string()]
        );
        let ast = backend.current_ast_generation().unwrap();
        let scip = backend.current_scip_generation().unwrap();
        assert_eq!(scip, 1);
        assert!(ast - scip >= 2, "ast={ast} scip={scip}");
    }

    std::env::set_var("INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD", "2");
    std::env::set_var("INFIGRAPH_SCIP_INDEX_STALENESS_CHECK_SECS", "1");
    std::env::set_var("INFIGRAPH_SCIP_START_GRACE_SECS", "0");

    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let (stop_tx, handle) = start_coordinator(project_dir.path(), &calls);

    // The coordinator builds the full language registry before its loop
    // starts (seconds in a debug build), and the check then waits out one
    // interval -- so poll to a deadline rather than sleep a fixed time.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while calls.lock().unwrap().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "no enrichment request within 20s of starting the coordinator"
        );
        assert!(!handle.is_finished(), "coordinator exited early");
        std::thread::sleep(Duration::from_millis(100));
    }
    {
        let recorded = calls.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "expected exactly one enrichment request, got {recorded:?}"
        );
        assert_eq!(recorded[0].languages, vec!["rust".to_string()]);
        // The job carries the generation enrichment started from, so the
        // import can stamp that rather than whatever the graph is at by the
        // time the indexers finish (see `stamp_scip_generation_conn`).
        assert_eq!(recorded[0].ast_generation, 4, "1 upsert + 3 bumps");
    }

    // The fake callback never bumps scip_generation, so the gap is still
    // over threshold -- but the graph hasn't moved since the attempt, so
    // further checks must not keep re-running the (expensive) indexers.
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "enrichment was retried without any new graph write"
    );

    stop_tx.send(()).unwrap();
    handle
        .join()
        .unwrap()
        .expect("coordinator loop returned an error instead of a clean shutdown");

    std::env::remove_var("INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD");
    std::env::remove_var("INFIGRAPH_SCIP_INDEX_STALENESS_CHECK_SECS");
    std::env::remove_var("INFIGRAPH_SCIP_START_GRACE_SECS");
}

/// A project with no drift at all (SCIP and AST generations equal) whose SCIP
/// import ledger is behind the current revision: the one case only the revision
/// replay can answer. The ledger is a copy of a live one from before revisions.
fn replayable_project() -> tempfile::TempDir {
    let project_dir = tempfile::tempdir().unwrap();
    let infigraph_dir = project_dir.path().join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();
    let graph_path = infigraph_dir.join(infigraph_core::graph::names::STEM);

    let source = b"pub fn f() {}\n";
    std::fs::create_dir_all(project_dir.path().join("src")).unwrap();
    std::fs::write(project_dir.path().join("src/lib.rs"), source).unwrap();
    let content_hash = format!("{:x}", sha2::Sha256::digest(source));

    {
        let backend = KuzuBackend::open(&graph_path).unwrap();
        backend
            .upsert_files_bulk(
                &[FileExtraction {
                    file: "src/lib.rs".to_string(),
                    language: "rust".to_string(),
                    content_hash: content_hash.clone(),
                    symbols: vec![],
                    relations: vec![],
                    statements: vec![],
                }],
                true,
            )
            .unwrap();
    }
    {
        // Stamped at the current AST generation, with no bump after: no drift.
        let store = GraphStore::open(&graph_path).unwrap();
        let lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();
        store
            .stamp_scip_generation_conn(&conn, &lock, None)
            .unwrap();
    }
    std::fs::write(
        infigraph_dir.join("scip-imports.json"),
        r#"{
          "rust-analyzer": {"hash": 5536960326523201910, "scip_generation": 1},
          "scip-go": {"hash": 181646230573971328, "scip_generation": 1},
          "scip-python": {"hash": 16813754624391336735, "scip_generation": 1},
          "scip-typescript": {"hash": 4920164732017185868, "scip_generation": 1}
        }"#,
    )
    .unwrap();
    project_dir
}

/// No drift, but the ledger is behind: the coordinator replays the behind
/// language that the project has, exactly once. The fake callback never updates
/// the ledger, so further checks must not rerun it at the same AST generation.
#[test]
fn coordinator_replays_a_behind_ledger_once_without_any_drift() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let project_dir = replayable_project();

    std::env::set_var("INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD", "2");
    std::env::set_var("INFIGRAPH_SCIP_INDEX_STALENESS_CHECK_SECS", "1");
    std::env::set_var("INFIGRAPH_SCIP_START_GRACE_SECS", "0");

    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let (stop_tx, handle) = start_coordinator(project_dir.path(), &calls);

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while calls.lock().unwrap().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "no replay request within 20s of starting the coordinator"
        );
        assert!(!handle.is_finished(), "coordinator exited early");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(2500));
    {
        let recorded = calls.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "a replay runs once per AST generation, got {recorded:?}"
        );
        assert_eq!(recorded[0].languages, vec!["rust".to_string()]);
        assert_eq!(
            recorded[0].ast_generation, 1,
            "no drift: the generation it stamps"
        );
    }

    stop_tx.send(()).unwrap();
    handle
        .join()
        .unwrap()
        .expect("coordinator loop returned an error instead of a clean shutdown");

    std::env::remove_var("INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD");
    std::env::remove_var("INFIGRAPH_SCIP_INDEX_STALENESS_CHECK_SECS");
    std::env::remove_var("INFIGRAPH_SCIP_START_GRACE_SECS");
}

/// The same drifted graph, and the trigger would fire at once -- but the
/// daemon has just started, and a restart, the true-up and an editing burst
/// settle first. The positive test above sees its request within 20s with the
/// grace at 0, so a quiet 6s here is a real wait, not a coordinator that never
/// got going.
#[test]
fn the_staleness_trigger_waits_out_the_start_grace() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let project_dir = stale_project();
    std::env::set_var("INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD", "2");
    std::env::set_var("INFIGRAPH_SCIP_INDEX_STALENESS_CHECK_SECS", "1");
    std::env::set_var("INFIGRAPH_SCIP_START_GRACE_SECS", "3600");

    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let (stop_tx, handle) = start_coordinator(project_dir.path(), &calls);
    std::thread::sleep(Duration::from_secs(6));
    assert!(
        calls.lock().unwrap().is_empty(),
        "enrichment started inside the start grace: {:?}",
        calls.lock().unwrap()
    );
    stop_tx.send(()).unwrap();
    handle.join().unwrap().unwrap();

    std::env::remove_var("INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD");
    std::env::remove_var("INFIGRAPH_SCIP_INDEX_STALENESS_CHECK_SECS");
    std::env::remove_var("INFIGRAPH_SCIP_START_GRACE_SECS");
}

/// `[scip] enabled = false`: the same drifted graph, the same coordinator,
/// and no enrichment request at all. The positive test above sees its
/// request within 20s, so waiting out 10s here is a real absence, not a
/// coordinator that never got going.
#[test]
fn coordinator_does_not_trigger_scip_enrichment_when_scip_is_off() {
    let project_dir = stale_project();
    infigraph_core::settings_file::set_project_setting(
        project_dir.path(),
        "scip",
        "enabled",
        toml_edit::value(false),
    )
    .unwrap();

    std::env::set_var("INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD", "2");
    std::env::set_var("INFIGRAPH_SCIP_INDEX_STALENESS_CHECK_SECS", "1");
    std::env::set_var("INFIGRAPH_SCIP_START_GRACE_SECS", "0");

    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let (stop_tx, handle) = start_coordinator(project_dir.path(), &calls);

    let until = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < until {
        assert!(!handle.is_finished(), "coordinator exited early");
        std::thread::sleep(Duration::from_millis(100));
    }
    let recorded = calls.lock().unwrap().len();

    stop_tx.send(()).unwrap();
    handle
        .join()
        .unwrap()
        .expect("coordinator loop returned an error instead of a clean shutdown");
    std::env::remove_var("INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD");
    std::env::remove_var("INFIGRAPH_SCIP_INDEX_STALENESS_CHECK_SECS");
    std::env::remove_var("INFIGRAPH_SCIP_START_GRACE_SECS");

    assert_eq!(
        recorded, 0,
        "enrichment was requested with [scip] enabled = false"
    );
}

/// `[scip] disable = ["rust"]`: the only language the graph holds is off, so
/// the staleness trigger starts a task that finds nothing to run and never
/// reaches the callback that runs the indexers. (The test above is the
/// control: the same project, nothing disabled, is enriched.)
#[test]
fn a_disabled_language_is_not_enriched_by_the_staleness_trigger() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let project_dir = stale_project();
    let mut names = toml_edit::Array::new();
    names.push("rust");
    infigraph_core::settings_file::set_project_setting(
        project_dir.path(),
        "scip",
        "disable",
        toml_edit::value(names),
    )
    .unwrap();
    std::env::set_var("INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD", "2");
    std::env::set_var("INFIGRAPH_SCIP_INDEX_STALENESS_CHECK_SECS", "1");
    std::env::set_var("INFIGRAPH_SCIP_START_GRACE_SECS", "0");

    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let (stop_tx, handle) = start_coordinator(project_dir.path(), &calls);
    // Long enough for the registry build and several checks.
    std::thread::sleep(Duration::from_secs(8));
    let seen = calls.lock().unwrap().len();
    stop_tx.send(()).unwrap();
    handle.join().unwrap().expect("clean shutdown");
    std::env::remove_var("INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD");
    std::env::remove_var("INFIGRAPH_SCIP_INDEX_STALENESS_CHECK_SECS");
    std::env::remove_var("INFIGRAPH_SCIP_START_GRACE_SECS");

    assert_eq!(seen, 0, "a disabled language was handed to the indexers");
}
