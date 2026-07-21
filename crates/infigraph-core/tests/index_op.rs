use std::collections::HashMap;
use std::time::Duration;

use infigraph_core::lang::LanguageRegistry;
use infigraph_core::multi::{index_group, Group, Registry, RepoEntry};
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

/// `index_group` must skip a member whose own index operation lock is
/// already held (by another in-flight run) rather than racing it, while
/// still indexing the other members normally.
#[test]
fn test_group_index_skips_locked_member() {
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    // Simulate another in-flight index holding repo-a's op lock.
    let held = match begin_index_op(dir_a.path(), "external-holder", Duration::ZERO).unwrap() {
        IndexOpOutcome::Acquired(g) => g,
        IndexOpOutcome::AlreadyRunning(_) => panic!("fresh lock must acquire"),
    };

    let mut registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };
    registry.repos.insert(
        "repo-a".to_string(),
        RepoEntry {
            name: "repo-a".to_string(),
            path: dir_a.path().to_path_buf(),
            languages: vec![],
            symbol_count: 0,
            module_count: 0,
            last_indexed_commit: None,
        },
    );
    registry.repos.insert(
        "repo-b".to_string(),
        RepoEntry {
            name: "repo-b".to_string(),
            path: dir_b.path().to_path_buf(),
            languages: vec![],
            symbol_count: 0,
            module_count: 0,
            last_indexed_commit: None,
        },
    );
    registry.groups.insert(
        "g".to_string(),
        Group {
            name: "g".to_string(),
            org: String::new(),
            repos: vec!["repo-a".to_string(), "repo-b".to_string()],
            contracts: vec![],
        },
    );

    let results = index_group(&mut registry, "g", false, || Ok(LanguageRegistry::new())).unwrap();
    assert_eq!(results.len(), 2, "both members should appear: {results:?}");

    let (_, _, _, note_a) = results
        .iter()
        .find(|(name, ..)| name == "repo-a")
        .expect("repo-a in results");
    let note_a = note_a.as_ref().expect("repo-a should carry a skip note");
    assert!(note_a.contains("index already in progress"), "{note_a}");
    assert!(
        note_a.contains("external-holder"),
        "holder role in note: {note_a}"
    );
    assert!(
        !note_a.ends_with("— skipped"),
        "skip_reason should drop the trailing suffix: {note_a}"
    );

    let (_, _, _, note_b) = results
        .iter()
        .find(|(name, ..)| name == "repo-b")
        .expect("repo-b in results");
    assert!(
        note_b.is_none(),
        "repo-b has no contending lock and should index normally: {note_b:?}"
    );

    drop(held);
}
