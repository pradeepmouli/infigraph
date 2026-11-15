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
        degraded: vec![
            DegradedMode::WatcherMissing {
                root: "/p".to_string(),
            }
            .notice(),
            DegradedMode::TrigramEmbedder.notice(),
            DegradedMode::HnswMissing.notice(),
        ],
        slow_waits: vec![(infigraph_core::graph::names::LOCK.to_string(), 5)],
    };
    let footer = compose_footer(&sig).unwrap();
    let lines: Vec<&str> = footer.lines().collect();
    assert_eq!(lines.len(), 5, "one line per degraded condition: {footer}");
    assert!(lines.iter().all(|l| l.starts_with('⚠')), "{footer}");
    assert!(footer.contains("worker restarted since your previous call"));
    assert!(footer.contains("No file watcher running for /p"));
    assert!(footer.contains("trigram fallback"));
    assert!(footer.contains("HNSW index missing"));
    assert!(footer.contains(&format!(
        "waited 5s for {}",
        infigraph_core::graph::names::LOCK
    )));
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

fn watcher_beacon(sig: &Signals) -> bool {
    sig.degraded
        .iter()
        .any(|n| n.key == infigraph_core::degraded::WATCHER_MISSING)
}

/// An indexed project: `.infigraph/` with a store file in it.
fn indexed_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let tg = dir.path().join(".infigraph");
    std::fs::create_dir_all(&tg).unwrap();
    std::fs::write(tg.join(infigraph_core::graph::names::STEM), b"graph").unwrap();
    dir
}

