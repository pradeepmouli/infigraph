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
        trigram_fallback: true,
        hnsw_missing: true,
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
    assert!(!sig.hnsw_missing);
}
