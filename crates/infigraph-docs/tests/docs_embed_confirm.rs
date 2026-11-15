//! A document index run that changes nothing still confirms
//! `docs_embeddings.bin` against its store: the file keeps its bytes (and its
//! old mtime) and its generation marker moves, so `doctor` does not call it
//! stale because `docs.kuzu` moved for a reason that embeds nothing.

use std::time::{Duration, SystemTime};

use infigraph_docs::DocIndex;

fn age(path: &std::path::Path, by: Duration) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - by)
        .unwrap();
}

#[test]
fn a_run_that_changes_nothing_moves_the_embeddings_marker() {
    std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("docs/a.md"),
        "# Title\n\nSome ordinary prose about the system.\n",
    )
    .unwrap();
    infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();

    let mut idx = DocIndex::open(&root).unwrap();
    idx.init().unwrap();
    let first = idx.index().unwrap();
    assert!(first.total_chunks > 0, "the first run embeds something");

    let ig = root.join(".infigraph");
    let bin = ig.join("docs_embeddings.bin");
    assert!(bin.exists(), "the first run wrote the embeddings");
    let bytes = std::fs::read(&bin).unwrap();
    age(&bin, Duration::from_secs(3 * 3600));
    let marker = ig.join("docs_embeddings.bin.generation");
    // The run that embedded is not confirmed: a chunk the embedder failed
    // on would be missing, and the call would still have returned.
    assert!(
        !marker.exists(),
        "a run that wrote embeddings does not confirm them"
    );
    let aged = std::fs::metadata(&bin).unwrap().modified().unwrap();

    let second = idx.index().unwrap();
    assert_eq!(second.indexed_files, 0, "nothing changed");

    assert_eq!(std::fs::read(&bin).unwrap(), bytes, "bytes untouched");
    assert_eq!(std::fs::metadata(&bin).unwrap().modified().unwrap(), aged);
    let confirmed = std::fs::metadata(&marker).unwrap().modified().unwrap();
    assert!(
        confirmed > aged + Duration::from_secs(3600),
        "the marker moved with the run"
    );
}