#[test]
fn watcher_beacon_from_durable_state() {
    let state = HealthState::new();
    state.mark_initialized();
    let dir = indexed_project();
    let tg = dir.path().join(".infigraph");

    // No watcher anywhere: beacon fires, and names the project.
    let sig = gather_signals(&state, "search", Some(dir.path()));
    assert!(watcher_beacon(&sig), "{:?}", sig.degraded);
    let footer = compose_footer(&sig).unwrap();
    assert!(
        footer.contains(&format!(
            "No file watcher running for {}",
            dir.path().display()
        )),
        "{footer}"
    );

    // Another process (simulated: separate fd) holds watch.lock: healthy.
    use fs2::FileExt;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(tg.join("watch.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let state = HealthState::new();
    state.mark_initialized();
    assert!(!watcher_beacon(&gather_signals(
        &state,
        "search",
        Some(dir.path())
    )));
}

/// A daemon that exited after sitting idle left on purpose, and the next use
/// starts one: that is not a warning on every tool call that never needed it.
#[test]
fn a_daemon_that_went_idle_is_not_reported_as_a_missing_watcher() {
    let state = HealthState::new();
    state.mark_initialized();
    let dir = indexed_project();
    infigraph_core::daemon::idle_exit::record(dir.path(), 1800);
    assert!(!watcher_beacon(&gather_signals(
        &state,
        "save_session",
        Some(dir.path())
    )));
}

/// Said once, then again only after the repeat period, like every other mode.
#[test]
fn the_watcher_beacon_is_not_repeated_on_every_call() {
    let state = HealthState::new();
    state.mark_initialized();
    let dir = indexed_project();
    assert!(watcher_beacon(&gather_signals(
        &state,
        "search",
        Some(dir.path())
    )));
    assert!(!watcher_beacon(&gather_signals(
        &state,
        "search",
        Some(dir.path())
    )));
}

#[test]
fn watcher_lifecycle_tools_are_exempt() {
    let state = HealthState::new();
    state.mark_initialized();
    let dir = indexed_project();
    assert!(
        !watcher_beacon(&gather_signals(&state, "stop_watch", Some(dir.path()))),
        "a 'no watcher' beacon right after a deliberate stop_watch is noise"
    );
}

#[test]
fn no_project_means_no_project_scoped_beacons() {
    let state = HealthState::new();
    state.mark_initialized();
    let sig = gather_signals(&state, "compress", None);
    assert!(!watcher_beacon(&sig));
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
    std::fs::write(
        dir.path()
            .join(".infigraph")
            .join(infigraph_core::graph::names::STEM),
        b"graph",
    )
    .unwrap();

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
    std::fs::write(
        dir.path()
            .join(".infigraph")
            .join(infigraph_core::graph::names::STEM),
        b"graph",
    )
    .unwrap();
    for tool in ["get_stats", "doctor"] {
        let sig = gather_signals(&state, tool, Some(dir.path()));
        assert!(sig.degraded.is_empty(), "{tool}: {:?}", sig.degraded);
    }
}

fn shown(
    state: &HealthState,
    root: &std::path::Path,
    modes: &[DegradedMode],
    now: std::time::Instant,
) -> Vec<String> {
    state
        .degraded_due(
            Some(root),
            modes.iter().map(DegradedMode::notice).collect(),
            now,
        )
        .into_iter()
        .map(|n| n.key)
        .collect()
}

/// A lasting degraded mode is shown once, then at most once per
/// `FOOTER_REPEAT_AFTER`: repeating it under every tool call costs tokens
/// and tells the reader nothing new.
#[test]
fn a_lasting_mode_is_shown_once_then_only_after_the_repeat_period() {
    use infigraph_mcp::health::FOOTER_REPEAT_AFTER;
    let state = HealthState::new();
    let root = std::path::Path::new("/project/a");
    let missing = [DegradedMode::EmbeddingsMissing];
    let key = DegradedMode::EmbeddingsMissing.key();
    let t0 = std::time::Instant::now();

    assert_eq!(shown(&state, root, &missing, t0), vec![key]);
    assert!(shown(&state, root, &missing, t0 + FOOTER_REPEAT_AFTER / 2).is_empty());
    assert_eq!(
        shown(&state, root, &missing, t0 + FOOTER_REPEAT_AFTER),
        vec![key]
    );
    // Shown again, so the period starts again.
    assert!(shown(&state, root, &missing, t0 + FOOTER_REPEAT_AFTER * 3 / 2).is_empty());
}

#[test]
fn a_mode_that_appears_while_another_is_quiet_is_shown_alone() {
    let state = HealthState::new();
    let root = std::path::Path::new("/project/a");
    let t0 = std::time::Instant::now();
    shown(&state, root, &[DegradedMode::EmbeddingsMissing], t0);

    let both = [DegradedMode::EmbeddingsMissing, DegradedMode::HnswMissing];
    let later = t0 + std::time::Duration::from_secs(1);
    assert_eq!(
        shown(&state, root, &both, later),
        vec![DegradedMode::HnswMissing.key()]
    );
}

/// The key decides, not the wording: a count that moved is the same mode.
#[test]
fn a_changed_message_under_the_same_key_stays_quiet() {
    let state = HealthState::new();
    let root = std::path::Path::new("/project/a");
    let unwatched = |failed| DegradedMode::UnwatchedDirectories {
        failed,
        first: "x: denied".to_string(),
    };
    let t0 = std::time::Instant::now();
    assert_eq!(shown(&state, root, &[unwatched(2)], t0).len(), 1);
    assert!(shown(&state, root, &[unwatched(5)], t0).is_empty());
}

/// A mode that cleared and came back is news, whatever the clock says.
#[test]
fn a_mode_that_cleared_and_returned_is_shown_at_once() {
    let state = HealthState::new();
    let root = std::path::Path::new("/project/a");
    let missing = [DegradedMode::EmbeddingsMissing];
    let t0 = std::time::Instant::now();
    shown(&state, root, &missing, t0);
    assert!(shown(&state, root, &[], t0).is_empty());
    assert_eq!(shown(&state, root, &missing, t0).len(), 1);
}

#[test]
fn each_project_is_throttled_on_its_own() {
    let state = HealthState::new();
    let missing = [DegradedMode::EmbeddingsMissing];
    let t0 = std::time::Instant::now();
    shown(&state, std::path::Path::new("/project/a"), &missing, t0);
    assert_eq!(
        shown(&state, std::path::Path::new("/project/b"), &missing, t0).len(),
        1
    );
    // Another project's call did not make `a` forget what it had shown.
    assert!(shown(&state, std::path::Path::new("/project/a"), &missing, t0).is_empty());
}

/// Through the real entry point: the second tool call on a project that is
/// still degraded carries no repeat of the line.
#[test]
fn the_second_tool_call_does_not_repeat_a_projects_degraded_mode() {
    let state = HealthState::new();
    state.mark_initialized();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
    std::fs::write(
        dir.path()
            .join(".infigraph")
            .join(infigraph_core::graph::names::STEM),
        b"graph",
    )
    .unwrap();
    let key = DegradedMode::EmbeddingsMissing.key();

    assert!(keys(&gather_signals(&state, "search", Some(dir.path()))).contains(&key));
    // A tool that lists the modes itself neither shows nor resets anything.
    gather_signals(&state, "get_stats", Some(dir.path()));
    assert!(!keys(&gather_signals(&state, "search", Some(dir.path()))).contains(&key));
}

fn installed_is_newer() -> Option<String> {
    Some("ffffffffffff-newer".to_string())
}

fn installed_unknown() -> Option<String> {
    None
}

fn stale_notice(sig: &Signals) -> Option<&infigraph_core::degraded::Notice> {
    sig.degraded
        .iter()
        .find(|n| n.key == infigraph_core::degraded::STALE_BUILD)
}

/// An older build than the installed one is said once, with or without a
/// project, naming both builds; an installed build that cannot be read says
/// nothing; a state that is not asked says nothing.
#[test]
fn a_worker_older_than_the_installed_build_says_so_once() {
    let state = HealthState::checking_installed_build(installed_is_newer);
    state.mark_initialized();
    let sig = gather_signals(&state, "search", None);
    let notice = stale_notice(&sig).expect("stale-build notice");
    assert!(
        notice.message.contains("build ffffffffffff is installed"),
        "{notice:?}"
    );
    assert!(notice.remedy.contains("reconnect"), "{notice:?}");
    assert!(
        stale_notice(&gather_signals(&state, "search", None)).is_none(),
        "once per repeat period, not after every call"
    );

    let unknown = HealthState::checking_installed_build(installed_unknown);
    unknown.mark_initialized();
    assert!(stale_notice(&gather_signals(&unknown, "search", None)).is_none());

    let unasked = HealthState::new();
    unasked.mark_initialized();
    assert!(stale_notice(&gather_signals(&unasked, "search", None)).is_none());
}

#[test]
fn the_stale_build_notice_reaches_the_footer() {
    let state = HealthState::checking_installed_build(installed_is_newer);
    state.mark_initialized();
    let dir = tempfile::tempdir().unwrap();
    let footer = compose_footer(&gather_signals(&state, "search", Some(dir.path()))).unwrap();
    assert!(footer.contains("is installed; reconnect it"), "{footer}");
}
