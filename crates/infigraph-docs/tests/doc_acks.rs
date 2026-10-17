//! Acknowledgements in the document store: an entry reviewed against its
//! symbol as of a commit. They are kept per target (the newest wins), read
//! back from a store that predates them as "none", and survive a full
//! reindex, which wipes the store and rebuilds it.

use std::path::Path;

use infigraph_core::daemon_protocol::DocAck;
use infigraph_docs::backend::DocBackend;
use infigraph_docs::store::DocStore;
use infigraph_docs::DocIndex;

const GLOSSARY: &str = "# Glossary\n\n### `src/a.py::f`\n\nWhy f.\n\n### `src/a.py::g`\n\nWhy g.\n";

fn write(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn index(root: &Path) -> DocIndex {
    let mut idx = DocIndex::open(root).unwrap();
    idx.set_skip_file_embeddings(true);
    idx.init().unwrap();
    idx.index().unwrap();
    idx
}

fn ack(target: &str, commit: &str, commit_time: i64) -> DocAck {
    DocAck {
        target: target.to_string(),
        commit: commit.to_string(),
        commit_time,
        acked_at: 1_800_000_000,
    }
}

fn read(idx: &DocIndex, targets: &[&str]) -> Vec<DocAck> {
    let targets: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
    let mut found: Vec<_> = idx
        .store()
        .unwrap()
        .doc_acks(&targets)
        .unwrap()
        .into_values()
        .collect();
    found.sort_by(|a, b| a.target.cmp(&b.target));
    found
}

#[test]
fn an_ack_is_stored_and_read_back_by_target() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "docs/g.md", GLOSSARY);
    let idx = index(dir.path());

    let stored = idx
        .store()
        .unwrap()
        .upsert_acks(&[ack("src/a.py::f", "aaaa", 100)], &[])
        .unwrap();

    assert_eq!(stored, (1, 0));
    assert_eq!(
        read(&idx, &["src/a.py::f"]),
        [ack("src/a.py::f", "aaaa", 100)]
    );
    assert!(read(&idx, &["src/a.py::g"]).is_empty());
}

#[test]
fn a_newer_ack_replaces_the_target_row_and_an_older_one_does_not() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "docs/g.md", GLOSSARY);
    let idx = index(dir.path());
    let store = idx.store().unwrap();

    store
        .upsert_acks(&[ack("src/a.py::f", "bbbb", 200)], &[])
        .unwrap();
    store
        .upsert_acks(&[ack("src/a.py::f", "aaaa", 100)], &[])
        .unwrap();
    assert_eq!(
        read(&idx, &["src/a.py::f"]),
        [ack("src/a.py::f", "bbbb", 200)],
        "an older review replaced a newer one"
    );

    store
        .upsert_acks(&[ack("src/a.py::f", "cccc", 300)], &[])
        .unwrap();
    assert_eq!(
        read(&idx, &["src/a.py::f"]),
        [ack("src/a.py::f", "cccc", 300)]
    );
}

#[test]
fn dropping_a_target_removes_its_ack_and_only_its_ack() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "docs/g.md", GLOSSARY);
    let idx = index(dir.path());
    let store = idx.store().unwrap();
    store
        .upsert_acks(
            &[
                ack("src/a.py::f", "aaaa", 100),
                ack("src/a.py::g", "aaaa", 100),
            ],
            &[],
        )
        .unwrap();

    let stored = store
        .upsert_acks(&[], &["src/a.py::f".to_string()])
        .unwrap();

    assert_eq!(stored, (0, 1));
    assert!(read(&idx, &["src/a.py::f"]).is_empty());
    assert_eq!(read(&idx, &["src/a.py::g"]).len(), 1);
}

#[test]
fn a_store_without_the_table_reads_as_no_acks() {
    let dir = tempfile::tempdir().unwrap();
    let store = DocStore::open(&dir.path().join(".infigraph/docs.kuzu")).unwrap();
    store
        .connection()
        .unwrap()
        .query("DROP TABLE DocAck")
        .unwrap();

    let found = store.doc_acks(&["src/a.py::f".to_string()]).unwrap();

    assert!(found.is_empty());
    // Writing heals it.
    store
        .upsert_acks(&[ack("src/a.py::f", "aaaa", 100)], &[])
        .unwrap();
    assert_eq!(
        store.doc_acks(&["src/a.py::f".to_string()]).unwrap().len(),
        1
    );
}

#[test]
fn a_full_reindex_keeps_the_acks() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "docs/g.md", GLOSSARY);
    let mut idx = index(dir.path());
    idx.store()
        .unwrap()
        .upsert_acks(&[ack("src/a.py::f", "aaaa", 100)], &[])
        .unwrap();

    idx.reindex().unwrap();

    assert_eq!(
        read(&idx, &["src/a.py::f"]),
        [ack("src/a.py::f", "aaaa", 100)]
    );
}
