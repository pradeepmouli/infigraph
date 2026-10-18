//! A session file another writer produced. The hook's file fallback has a
//! Python writer put `time.time()` into the epoch fields (`1792333398.318963`),
//! and `save_session` then failed on that valid JSON on every call, with a
//! message that named neither the field nor the cause. A float epoch is read
//! as whole seconds and written back as an integer; any other bad value is
//! still an error that says which field; and a file that cannot be read is
//! reported, not dropped from the list without a word.

use infigraph_core::degraded;
use infigraph_core::graph::{SessionData, SessionStore};

const FLOATS: &str = r#"{
  "id": "session_2026-10-18",
  "summary": "written by python",
  "created_at": 1792333000,
  "updated_at": 1792333398.318963,
  "confidence": 0.9,
  "last_accessed": 1792333399.9
}"#;

fn readable(id: &str) -> SessionData {
    SessionData {
        id: id.into(),
        summary: "fine".into(),
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

fn store_with(file: &str, text: &str) -> (tempfile::TempDir, SessionStore) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".infigraph/sessions")).unwrap();
    std::fs::write(dir.path().join(".infigraph/sessions").join(file), text).unwrap();
    let store = SessionStore::open(dir.path()).unwrap();
    (dir, store)
}

#[test]
fn float_epochs_load_as_whole_seconds_and_save_back_as_integers() {
    let (dir, store) = store_with("session_2026-10-18.json", FLOATS);

    let loaded = store.load("session_2026-10-18").unwrap().unwrap();
    assert_eq!(loaded.updated_at, 1792333398);
    assert_eq!(loaded.last_accessed, 1792333399);
    assert_eq!(loaded.created_at, 1792333000);

    store.save(&loaded).unwrap();
    let text = std::fs::read_to_string(
        dir.path()
            .join(".infigraph/sessions/session_2026-10-18.json"),
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    for field in ["created_at", "updated_at", "last_accessed"] {
        assert!(value[field].is_i64(), "{field} is not an integer:\n{text}");
    }
    assert_eq!(value["updated_at"], 1792333398);
}

#[test]
fn a_bad_epoch_is_an_error_that_names_the_field_and_the_cause() {
    for (field, bad, cause) in [
        ("updated_at", "\"soon\"", "string"),
        ("created_at", "-5", "negative"),
        ("last_accessed", "-1.5", "negative"),
        ("updated_at", "null", "null"),
    ] {
        let text = FLOATS.replace(
            &format!("\"{field}\": "),
            &format!("\"{field}\": {bad}, \"x_{field}\": "),
        );
        let (_dir, store) = store_with("session_2026-10-18.json", &text);

        let why = match store.load("session_2026-10-18") {
            Ok(loaded) => panic!("{field}={bad} was accepted as {loaded:?}"),
            Err(e) => e.to_string(),
        };

        assert!(
            why.contains(field) && why.contains("session_2026-10-18.json"),
            "{field}={bad} ({cause}): {why}"
        );
    }
}

#[test]
fn the_message_carries_the_parsers_own_words_not_only_the_path() {
    let (_dir, store) = store_with(
        "session_2026-10-18.json",
        "{\"id\": \"session_2026-10-18\", \"summary\": 5}",
    );

    let why = store.load("session_2026-10-18").unwrap_err().to_string();

    assert!(
        why.contains("failed to parse session") && why.contains("expected a string"),
        "{why}"
    );
}

#[test]
fn a_file_that_cannot_be_read_is_reported_not_silently_dropped() {
    let (dir, store) = store_with("session_2026-10-18.json", "{ not json");
    store.save(&readable("session_ok")).unwrap();

    let scan = store.scan().unwrap();

    assert_eq!(scan.sessions.len(), 1, "the readable one is listed");
    assert_eq!(scan.skipped.len(), 1);
    assert!(scan.skipped[0].file.ends_with("session_2026-10-18.json"));
    assert!(
        scan.skipped[0].reason.contains("line 1"),
        "{:?}",
        scan.skipped
    );
    // and the list the tools use says so where a person looks
    store.list_all().unwrap();
    let notices = degraded::gather(dir.path());
    let mode = notices
        .iter()
        .find(|n| n.key == "sessions-unreadable")
        .unwrap_or_else(|| panic!("no notice for the skipped file: {notices:?}"));
    assert!(mode.message.contains("session_2026-10-18.json"), "{mode:?}");
}

#[test]
fn the_notice_goes_once_every_file_reads_again() {
    let (dir, store) = store_with("session_2026-10-18.json", "{ not json");
    store.list_all().unwrap();
    assert!(degraded::gather(dir.path())
        .iter()
        .any(|n| n.key == "sessions-unreadable"));

    std::fs::write(
        dir.path()
            .join(".infigraph/sessions/session_2026-10-18.json"),
        FLOATS,
    )
    .unwrap();
    store.list_all().unwrap();

    assert!(!degraded::gather(dir.path())
        .iter()
        .any(|n| n.key == "sessions-unreadable"));
}
