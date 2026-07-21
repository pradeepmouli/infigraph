use std::time::Duration;

use infigraph_core::ops::{begin_index_op, IndexOpOutcome};
use tempfile::TempDir;

#[test]
fn test_acquire_then_coalesce() {
    let dir = TempDir::new().unwrap();
    let g = match begin_index_op(dir.path(), "test-index", Duration::ZERO).unwrap() {
        IndexOpOutcome::Acquired(g) => g,
        IndexOpOutcome::AlreadyRunning(_) => panic!("free lock must acquire"),
    };
    // Second try coalesces and can render the skip note with holder identity.
    match begin_index_op(dir.path(), "second", Duration::ZERO).unwrap() {
        IndexOpOutcome::Acquired(_) => panic!("held lock must coalesce"),
        o @ IndexOpOutcome::AlreadyRunning(_) => {
            let note = o.skip_note().expect("skip note for AlreadyRunning");
            assert!(note.contains("index already in progress"), "{note}");
            assert!(note.contains("test-index"), "holder role in note: {note}");
            assert!(
                note.contains(&std::process::id().to_string()),
                "holder pid: {note}"
            );
            assert!(note.ends_with("— skipped"), "{note}");
        }
    }
    drop(g);
    // Released → acquirable again.
    assert!(matches!(
        begin_index_op(dir.path(), "third", Duration::ZERO).unwrap(),
        IndexOpOutcome::Acquired(_)
    ));
}

#[test]
fn test_wait_mode_busy_is_error() {
    let dir = TempDir::new().unwrap();
    let _g = begin_index_op(dir.path(), "holder", Duration::ZERO).unwrap();
    let err = begin_index_op(dir.path(), "waiter", Duration::from_millis(200))
        .expect_err("nonzero wait on held lock must Err(Busy), not coalesce");
    assert!(err
        .downcast_ref::<infigraph_core::lockfile::Busy>()
        .is_some());
}

#[test]
fn test_skip_note_unknown_holder() {
    // Bare flock (no payload) → unknown-holder note.
    let dir = TempDir::new().unwrap();
    let lock_path = dir.path().join(".infigraph").join("index.lock");
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let bare = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    fs2::FileExt::lock_exclusive(&bare).unwrap();
    let o = begin_index_op(dir.path(), "x", Duration::ZERO).unwrap();
    let note = o.skip_note().expect("note");
    assert!(note.contains("unknown holder"), "{note}");
    fs2::FileExt::unlock(&bare).unwrap();
}
