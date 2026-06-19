use std::time::{Duration, Instant};
use tempfile::TempDir;
use infigraph_core::graph::GraphStore;

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
    assert!(result.is_none(), "try_write_lock should return None when lock is held");
}

#[test]
fn test_try_write_lock_succeeds_when_free() {
    let (_dir, store) = make_store();
    let result = store.try_write_lock().unwrap();
    assert!(result.is_some(), "try_write_lock should return Some when lock is free");
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
        matches!(result, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock)
    });

    let was_blocked = handle.join().unwrap();
    assert!(was_blocked, "second thread should be blocked by held lock");

    drop(lock);
}

#[test]
fn test_write_lock_different_stores_same_path() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("shared.db");
    let store1 = GraphStore::open(&db_path).unwrap();
    let store2 = GraphStore::open(&db_path).unwrap();

    let _lock1 = store1.write_lock().unwrap();
    let result = store2.try_write_lock().unwrap();
    assert!(result.is_none(), "second store should see lock held by first");
}
