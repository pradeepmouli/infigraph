//! `SessionStore` under concurrent use. Five sessions of one project save to
//! one daily file, and `save_session` once failed with "failed to parse
//! session" because a reader met the file between `fs::write`'s truncate and
//! its write. A save is one rename now; a read-modify-write holds
//! `sessions.lock`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use infigraph_core::graph::{SessionData, SessionStore};
use infigraph_core::lockfile;

fn session(id: &str, summary: String) -> SessionData {
    SessionData {
        id: id.to_string(),
        summary,
        name: String::new(),
        pending_tasks: String::new(),
        decisions: String::new(),
        files_touched: String::new(),
        constraints: String::new(),
        assumptions: String::new(),
        blockers: String::new(),
        created_at: 1,
        updated_at: 1,
        confidence: 0.0,
        last_accessed: 0,
    }
}

/// Big enough that a truncate-then-write leaves a visible window.
fn big(n: usize) -> String {
    format!("{n}:{}", "x".repeat(512 * 1024))
}

#[test]
fn a_load_during_a_stream_of_saves_never_sees_a_torn_file() {
    let dir = tempfile::tempdir().unwrap();
    let writer = SessionStore::open_dir(dir.path()).unwrap();
    let reader = SessionStore::open_dir(dir.path()).unwrap();
    writer.save(&session("session_2026-10-18", big(0))).unwrap();
    let stop = Arc::new(AtomicBool::new(false));

    let stop_w = stop.clone();
    let saves = std::thread::spawn(move || {
        let mut n = 1;
        while !stop_w.load(Ordering::Relaxed) {
            writer.save(&session("session_2026-10-18", big(n))).unwrap();
            n += 1;
        }
        n
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut loads = 0;
    while std::time::Instant::now() < deadline {
        let loaded = reader
            .load("session_2026-10-18")
            .unwrap_or_else(|e| panic!("load #{loads} failed: {e:#}"))
            .expect("the session exists from the first save on");
        assert!(loaded.summary.ends_with('x'));
        loads += 1;
    }
    stop.store(true, Ordering::Relaxed);
    let saved = saves.join().unwrap();

    assert!(loads > 10 && saved > 10, "loads {loads}, saves {saved}");
}

#[test]
fn list_all_keeps_a_session_that_is_being_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let writer = SessionStore::open_dir(dir.path()).unwrap();
    let lister = SessionStore::open_dir(dir.path()).unwrap();
    writer.save(&session("session_a", big(0))).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    let saves = std::thread::spawn(move || {
        let mut n = 1;
        while !stop_w.load(Ordering::Relaxed) {
            writer.save(&session("session_a", big(n))).unwrap();
            n += 1;
        }
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        assert_eq!(
            lister.list_all().unwrap().len(),
            1,
            "the session vanished from the list while it was being saved"
        );
    }
    stop.store(true, Ordering::Relaxed);
    saves.join().unwrap();
}

/// What `save_session` does: hold the lock across load, change, save. A touch
/// (every `get_latest_session`, `search_sessions` and context injection does
/// one) is the same read-modify-write and must not write back an old copy.
#[test]
fn a_touch_and_a_locked_update_lose_nothing() {
    const UPDATES: i64 = 150;
    let dir = tempfile::tempdir().unwrap();
    let a = SessionStore::open_dir(dir.path()).unwrap();
    let b = SessionStore::open_dir(dir.path()).unwrap();
    a.save(&session("session_x", "0".into())).unwrap();
    let lock_path = dir.path().join("sessions.lock");

    let updater = std::thread::spawn(move || {
        for _ in 0..UPDATES {
            let _held =
                lockfile::acquire(&lock_path, "session-write", Duration::from_secs(10)).unwrap();
            let mut s = a.load("session_x").unwrap().unwrap();
            s.summary = (s.summary.parse::<i64>().unwrap() + 1).to_string();
            a.save(&s).unwrap();
        }
    });
    let toucher = std::thread::spawn(move || {
        for _ in 0..UPDATES * 2 {
            b.touch_session("session_x").unwrap();
        }
    });
    updater.join().unwrap();
    toucher.join().unwrap();

    let last = SessionStore::open_dir(dir.path())
        .unwrap()
        .load("session_x")
        .unwrap()
        .unwrap();
    assert_eq!(
        last.summary,
        UPDATES.to_string(),
        "a touch overwrote an update with the copy it had read before it"
    );
    assert!(last.last_accessed > 0, "the touch was lost altogether");
}

#[test]
fn a_temp_file_left_by_a_killed_save_is_not_a_session_and_the_old_session_stands() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open_dir(dir.path()).unwrap();
    store
        .save(&session("session_2026-10-18", "before".into()))
        .unwrap();
    // A process killed between the temp write and the rename.
    std::fs::write(
        dir.path().join(".session_2026-10-18.json.tmp-4242-7"),
        "{\"id\": \"session_2026-10-18\", \"summ",
    )
    .unwrap();

    assert_eq!(store.list_all().unwrap().len(), 1);
    let loaded = store.load("session_2026-10-18").unwrap().unwrap();
    assert_eq!(loaded.summary, "before");

    store
        .save(&session("session_2026-10-18", "after".into()))
        .unwrap();
    assert_eq!(
        store.load("session_2026-10-18").unwrap().unwrap().summary,
        "after"
    );
    assert_eq!(store.list_all().unwrap().len(), 1);
}

#[test]
fn a_session_that_was_never_saved_is_absent_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open_dir(dir.path()).unwrap();

    assert!(store.load("session_never_saved").unwrap().is_none());
}
