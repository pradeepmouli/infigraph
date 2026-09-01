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
