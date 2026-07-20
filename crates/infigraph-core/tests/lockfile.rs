use infigraph_core::build_hash;
use infigraph_core::lockfile::{Busy, LockInfo};
use std::path::PathBuf;
use std::time::Duration;

#[test]
fn test_build_hash_is_nonempty() {
    let h = build_hash();
    assert!(!h.is_empty());
    // In a git checkout this is a short sha, possibly "-dirty"; outside git it's "unknown".
    assert!(h == "unknown" || h.len() >= 7, "unexpected build hash: {h}");
}

#[test]
fn test_lockinfo_current_and_roundtrip() {
    let info = LockInfo::current("test-role");
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.role, "test-role");
    assert_eq!(info.build_hash, build_hash());
    assert!(info.acquired_at > 1_700_000_000, "acquired_at should be epoch seconds");

    let json = serde_json::to_string(&info).unwrap();
    let back: LockInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(back.pid, info.pid);
    assert_eq!(back.role, info.role);
}

#[test]
fn test_busy_display_names_holder() {
    let busy = Busy {
        lock_path: PathBuf::from("/tmp/x.lock"),
        holder: Some(LockInfo {
            pid: 4242,
            role: "infigraph watch".into(),
            build_hash: "abc123".into(),
            acquired_at: 0,
        }),
        waited: Duration::from_secs(30),
    };
    let msg = busy.to_string();
    assert!(msg.contains("4242"), "message should name holder pid: {msg}");
    assert!(msg.contains("infigraph watch"), "message should name role: {msg}");
    assert!(msg.contains("30"), "message should mention wait: {msg}");
}

#[test]
fn test_busy_display_unknown_holder() {
    let busy = Busy {
        lock_path: PathBuf::from("/tmp/x.lock"),
        holder: None,
        waited: Duration::from_secs(5),
    };
    let msg = busy.to_string();
    assert!(msg.contains("unknown"), "unknown holder should be stated: {msg}");
}
