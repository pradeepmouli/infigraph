//! The daemon's write coordinator re-runs SCIP enrichment on its own when
//! the graph's AST generation has drifted past its SCIP generation by more
//! than `INFIGRAPH_SCIP_INDEX_STALENESS_THRESHOLD` (R3.3.4 follow-up). This
//! drives `run_write_coordinator` in-process with a fake `on_full_reindex`
//! callback that only records what it was asked to enrich.

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
    let graph_path = infigraph_dir.join("graph");

    {
        let backend = KuzuBackend::open(&graph_path).unwrap();
        backend
            .upsert_files_bulk(
                &[FileExtraction {
                    file: "src/lib.rs".to_string(),
                    language: "rust".to_string(),
                    content_hash: "aaa".to_string(),
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
        store.stamp_scip_generation_conn(&conn, &lock).unwrap();
        for _ in 0..3 {
            store.bump_ast_generation_conn(&conn, &lock).unwrap();
        }
    }
    project_dir
}

#[test]
fn coordinator_triggers_scip_enrichment_once_when_staleness_exceeds_threshold() {
    let project_dir = stale_project();

    // Precondition: the fixture really is what the coordinator will see.
    {
        let graph_path = project_dir.path().join(".infigraph").join("graph");
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

    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let calls_for_cb = Arc::clone(&calls);
    let on_full_reindex: Arc<infigraph_core::daemon::FullReindexCallback> =
        Arc::new(move |_prism, languages, _token| {
            calls_for_cb.lock().unwrap().push(languages);
        });

    let (stop_tx, stop_rx) = mpsc::channel();
    let daemon_token = tokio_util::sync::CancellationToken::new();
    let token_for_thread = daemon_token.clone();
    let root = project_dir.path().to_path_buf();

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
            &token_for_thread,
            None,
        )
    });

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
        assert_eq!(recorded[0], vec!["rust".to_string()]);
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
}
