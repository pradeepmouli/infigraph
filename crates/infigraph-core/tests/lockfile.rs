use infigraph_core::build_hash;
use infigraph_core::lockfile;
use infigraph_core::lockfile::{Busy, LockInfo};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

/// Serializes the tests that read or mutate INFIGRAPH_SLOW_LOCK_MS — the
/// threshold is process-global, so a lowered value must not leak into a
/// concurrently-running test's window.
static SLOW_LOCK_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    assert!(
        info.acquired_at > 1_700_000_000,
        "acquired_at should be epoch seconds"
    );

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
    assert!(
        msg.contains("4242"),
        "message should name holder pid: {msg}"
    );
    assert!(
        msg.contains("infigraph watch"),
        "message should name role: {msg}"
    );
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
    assert!(
        msg.contains("unknown"),
        "unknown holder should be stated: {msg}"
    );
}

#[test]
fn test_try_acquire_stamps_identity() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.lock");
    let guard = lockfile::try_acquire(&path, "unit-test")
        .unwrap()
        .expect("free lock");
    let holder = lockfile::read_holder(&path).expect("payload written");
    assert_eq!(holder.pid, std::process::id());
    assert_eq!(holder.role, "unit-test");
    assert_eq!(holder.build_hash, build_hash());
    drop(guard);
}

#[test]
fn test_try_acquire_none_when_held() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("b.lock");
    let _guard = lockfile::try_acquire(&path, "first")
        .unwrap()
        .expect("free lock");
    let second = lockfile::try_acquire(&path, "second").unwrap();
    assert!(
        second.is_none(),
        "second handle must not acquire a held lock"
    );
}

#[test]
fn test_release_clears_payload_and_reacquire_works() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("c.lock");
    {
        let _guard = lockfile::try_acquire(&path, "first")
            .unwrap()
            .expect("free lock");
    }
    // After clean release the payload is cleared (empty file), and the lock is free.
    assert!(
        lockfile::read_holder(&path).is_none(),
        "payload should clear on drop"
    );
    let again = lockfile::try_acquire(&path, "second").unwrap();
    assert!(again.is_some(), "lock should be reacquirable after drop");
}

#[test]
fn test_stale_payload_without_flock_is_adopted() {
    // Simulates a holder that died without cleanup (kernel released the
    // flock; stale JSON remains). Acquisition must succeed and overwrite.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("d.lock");
    std::fs::write(
        &path,
        r#"{"pid":999999,"role":"dead","build_hash":"x","acquired_at":1}"#,
    )
    .unwrap();
    let guard = lockfile::try_acquire(&path, "adopter").unwrap();
    assert!(
        guard.is_some(),
        "free flock with stale payload must be adopted"
    );
    let holder = lockfile::read_holder(&path).unwrap();
    assert_eq!(holder.role, "adopter");
}

#[test]
fn test_acquire_waits_then_succeeds() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("e.lock");
    let guard = lockfile::try_acquire(&path, "short-holder")
        .unwrap()
        .expect("free");
    let path2 = path.clone();
    let t = std::thread::spawn(move || {
        // Holder releases after 200ms; waiter has a 5s budget.
        std::thread::sleep(Duration::from_millis(200));
        drop(guard);
    });
    let acquired = lockfile::acquire(&path2, "waiter", Duration::from_secs(5)).unwrap();
    t.join().unwrap();
    assert_eq!(lockfile::read_holder(&path2).unwrap().role, "waiter");
    drop(acquired);
}

#[test]
fn test_acquire_times_out_with_busy_naming_holder() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("f.lock");
    let _guard = lockfile::try_acquire(&path, "long-holder")
        .unwrap()
        .expect("free");
    let err = lockfile::acquire(&path, "impatient", Duration::from_millis(300))
        .expect_err("must time out while held");
    let busy = err.downcast_ref::<Busy>().expect("error must be Busy");
    let holder = busy.holder.as_ref().expect("holder identity readable");
    assert_eq!(holder.role, "long-holder");
    assert_eq!(holder.pid, std::process::id());
    assert!(busy.waited >= Duration::from_millis(300));
}

#[test]
fn test_acquire_timeout_unknown_holder_on_bare_flock() {
    // Old-binary compatibility: flock held but no payload (pre-identity
    // binaries never write one). Must time out as unknown holder, never break.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("g.lock");
    let bare = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    fs2::FileExt::lock_exclusive(&bare).unwrap();
    let err =
        lockfile::acquire(&path, "modern", Duration::from_millis(200)).expect_err("must time out");
    let busy = err.downcast_ref::<Busy>().expect("error must be Busy");
    assert!(busy.holder.is_none(), "bare flock has unknown holder");
    fs2::FileExt::unlock(&bare).unwrap();
}

#[test]
fn test_slow_wait_recorded_and_drained() {
    let _env = SLOW_LOCK_ENV.lock().unwrap_or_else(|e| e.into_inner());
    // Edition 2021: set_var is safe. 50ms threshold keeps the test fast;
    // other tests in this binary never successfully acquire after a
    // >50ms contended wait (contended tests end in Busy, which must NOT
    // record), so cross-test interference is limited to extra entries we
    // filter out by path.
    std::env::set_var("INFIGRAPH_SLOW_LOCK_MS", "50");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("slow.lock");

    let held = lockfile::try_acquire(&path, "holder").unwrap().unwrap();
    let path2 = path.clone();
    let waiter = std::thread::spawn(move || {
        lockfile::acquire(&path2, "waiter", std::time::Duration::from_secs(5)).unwrap()
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    drop(held);
    let _guard = waiter.join().unwrap();

    let waits = lockfile::take_slow_waits();
    assert!(
        waits
            .iter()
            .any(|w| w.lock_path == path && w.waited >= std::time::Duration::from_millis(50)),
        "expected a recorded slow wait for {}, got {waits:?}",
        path.display()
    );
    // Drained: our path must not appear again.
    assert!(
        lockfile::take_slow_waits()
            .iter()
            .all(|w| w.lock_path != path),
        "take_slow_waits must drain recorded events"
    );
    std::env::remove_var("INFIGRAPH_SLOW_LOCK_MS");
}

#[test]
fn test_fast_acquire_records_nothing() {
    let _env = SLOW_LOCK_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fast.lock");
    let g = lockfile::acquire(&path, "solo", std::time::Duration::from_secs(1)).unwrap();
    drop(g);
    assert!(
        lockfile::take_slow_waits()
            .iter()
            .all(|w| w.lock_path != path),
        "uncontended acquire must not record a slow wait"
    );
}
