//! One contract, two indexers: the code graph (`CodeIndex`) and the document
//! index (`DocIndex`) answer the same `PathIndexer` calls the same way. The
//! watcher's sinks are written against the trait, so anything one indexer does
//! that the other does not is a bug in one of them.

use infigraph_core::watch::sink::PathIndexer;
use infigraph_languages::bundled_registry;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// What the contract needs from an indexer under test.
trait Harness {
    fn indexer(&self) -> &dyn PathIndexer;
    /// The paths the store holds, root-relative.
    fn stored(&self) -> BTreeSet<String>;
    /// A file extension this indexer indexes, and a body for revision `n`.
    fn ext(&self) -> &'static str;
    fn body(&self, n: u32) -> String;
}

struct Code(infigraph_core::CodeIndex);
struct Docs(infigraph_docs::DocIndex);

impl Harness for Code {
    fn indexer(&self) -> &dyn PathIndexer {
        &self.0
    }
    fn stored(&self) -> BTreeSet<String> {
        self.0
            .store()
            .unwrap()
            .get_file_hashes()
            .unwrap()
            .into_keys()
            .collect()
    }
    fn ext(&self) -> &'static str {
        "py"
    }
    fn body(&self, n: u32) -> String {
        format!("def f():\n    return {n}\n")
    }
}

impl Harness for Docs {
    fn indexer(&self) -> &dyn PathIndexer {
        &self.0
    }
    fn stored(&self) -> BTreeSet<String> {
        self.0
            .store()
            .unwrap()
            .get_doc_hashes()
            .unwrap()
            .into_keys()
            .collect()
    }
    fn ext(&self) -> &'static str {
        "md"
    }
    fn body(&self, n: u32) -> String {
        format!("# Title\n\nrevision {n}\n")
    }
}

fn pinned_environment() {
    std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
    std::env::set_var("INFIGRAPH_NO_WATCH", "1");
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.keep());
}

fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();
    PathBuf::from(rel)
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn the_path_indexer_contract(root: &Path, h: &dyn Harness) {
    let ext = h.ext();
    let (a, b, c, sibling) = (
        write(root, &format!("a.{ext}"), &h.body(1)),
        write(root, &format!("dir/b.{ext}"), &h.body(1)),
        write(root, &format!("dir2/c.{ext}"), &h.body(1)),
        // Shares the text `dir` with the directory below but is not under it.
        write(root, &format!("dir.{ext}"), &h.body(1)),
    );
    let (a_s, b_s, c_s, sibling_s) = (
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
        c.to_string_lossy().into_owned(),
        sibling.to_string_lossy().into_owned(),
    );
    let indexer = h.indexer();

    // Only the paths named are indexed.
    let out = indexer.index_paths(&[a.clone(), b.clone()]).unwrap();
    assert!(out.wrote());
    assert_eq!(h.stored(), set(&[&a_s, &b_s]));

    // A byte-identical rewrite is skipped.
    write(root, &a_s, &h.body(1));
    let out = indexer.index_paths(std::slice::from_ref(&a)).unwrap();
    assert!(!out.wrote(), "unchanged content is not a write");

    // A changed one is not.
    write(root, &a_s, &h.body(2));
    assert!(indexer
        .index_paths(std::slice::from_ref(&a))
        .unwrap()
        .wrote());

    // The whole-project pass finds what no event reported, and only once.
    assert!(indexer.index_all().unwrap().wrote());
    assert_eq!(h.stored(), set(&[&a_s, &b_s, &c_s, &sibling_s]));
    assert!(
        !indexer.index_all().unwrap().wrote(),
        "nothing left to find"
    );

    // A directory goes with everything under it, and nothing beside it.
    let out = indexer.delete_paths(&[PathBuf::from("dir")]).unwrap();
    assert!(out.wrote());
    assert_eq!(h.stored(), set(&[&a_s, &c_s, &sibling_s]));

    // Deleting what is not stored writes nothing.
    assert!(!indexer
        .delete_paths(&[PathBuf::from("dir")])
        .unwrap()
        .wrote());

    // A path asked to be indexed that is no longer on disk is a deletion.
    std::fs::remove_file(root.join(&a_s)).unwrap();
    assert!(indexer.index_paths(&[a]).unwrap().wrote());
    assert_eq!(h.stored(), set(&[&c_s, &sibling_s]));
}

#[test]
fn the_code_graph_keeps_the_path_indexer_contract() {
    pinned_environment();
    let tmp = tempfile::tempdir().unwrap();
    let mut code =
        infigraph_core::CodeIndex::open(tmp.path(), bundled_registry().unwrap()).unwrap();
    code.init().unwrap();
    the_path_indexer_contract(tmp.path(), &Code(code));
}

#[test]
fn the_document_index_keeps_the_path_indexer_contract() {
    pinned_environment();
    let tmp = tempfile::tempdir().unwrap();
    let mut docs = infigraph_docs::DocIndex::open(tmp.path()).unwrap();
    docs.init().unwrap();
    the_path_indexer_contract(tmp.path(), &Docs(docs));
}
