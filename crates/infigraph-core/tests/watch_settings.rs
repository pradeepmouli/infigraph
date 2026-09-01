use std::sync::Mutex;

// INFIGRAPH_WATCH_* vars are process-wide; serialize tests that set them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn reap_scan_interval_reads_renamed_env_var() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_WATCH_REAP_SCAN_SECS", "42");
    assert_eq!(
        infigraph_core::instances::reap_scan_interval().as_secs(),
        42
    );
    std::env::remove_var("INFIGRAPH_WATCH_REAP_SCAN_SECS");
    assert_eq!(
        infigraph_core::instances::reap_scan_interval().as_secs(),
        600
    );
}

#[test]
fn index_via_daemon_mode_uses_permissive_truthy_and_renamed_var() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_WATCH_INDEX_VIA_DAEMON");
    assert!(!infigraph_core::index_via_daemon_mode_enabled());

    std::env::set_var("INFIGRAPH_WATCH_INDEX_VIA_DAEMON", "1");
    assert!(infigraph_core::index_via_daemon_mode_enabled());

    // Permissive convention (approved behavior change from the old
    // strict-"1"-only check): "true" now also means on.
    std::env::set_var("INFIGRAPH_WATCH_INDEX_VIA_DAEMON", "true");
    assert!(infigraph_core::index_via_daemon_mode_enabled());

    std::env::set_var("INFIGRAPH_WATCH_INDEX_VIA_DAEMON", "0");
    assert!(!infigraph_core::index_via_daemon_mode_enabled());

    std::env::remove_var("INFIGRAPH_WATCH_INDEX_VIA_DAEMON");
}
