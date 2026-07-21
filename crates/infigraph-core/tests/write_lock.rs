use infigraph_core::graph::GraphStore;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn make_store() -> (TempDir, GraphStore) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let store = GraphStore::open(&db_path).unwrap();
    (dir, store)
}

#[test]
fn test_write_lock_acquire_and_release() {
    let (_dir, store) = make_store();
    let lock = store.write_lock().unwrap();
    drop(lock);
    let lock2 = store.write_lock().unwrap();
    drop(lock2);
}

#[test]
fn test_try_write_lock_returns_none_when_held() {
    let (_dir, store) = make_store();
    let _lock = store.write_lock().unwrap();
    let result = store.try_write_lock().unwrap();
    assert!(
        result.is_none(),
        "try_write_lock should return None when lock is held"
    );
}

#[test]
fn test_try_write_lock_succeeds_when_free() {
    let (_dir, store) = make_store();
    let result = store.try_write_lock().unwrap();
    assert!(
        result.is_some(),
        "try_write_lock should return Some when lock is free"
    );
}

#[test]
fn test_write_lock_released_on_drop() {
    let (_dir, store) = make_store();
    {
        let _lock = store.write_lock().unwrap();
    }
    let result = store.try_write_lock().unwrap();
    assert!(result.is_some(), "lock should be released after drop");
}

#[test]
fn test_write_lock_no_perf_impact() {
    let (_dir, store) = make_store();

    let iterations = 1000;
    let start = Instant::now();
    for _ in 0..iterations {
        let _lock = store.write_lock().unwrap();
    }
    let elapsed = start.elapsed();

    let per_op = elapsed / iterations;
    assert!(
        per_op < Duration::from_millis(1),
        "lock acquire+release should take < 1ms, took {:?}",
        per_op
    );
}

#[test]
fn test_write_lock_cross_thread_blocking() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("cross_thread.db");
    let store = GraphStore::open(&db_path).unwrap();

    let lock = store.write_lock().unwrap();

    let lock_path = db_path.with_extension("lock");
    let handle = std::thread::spawn(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        use fs2::FileExt;
        let result = file.try_lock_exclusive();
        matches!(result, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(33))
    });

    let was_blocked = handle.join().unwrap();
    assert!(was_blocked, "second thread should be blocked by held lock");

    drop(lock);
}

#[test]
fn test_write_lock_different_stores_same_path() {
    // Kuzu locks the DB directory, so two GraphStore instances on the same path
    // fail on Windows. Test the lock file directly instead.
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join("shared.lock");

    use fs2::FileExt;
    let file1 = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    file1.lock_exclusive().unwrap();

    let file2 = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    let result = file2.try_lock_exclusive();
    assert!(
        result.is_err(),
        "second fd should fail to lock when first holds it"
    );

    drop(file1);
}

#[test]
fn test_write_lock_stamps_identity_and_busy_on_timeout() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("identity.db");
    let store = GraphStore::open(&db_path).unwrap();

    let _held = store.write_lock().unwrap();
    let holder = infigraph_core::lockfile::read_holder(&db_path.with_extension("lock"))
        .expect("write lock should stamp identity");
    assert_eq!(holder.pid, std::process::id());
    assert_eq!(holder.role, "graph-write");

    // A second store on the same path must time out with Busy, not hang.
    let store2 = GraphStore::open(&db_path); // may fail: kuzu holds its own db lock
    if let Ok(store2) = store2 {
        let err = store2
            .write_lock_with_timeout(std::time::Duration::from_millis(300))
            .expect_err("held lock must yield Busy");
        assert!(
            err.downcast_ref::<infigraph_core::lockfile::Busy>()
                .is_some(),
            "expected Busy, got: {err}"
        );
    }
}
