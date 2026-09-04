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

    // Give the daemon a window to complete an "everything matches" check
    // cycle, then prove it has NOT exited at the moment the mismatch is
    // introduced -- so the exit polled for below is attributable to the
    // mismatch and not to "it happened to exit anyway". On a slow debug
    // build this window may be spent entirely in startup, in which case the
    // assertion still holds (the coordinator is alive at flip time); only
    // the "completed a matching cycle" half degrades, and the exit-after
    // -mismatch claim below is the one that actually guards the regression.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !handle.is_finished(),
        "coordinator exited before the mismatch was introduced"
    );

    // Now introduce the mismatch: the daemon's OWN in-process build_hash()
    // stays the real one, but the next print-build-hash subprocess it
    // spawns will read this file and report something different.
    std::fs::write(&override_path, "totally-different-fake-hash").unwrap();

    // Poll for the exit rather than sleeping a fixed 2.5s. The check runs on
    // a 1s interval only *once the loop is running*, and reaching the loop is
    // not instant: `run_write_coordinator` builds the whole bundled language
    // registry up front, which its own comment notes costs seconds in a debug
    // build. On a loaded CI runner that startup can swallow this test's
    // entire former budget -- it failed on both macOS and ubuntu having
    // printed no `[watch]` output at all, i.e. the loop had not ticked once,
    // so the old fixed sleep was really asserting whether the coordinator had
    // finished booting, not whether it detects a mismatch.
    let wait_start = std::time::Instant::now();
    let deadline = wait_start + Duration::from_secs(60);
    while !handle.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let waited = wait_start.elapsed();
    if waited > Duration::from_secs(5) {
        // Surfaces the real startup cost on whichever machine ran this, so a
        // future budget decision is made from a measurement rather than a guess.
        eprintln!("[test] coordinator took {waited:?} to self-exit after the mismatch");
    }
    assert!(
        handle.is_finished(),
        "coordinator should have self-exited after detecting the build-hash mismatch \
         (waited {waited:?})"
    );
    handle
        .join()
        .unwrap()
        .expect("coordinator loop returned an error instead of a clean shutdown");

    std::env::remove_var("INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE");
    std::env::remove_var("INFIGRAPH_TEST_BUILD_HASH_CHECK_SECS");
}
