//! Regression coverage for pradeepmouli/infigraph#61 (I-21): running
//! `infigraph-mcp --version` while another instance held mcp.lock used to
//! fall through into the normal startup path, whose lock acquisition
//! requested a handover from the live server -- a pure version probe
//! killed the in-use MCP server out from under its clients. Introspection
//! flags must print and exit before ANY lock/registry/handover side
//! effects.

use std::fs;
use std::process::Command;

/// Seeds a lock payload that looks like a live incumbent on a DIFFERENT
/// build -- the exact state that made the old code's takeover eager. Uses
/// our own PID so the holder reads as alive.
fn seed_incumbent_lock(lock_path: &std::path::Path) -> String {
    fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let payload = serde_json::to_string(&infigraph_core::lockfile::LockInfo {
        pid: std::process::id(),
        role: "mcp-primary".to_string(),
        build_hash: "some-older-build".to_string(),
        acquired_at: 1_700_000_000,
        last_heartbeat: 1_700_000_000,
        holder_started_at: 0,
    })
    .unwrap();
    fs::write(lock_path, &payload).unwrap();
    payload
}

fn run_flag(flag: &str) -> (std::process::Output, tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("mcp.lock");
    let payload = seed_incumbent_lock(&lock_path);

    let output = Command::new(env!("CARGO_BIN_EXE_infigraph-mcp"))
        .arg(flag)
        .env("INFIGRAPH_MCP_LOCK_PATH", &lock_path)
        .env_remove("INFIGRAPH_BACKEND")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .expect("failed to spawn infigraph-mcp");
    (output, tmp, payload)
}

fn assert_no_lock_side_effects(tmp: &tempfile::TempDir, original_payload: &str) {
    let lock_path = tmp.path().join("mcp.lock");
    assert_eq!(
        fs::read_to_string(&lock_path).unwrap(),
        original_payload,
        "the incumbent's lock payload must be untouched -- no handover \
         request, no truncation, no takeover"
    );
    let entries: Vec<String> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries,
        vec!["mcp.lock".to_string()],
        "no sibling files (handover requests, instance registrations) may \
         appear next to the lock: {entries:?}"
    );
}

#[test]
fn version_prints_and_exits_without_touching_the_live_lock() {
    let (output, tmp, payload) = run_flag("--version");
    assert!(output.status.success(), "--version must exit 0: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("infigraph-mcp") && stdout.contains(env!("CARGO_PKG_VERSION")),
        "stdout must carry the binary name and version: {stdout:?}"
    );
    assert_no_lock_side_effects(&tmp, &payload);
}

#[test]
fn short_version_flag_behaves_identically() {
    let (output, tmp, payload) = run_flag("-V");
    assert!(output.status.success(), "-V must exit 0: {output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains(env!("CARGO_PKG_VERSION")));
    assert_no_lock_side_effects(&tmp, &payload);
}

#[test]
fn help_prints_usage_and_exits_without_touching_the_live_lock() {
    let (output, tmp, payload) = run_flag("--help");
    assert!(output.status.success(), "--help must exit 0: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Usage:") && stdout.contains("--version"),
        "help must document the flags: {stdout:?}"
    );
    assert_no_lock_side_effects(&tmp, &payload);
}
