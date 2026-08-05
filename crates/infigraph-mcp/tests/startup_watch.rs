//! Coverage for proactive startup watching
//! (`infigraph_mcp::recovery::start_daemon_watchers_for_all_registered`):
//! when the MCP server becomes primary under daemon mode
//! (`INFIGRAPH_BACKEND=daemon`), already-registered projects must start
//! being watched immediately, without waiting for some future write to
//! trigger `auto_start_watch` reactively — and the new
//! `[watch].auto_start_on_boot` / `INFIGRAPH_AUTO_START_WATCH` toggle must
//! actually suppress that effect when turned off, not just exist as an
//! unused config knob.
//!
//! `start_daemon_watchers_for_all_registered` lives in the library crate
//! (`infigraph-mcp/src/recovery.rs`), not in `main.rs`, specifically so it's
//! reachable from here — `main.rs` compiles to a separate `[[bin]]` target
//! with no unit-test history of its own (see the doc comment on that
//! function for the full rationale). It is synchronous by design (unlike
//! `main.rs`'s thin wrapper, which spawns a thread around it), so this test
//! can observe its effect deterministically.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Serializes tests that mutate process-global env vars, mirroring
/// `watcher_daemon_mode.rs`'s `ENV_LOCK` (not reusable across integration
/// test binaries, since each `tests/*.rs` file compiles to its own crate).
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn wait_for_watch_lock_state(
    lock_path: &std::path::Path,
    want_held: bool,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let held = infigraph_core::lockfile::try_acquire(lock_path, "test-probe")
            .map(|g| g.is_none())
            .unwrap_or(false);
        if held == want_held {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Given `INFIGRAPH_BACKEND=daemon` and a registered project that has never
/// had a write, `start_daemon_watchers_for_all_registered` starts watching
/// it on its own — proving proactive startup watching actually works, not
/// just that some helper exists. Folds in the negative case in the same
/// test (same setup, same lock-file observable): with
/// `INFIGRAPH_AUTO_START_WATCH=0`, the identical call must NOT spawn a
/// daemon — the config toggle must really suppress the effect.
#[test]
fn start_daemon_watchers_for_all_registered_respects_boot_toggle() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // This test needs a real `infigraph` CLI binary to spawn (daemon-mode
    // watching re-execs it) — skip rather than fail if this test binary was
    // built without it, matching `watch_daemon.rs`'s
    // `init_daemon_backend_starts_a_daemon` precedent (infigraph-mcp has no
    // dev-dependency on infigraph-cli, so cargo won't build it for us).
    let Ok(_cli) = infigraph_core::watch::daemon::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    ) else {
        eprintln!("skipping: infigraph CLI binary not built in this target dir");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    let lock_path = root.join(".infigraph").join("watch.lock");

    std::env::set_var("INFIGRAPH_BACKEND", "daemon");

    // --- Negative case: boot toggle explicitly off. ---
    std::env::set_var("INFIGRAPH_AUTO_START_WATCH", "0");
    infigraph_mcp::recovery::start_daemon_watchers_for_all_registered(Some(&root), &[], None);
    assert!(
        !wait_for_watch_lock_state(&lock_path, true, Duration::from_millis(800)),
        "INFIGRAPH_AUTO_START_WATCH=0 must suppress proactive startup watching, \
         but a daemon acquired watch.lock anyway"
    );

    // --- Positive case: boot toggle on (explicit, not relying on default \
    // resolution against whatever config.toml might be discoverable from \
    // this test binary's cwd). ---
    std::env::set_var("INFIGRAPH_AUTO_START_WATCH", "1");
    infigraph_mcp::recovery::start_daemon_watchers_for_all_registered(Some(&root), &[], None);
    let started = wait_for_watch_lock_state(&lock_path, true, Duration::from_secs(15));

    // Clean up unconditionally before asserting, so a failed assertion can't
    // leak a daemon process or a held lock.
    std::fs::write(root.join(".infigraph").join("watch.stop"), "").unwrap();
    wait_for_watch_lock_state(&lock_path, false, Duration::from_secs(15));

    std::env::remove_var("INFIGRAPH_AUTO_START_WATCH");
    std::env::remove_var("INFIGRAPH_BACKEND");

    assert!(
        started,
        "expected start_daemon_watchers_for_all_registered to start a daemon \
         watching the registered project when daemon mode + the boot toggle are both on"
    );
}

/// `auto_start_watch_on_boot_enabled` itself: env var overrides config file
/// overrides the hardcoded default (on), matching the priority convention
/// documented on `get_ml_compression_mode` in the same file.
#[test]
fn auto_start_watch_on_boot_enabled_env_override_priority() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    std::env::set_var("INFIGRAPH_AUTO_START_WATCH", "0");
    assert!(!infigraph_mcp::session_context::auto_start_watch_on_boot_enabled());

    std::env::set_var("INFIGRAPH_AUTO_START_WATCH", "false");
    assert!(!infigraph_mcp::session_context::auto_start_watch_on_boot_enabled());

    std::env::set_var("INFIGRAPH_AUTO_START_WATCH", "1");
    assert!(infigraph_mcp::session_context::auto_start_watch_on_boot_enabled());

    std::env::remove_var("INFIGRAPH_AUTO_START_WATCH");
}
