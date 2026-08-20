use std::time::{Duration, Instant};

/// Serializes tests that mutate the process-global INFIGRAPH_MCP_LOCK_*
/// env vars -- cargo runs this binary's tests on parallel threads, so a
/// lowered override in one test must not leak into another test's window
/// (same lesson as idle.rs's/instances.rs's own env-mutation tests).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn heartbeat_interval_default_and_override() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
    assert_eq!(
        infigraph_mcp::mcp_lock::heartbeat_interval(),
        Duration::from_secs(15)
    );
    std::env::set_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS", "2");
    assert_eq!(
        infigraph_mcp::mcp_lock::heartbeat_interval(),
        Duration::from_secs(2)
    );
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
}

#[test]
fn wedged_threshold_default_and_override() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_WEDGED_SECS");
    assert_eq!(infigraph_mcp::mcp_lock::wedged_threshold_secs(), 60);
    std::env::set_var("INFIGRAPH_MCP_LOCK_WEDGED_SECS", "5");
    assert_eq!(infigraph_mcp::mcp_lock::wedged_threshold_secs(), 5);
    std::env::remove_var("INFIGRAPH_MCP_LOCK_WEDGED_SECS");
}

#[test]
fn acquire_primary_then_busy_then_free_again() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));

    let first = infigraph_mcp::mcp_lock::acquire_primary();
    assert!(first.is_some(), "lock should be free on first acquire");

    let second = infigraph_mcp::mcp_lock::acquire_primary();
    assert!(second.is_none(), "lock is held, second acquire must fail");

    drop(first);

    let third = infigraph_mcp::mcp_lock::acquire_primary();
    assert!(
        third.is_some(),
        "lock must be free again after the holder drops"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
}

#[test]
fn heartbeat_tick_advances_last_heartbeat() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));

    let mut lock = infigraph_mcp::mcp_lock::acquire_primary().expect("lock should be free");
    let path = infigraph_mcp::mcp_lock::lock_path();
    let before = infigraph_core::lockfile::read_holder(&path).unwrap();

    std::thread::sleep(Duration::from_millis(1100));
    infigraph_mcp::mcp_lock::heartbeat_tick(&mut lock);

    let after = infigraph_core::lockfile::read_holder(&path).unwrap();
    assert!(after.last_heartbeat > before.last_heartbeat);

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
}

#[test]
fn check_wedged_and_log_does_not_panic_on_fresh_or_stale_heartbeat() {
    // This function's only observable effect is a log line; there's no
    // return value to assert on directly (mcp_log has no test hook). This
    // test exists to catch a panic (e.g. an integer underflow bug in the
    // staleness math) on both a fresh and a very stale heartbeat -- the
    // real coverage of the underlying pure math is
    // `is_holder_wedged_pure_cases` in infigraph-core's own lockfile.rs
    // tests (Task 1).
    let fresh = infigraph_core::lockfile::LockInfo {
        pid: 1,
        role: "mcp-primary".to_string(),
        build_hash: "abc".to_string(),
        acquired_at: 1000,
        last_heartbeat: 1000,
        holder_started_at: 0,
    };
    infigraph_mcp::mcp_lock::check_wedged_and_log(&fresh, 1005);

    let stale = infigraph_core::lockfile::LockInfo {
        pid: 1,
        role: "mcp-primary".to_string(),
        build_hash: "abc".to_string(),
        acquired_at: 1000,
        last_heartbeat: 1000,
        holder_started_at: 0,
    };
    infigraph_mcp::mcp_lock::check_wedged_and_log(&stale, 1000 + wedged_secs_for_test() + 1);
}

fn wedged_secs_for_test() -> u64 {
    infigraph_mcp::mcp_lock::wedged_threshold_secs()
}

/// The build_hash comparison is the ONLY thing that gates a takeover
/// request in `acquire_with_takeover` -- staleness alone never triggers
/// one (see `check_wedged_and_log`, which only logs). Tested directly with
/// hand-supplied strings, same pattern as `is_stale`/`should_exit_idle`/
/// `is_holder_wedged` elsewhere in this codebase: a same-process
/// integration test can't produce a genuine mismatch, since both the
/// incumbent and challenger would compute the same real
/// `infigraph_core::build_hash()` -- see the doc comment on
/// `takeover_succeeds_when_incumbent_honors_handover_request` below for how
/// that test compensates.
#[test]
fn build_hash_mismatch_pure_cases() {
    assert!(!infigraph_mcp::mcp_lock::build_hash_mismatch("abc", "abc"));
    assert!(infigraph_mcp::mcp_lock::build_hash_mismatch("abc", "def"));
    assert!(!infigraph_mcp::mcp_lock::build_hash_mismatch("", ""));
}

