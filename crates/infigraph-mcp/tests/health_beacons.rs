use infigraph_core::degraded::DegradedMode;
use infigraph_mcp::health::{compose_footer, gather_signals, HealthState, Signals};

#[test]
fn healthy_signals_produce_no_footer() {
    assert!(compose_footer(&Signals::default()).is_none());
}

#[test]
fn each_condition_renders_one_warning_line() {
    let sig = Signals {
        worker_restarted: true,
        watcher_missing: true,
        degraded: vec![
            DegradedMode::TrigramEmbedder.notice(),
            DegradedMode::HnswMissing.notice(),
        ],
        slow_waits: vec![("graph.lock".to_string(), 5)],
    };
    let footer = compose_footer(&sig).unwrap();
    let lines: Vec<&str> = footer.lines().collect();
    assert_eq!(lines.len(), 5, "one line per degraded condition: {footer}");
    assert!(lines.iter().all(|l| l.starts_with('⚠')), "{footer}");
    assert!(footer.contains("worker restarted since your previous call"));
    assert!(footer.contains("No file watcher running — results may be stale"));
    assert!(footer.contains("trigram fallback"));
    assert!(footer.contains("HNSW index missing"));
    assert!(footer.contains("waited 5s for graph.lock"));
}

#[test]
fn restart_beacon_fires_exactly_once_per_worker() {
    let state = HealthState::new();
    assert!(gather_signals(&state, "search", None).worker_restarted);
    assert!(
        !gather_signals(&state, "search", None).worker_restarted,
        "second call must not repeat the restart warning"
    );
}

#[test]
fn initialized_worker_never_fires_restart_beacon() {
    let state = HealthState::new();
    state.mark_initialized();
    assert!(!gather_signals(&state, "search", None).worker_restarted);
}

#[test]
fn watcher_beacon_from_durable_state() {
    let state = HealthState::new();
    state.mark_initialized();
    let dir = tempfile::tempdir().unwrap();
    let tg = dir.path().join(".infigraph");
    std::fs::create_dir_all(&tg).unwrap();

    // No watcher anywhere: beacon fires.
    assert!(gather_signals(&state, "search", Some(dir.path())).watcher_missing);

    // Another process (simulated: separate fd) holds watch.lock: healthy.
    use fs2::FileExt;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(tg.join("watch.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    assert!(!gather_signals(&state, "search", Some(dir.path())).watcher_missing);
}

#[test]
fn watcher_lifecycle_tools_are_exempt() {
    let state = HealthState::new();
    state.mark_initialized();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
    assert!(
        !gather_signals(&state, "stop_watch", Some(dir.path())).watcher_missing,
        "a 'no watcher' beacon right after a deliberate stop_watch is noise"
    );
}

#[test]
fn no_project_means_no_project_scoped_beacons() {
    let state = HealthState::new();
    state.mark_initialized();
    let sig = gather_signals(&state, "compress", None);
    assert!(!sig.watcher_missing);
    // Only what this process knows about itself, never a project's modes.
    assert!(
        sig.degraded
            .iter()
            .all(|n| n.key == DegradedMode::TrigramEmbedder.key()),
        "{:?}",
        sig.degraded
    );
}

fn keys(sig: &Signals) -> Vec<&str> {
    sig.degraded.iter().map(|n| n.key.as_str()).collect()
}

/// The project's degraded modes come from core's one definition (#75), so a
/// mode added there reaches the footer with no change here.
#[test]
fn a_projects_degraded_modes_reach_the_footer_from_core() {
    let state = HealthState::new();
    state.mark_initialized();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
    std::fs::write(dir.path().join(".infigraph/graph"), b"graph").unwrap();

    let sig = gather_signals(&state, "search", Some(dir.path()));
    let missing = DegradedMode::EmbeddingsMissing;
    assert!(keys(&sig).contains(&missing.key()), "{:?}", sig.degraded);
    let footer = compose_footer(&sig).unwrap();
    assert!(footer.contains(&missing.message()), "{footer}");
}

/// `get_stats` and `doctor` list the degraded modes in their own output;
/// repeating them in the footer of the same reply is noise.
#[test]
fn tools_that_list_degraded_modes_themselves_get_none_in_the_footer() {
    let state = HealthState::new();
    state.mark_initialized();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
    std::fs::write(dir.path().join(".infigraph/graph"), b"graph").unwrap();
    for tool in ["get_stats", "doctor"] {
        let sig = gather_signals(&state, tool, Some(dir.path()));
        assert!(sig.degraded.is_empty(), "{tool}: {:?}", sig.degraded);
    }
}
