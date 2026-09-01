use std::sync::mpsc;
use std::time::Duration;

/// Uses a real tempdir project with a `.infigraph` dir (no code needs to
/// actually parse) -- the coordinator loop only needs `root` to exist and
/// be watchable, it doesn't need a real indexed graph for this test.
#[test]
fn coordinator_self_exits_when_build_hash_check_detects_a_mismatch() {
    let project_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project_dir.path().join(".infigraph")).unwrap();

    let override_dir = tempfile::tempdir().unwrap();
    let override_path = override_dir.path().join("build-hash.txt");
    // Start with the REAL build hash so the daemon doesn't self-exit
    // immediately -- the test flips this file's contents after the daemon
    // is up and running, to prove the *next* check picks up the change.
    std::fs::write(&override_path, infigraph_core::build_hash()).unwrap();

    std::env::set_var("INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE", &override_path);
    std::env::set_var("INFIGRAPH_TEST_BUILD_HASH_CHECK_SECS", "1");

    let (_stop_tx, stop_rx) = mpsc::channel();
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
            false,
            None,
            &token_for_thread,
            None,
        )
    });

    // Let the daemon complete at least one "everything matches" check
    // cycle first (proves this isn't just "it happened to exit anyway").
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !handle.is_finished(),
        "coordinator exited before the mismatch was introduced"
    );

    // Now introduce the mismatch: the daemon's OWN in-process build_hash()
    // stays the real one, but the next print-build-hash subprocess it
    // spawns will read this file and report something different.
    std::fs::write(&override_path, "totally-different-fake-hash").unwrap();

    // Wait past at least one more check interval.
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        handle.is_finished(),
        "coordinator should have self-exited after detecting the build-hash mismatch"
    );
    handle
        .join()
        .unwrap()
        .expect("coordinator loop returned an error instead of a clean shutdown");

    std::env::remove_var("INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE");
    std::env::remove_var("INFIGRAPH_TEST_BUILD_HASH_CHECK_SECS");
}