#[test]
fn takeover_poll_interval_default_and_override() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS");
    assert_eq!(
        infigraph_mcp::mcp_lock::takeover_poll_interval(),
        Duration::from_secs(1)
    );
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS", "3");
    assert_eq!(
        infigraph_mcp::mcp_lock::takeover_poll_interval(),
        Duration::from_secs(3)
    );
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS");
}

#[test]
fn takeover_wait_timeout_default_and_override() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
    assert_eq!(
        infigraph_mcp::mcp_lock::takeover_wait_timeout(),
        Duration::from_secs(10)
    );
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS", "4");
    assert_eq!(
        infigraph_mcp::mcp_lock::takeover_wait_timeout(),
        Duration::from_secs(4)
    );
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
}

#[test]
fn acquire_with_takeover_wins_immediately_when_lock_is_free() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));

    let outcome = infigraph_mcp::mcp_lock::acquire_with_takeover();
    match outcome {
        infigraph_mcp::mcp_lock::AcquireOutcome::Primary(_) => {}
        infigraph_mcp::mcp_lock::AcquireOutcome::Secondary => {
            panic!("must win immediately when the lock is free")
        }
    }

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
}

/// Two builds with DIFFERENT build_hash would have the challenger request
/// and win a handover once the incumbent's own heartbeat loop honors it --
/// but a same-process test can't produce a genuine build_hash mismatch via
/// the public `acquire_with_takeover()` (both sides compute the same real
/// `infigraph_core::build_hash()`; see `build_hash_mismatch_pure_cases` for
/// direct coverage of that gate, and the
/// `takeover_wins_when_incumbent_honors_request_mid_poll` /
/// `takeover_times_out_to_secondary_and_clears_request` pair for full-loop
/// coverage via `acquire_with_takeover_using`). This test exercises the
/// handover *mechanics* that fire once a request already exists on disk:
/// `write_handover_request` stands in for whatever a real challenger's
/// `acquire_with_takeover` would have written after a genuine mismatch, and
/// `heartbeat_and_check_handover` is the real incumbent-side function under
/// test -- it must see the request, log, clear it, and signal the caller to
/// release and exit. After the incumbent drops its lock, a fresh acquire
/// must succeed, standing in for the challenger's poll loop finally
/// winning. It doubles as the "fresh request from a live PID is still
/// honored" case of the staleness gate: `write_handover_request` stamps
/// `requested_at = now` and this process's own (obviously alive) PID.
#[test]
fn takeover_succeeds_when_incumbent_honors_handover_request() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));

    let mut incumbent = infigraph_mcp::mcp_lock::acquire_primary().expect("free");

    infigraph_mcp::mcp_lock::write_handover_request().expect("write handover request");
    let handover_path = dir.path().join("mcp.lock.handover");
    assert!(
        handover_path.exists(),
        "handover request must be written to disk before the incumbent can see it"
    );

    let handed_over = infigraph_mcp::mcp_lock::heartbeat_and_check_handover(&mut incumbent);
    assert!(
        handed_over,
        "incumbent must see the pending handover request"
    );
    assert!(
        !handover_path.exists(),
        "handover request must be cleared once honored"
    );

    drop(incumbent);

    let reacquired = infigraph_mcp::mcp_lock::acquire_primary();
    assert!(
        reacquired.is_some(),
        "lock must be free for a challenger to win after the incumbent releases it"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
}

/// The build_hash comparison in `acquire_with_takeover` is what gates
/// whether a handover request is even written. Since both the incumbent
/// and challenger in an in-process test share the same real
/// `infigraph_core::build_hash()`, this test verifies the SAME-build path
/// directly: no handover request should appear on disk, and the
/// challenger must give up as Secondary without ever unblocking the
/// incumbent.
#[test]
fn no_handover_request_written_when_build_hash_matches() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS", "1");
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS", "1");

    let _incumbent = infigraph_mcp::mcp_lock::acquire_primary().expect("free");

    // The same-build path must return immediately (no handover ever
    // written, so no poll/timeout wait). Without this timing check, this
    // test can't actually distinguish "no request was ever written" from
    // "a request was written and then cleaned up after the (here,
    // 20s-floored) timeout elapsed" -- both leave the handover file
    // absent by the time the assertions below run. `effective_takeover_wait_timeout()`
    // floors the wait at 20s regardless of the 1s env overrides above, so
    // a generous 2s bound cleanly separates the two cases.
    let started = Instant::now();
    let outcome = infigraph_mcp::mcp_lock::acquire_with_takeover();
    let elapsed = started.elapsed();
    match outcome {
        infigraph_mcp::mcp_lock::AcquireOutcome::Secondary => {}
        infigraph_mcp::mcp_lock::AcquireOutcome::Primary(_) => {
            panic!("must not win the lock when build_hash matches the incumbent's")
        }
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "same-build path must return immediately without writing/polling a \
         handover request, not after a ~20s timeout wait (took {elapsed:?})"
    );
    let handover_path = dir.path().join("mcp.lock.handover");
    assert!(
        !handover_path.exists(),
        "no handover request should be written or left behind on the same-build path"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
}

