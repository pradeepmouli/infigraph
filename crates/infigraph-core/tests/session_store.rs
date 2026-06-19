use infigraph_core::graph::{SessionData, SessionStore};

fn make_session(id: &str, created_at: i64, updated_at: i64) -> SessionData {
    SessionData {
        id: id.to_string(),
        name: String::new(),
        summary: format!("work on {id}"),
        pending_tasks: String::new(),
        decisions: String::new(),
        files_touched: String::new(),
        constraints: String::new(),
        assumptions: String::new(),
        blockers: String::new(),
        created_at,
        updated_at,
    }
}

#[test]
fn test_save_load_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open_dir(dir.path()).unwrap();
    let s = make_session("session_2026-06-08", 1000, 2000);
    store.save(&s).unwrap();
    let loaded = store.load("session_2026-06-08").unwrap().unwrap();
    assert_eq!(loaded.id, "session_2026-06-08");
    assert_eq!(loaded.updated_at, 2000);
}

#[test]
fn test_list_all_sorted_by_created() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open_dir(dir.path()).unwrap();
    store
        .save(&make_session("session_2026-06-05", 100, 200))
        .unwrap();
    store
        .save(&make_session("session_2026-06-07", 300, 400))
        .unwrap();
    store
        .save(&make_session("session_2026-06-06", 200, 500))
        .unwrap();

    let all = store.list_all().unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, "session_2026-06-07");
    assert_eq!(all[1].id, "session_2026-06-06");
    assert_eq!(all[2].id, "session_2026-06-05");
}

#[test]
fn test_list_by_updated_sorted() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open_dir(dir.path()).unwrap();
    store
        .save(&make_session("session_2026-06-05", 100, 500))
        .unwrap();
    store
        .save(&make_session("session_2026-06-07", 300, 300))
        .unwrap();
    store
        .save(&make_session("session_2026-06-06", 200, 400))
        .unwrap();

    let sorted = store.list_by_updated().unwrap();
    assert_eq!(sorted[0].id, "session_2026-06-05");
    assert_eq!(sorted[1].id, "session_2026-06-06");
    assert_eq!(sorted[2].id, "session_2026-06-07");
}

#[test]
fn test_list_recent_truncates() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open_dir(dir.path()).unwrap();
    store
        .save(&make_session("session_2026-06-05", 100, 100))
        .unwrap();
    store
        .save(&make_session("session_2026-06-06", 200, 200))
        .unwrap();
    store
        .save(&make_session("session_2026-06-07", 300, 300))
        .unwrap();

    let recent = store.list_recent(2).unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].id, "session_2026-06-07");
    assert_eq!(recent[1].id, "session_2026-06-06");
}

#[test]
fn test_delete_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open_dir(dir.path()).unwrap();
    store
        .save(&make_session("session_2026-06-08", 100, 100))
        .unwrap();
    assert!(store.load("session_2026-06-08").unwrap().is_some());
    store.delete("session_2026-06-08").unwrap();
    assert!(store.load("session_2026-06-08").unwrap().is_none());
}

#[test]
fn test_load_nonexistent() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open_dir(dir.path()).unwrap();
    assert!(store.load("session_nope").unwrap().is_none());
}