/// Regression: the raw `takeover_wait_timeout()` default (10s) is SHORTER
/// than the `heartbeat_interval()` default (15s), so a challenger would give
/// up before the incumbent's next tick could ever read the request -- the
/// handshake failed ~1/3 of the time by construction, and a tick landing
/// just inside the deadline could leave zero primaries. The deadline must
/// therefore be derived from `effective_takeover_wait_timeout()`, which
/// enforces the margin in code rather than by convention -- but bounded,
/// because the entire wait is dead time before the MCP transport is up and a
/// wedged incumbent makes the challenger burn all of it.
#[test]
fn effective_takeover_wait_timeout_keeps_bounded_margin_over_heartbeat_interval() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");

    // Defaults: raw timeout 10s < heartbeat 15s -- the original bug.
    assert!(
        infigraph_mcp::mcp_lock::takeover_wait_timeout()
            < infigraph_mcp::mcp_lock::heartbeat_interval(),
        "precondition: the raw defaults are the ones that produced the bug"
    );
    let effective = infigraph_mcp::mcp_lock::effective_takeover_wait_timeout();
    assert_eq!(
        effective,
        Duration::from_secs(20),
        "default effective wait must be heartbeat + 5s, at the 20s cap"
    );
    assert!(
        effective > infigraph_mcp::mcp_lock::heartbeat_interval(),
        "effective wait {effective:?} must still outlast one whole heartbeat tick, \
         or the incumbent never gets a chance to honor the request"
    );
    assert!(
        effective < Duration::from_secs(30),
        "effective wait {effective:?} is dead time before the MCP transport exists -- \
         it must stay well inside a ~30s client startup budget"
    );

    // A heartbeat interval large enough that heartbeat + 5s would exceed the
    // cap: the cap wins, and the tick-covering guarantee is knowingly given
    // up (documented on `effective_takeover_wait_timeout`).
    std::env::set_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS", "30");
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS", "5");
    assert_eq!(
        infigraph_mcp::mcp_lock::effective_takeover_wait_timeout(),
        Duration::from_secs(20),
        "the derived margin must never push the wait past the startup-latency cap"
    );

    // Below the cap, the margin is the full heartbeat + 5s.
    std::env::set_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS", "8");
    assert_eq!(
        infigraph_mcp::mcp_lock::effective_takeover_wait_timeout(),
        Duration::from_secs(13),
        "under the cap the derived wait must cover a whole tick plus jitter"
    );

    // An explicitly-configured timeout larger than the margin still wins: the
    // cap bounds only the wait an operator inherited, not one they chose.
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS", "300");
    assert_eq!(
        infigraph_mcp::mcp_lock::effective_takeover_wait_timeout(),
        Duration::from_secs(300),
        "an operator-configured timeout above the margin must be honored as-is"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
}

/// The handover file is the only channel by which one process can tell
/// another to exit, so a request that outlived the challenger that wrote it
/// must never be honored: the next unrelated primary would read it on its
/// very first tick and exit for no reason.
fn write_raw_handover_request(dir: &std::path::Path, pid: u32, requested_at: u64) {
    let json =
        format!(r#"{{"pid":{pid},"build_hash":"other-build","requested_at":{requested_at}}}"#);
    std::fs::write(dir.join("mcp.lock.handover"), json).expect("write handover request");
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[test]
fn stale_handover_request_is_discarded_not_honored() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));

    let mut incumbent = infigraph_mcp::mcp_lock::acquire_primary().expect("free");

    // Live PID (our own), but written long enough ago that whatever wrote
    // it has certainly given up waiting.
    write_raw_handover_request(dir.path(), std::process::id(), now_epoch_secs() - 100_000);

    let handed_over = infigraph_mcp::mcp_lock::heartbeat_and_check_handover(&mut incumbent);
    assert!(
        !handed_over,
        "a handover request older than the challenger's own wait window must not kill the primary"
    );
    assert!(
        !dir.path().join("mcp.lock.handover").exists(),
        "the stale request must be cleared, not left to be re-read every tick"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
}

#[test]
fn handover_request_from_dead_pid_is_discarded() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));

    let mut incumbent = infigraph_mcp::mcp_lock::acquire_primary().expect("free");

    // Fresh timestamp, but no process can hold this PID -- it is above every
    // platform's pid_max, so it is never assigned.
    write_raw_handover_request(dir.path(), i32::MAX as u32, now_epoch_secs());

    let handed_over = infigraph_mcp::mcp_lock::heartbeat_and_check_handover(&mut incumbent);
    assert!(
        !handed_over,
        "a handover request whose requester is gone must not kill the primary"
    );
    assert!(
        !dir.path().join("mcp.lock.handover").exists(),
        "the orphaned request must be cleared, not left to be re-read every tick"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
}

/// Full request -> poll -> win loop, with real threads and real polling. A
/// genuine build-hash mismatch is impossible between two "processes" in one
/// test binary, so the challenger side goes through
/// `acquire_with_takeover_using` with a hand-supplied fake own-build hash
/// against a real incumbent holding the real `infigraph_core::build_hash()`.
#[test]
fn takeover_wins_when_incumbent_honors_request_mid_poll() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));
    std::env::set_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS", "1");
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS", "1");
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS", "1");

    let mut incumbent = infigraph_mcp::mcp_lock::acquire_primary().expect("free");
    let handover_path = dir.path().join("mcp.lock.handover");

    let challenger = std::thread::spawn(|| {
        let started = std::time::Instant::now();
        let outcome = infigraph_mcp::mcp_lock::acquire_with_takeover_using("challenger-build-hash");
        (outcome, started.elapsed())
    });

    // Wait for the challenger to actually publish its request rather than
    // guessing at a sleep duration.
    let mut waited = Duration::ZERO;
    while !handover_path.exists() && waited < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
        waited += Duration::from_millis(20);
    }
    assert!(
        handover_path.exists(),
        "challenger must publish a handover request on a build-hash mismatch"
    );

    // The incumbent's heartbeat tick sees the request and honors it.
    assert!(
        infigraph_mcp::mcp_lock::heartbeat_and_check_handover(&mut incumbent),
        "incumbent must honor a fresh request from a live challenger"
    );
    drop(incumbent);

    let (outcome, elapsed) = challenger.join().expect("challenger thread");
    match outcome {
        infigraph_mcp::mcp_lock::AcquireOutcome::Primary(_) => {}
        infigraph_mcp::mcp_lock::AcquireOutcome::Secondary => {
            panic!("challenger's poll loop must win the lock once the incumbent releases it")
        }
    }
    // The effective wait here is 6s (the 1s heartbeat interval + the 5s
    // fixed margin). Winning well inside that proves the in-loop poll is what
    // acquired the lock,
    // not the post-deadline last-chance attempt -- otherwise a broken poll
    // loop would still show up as Primary and this test would prove nothing.
    assert!(
        elapsed < Duration::from_millis(2500),
        "challenger took {elapsed:?} -- it won via the post-deadline fallback, \
         not the poll loop"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
}

/// Same full loop, but the incumbent never runs a heartbeat tick (a wedged
/// or simply too-slow primary). The challenger must fall back to Secondary
/// once its effective wait elapses -- and must not leak its request file,
/// or the next unrelated primary would be killed by it.
#[test]
fn takeover_times_out_to_secondary_and_clears_request() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("INFIGRAPH_MCP_LOCK_PATH", dir.path().join("mcp.lock"));
    std::env::set_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS", "1");
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS", "1");
    std::env::set_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS", "1");

    let _incumbent = infigraph_mcp::mcp_lock::acquire_primary().expect("free");

    let outcome = infigraph_mcp::mcp_lock::acquire_with_takeover_using("challenger-build-hash");
    match outcome {
        infigraph_mcp::mcp_lock::AcquireOutcome::Secondary => {}
        infigraph_mcp::mcp_lock::AcquireOutcome::Primary(_) => {
            panic!("challenger must not win while the incumbent still holds the lock")
        }
    }
    assert!(
        !dir.path().join("mcp.lock.handover").exists(),
        "a timed-out challenger must clear its own request instead of leaking it"
    );

    std::env::remove_var("INFIGRAPH_MCP_LOCK_PATH");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS");
    std::env::remove_var("INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS");
}
