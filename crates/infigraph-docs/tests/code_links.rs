//! Code links in the document store: recorded when a document is indexed,
//! replaced when it changes, gone when it is, and filled in for a store that
//! an older build wrote.

use std::path::Path;

use infigraph_docs::code_links::{LinkKind, CODE_LINKS_VERSION};
use infigraph_docs::store::DocStore;
use infigraph_docs::DocIndex;

const GLOSSARY: &str = "# Glossary\n\n### `src/a.py::f`\n\nWhy f.\n\n#### body\n\nInside f.\n\n### `src/a.py::g`\n\nWhy g.\n\nSee `src/a.py::f` and `Engine::parse`.\n";
const OTHER: &str = "# Other\n\n## `src/b.py::h`\n\nWhy h.\n";

fn write(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// An index run over `root`'s documents (the store stays open in `idx`).
fn index(root: &Path) -> DocIndex {
    let mut idx = DocIndex::open(root).unwrap();
    idx.set_skip_file_embeddings(true);
    idx.init().unwrap();
    idx.index().unwrap();
    idx
}

fn targets(idx: &DocIndex, kind: LinkKind) -> Vec<(String, String)> {
    let mut rows: Vec<_> = idx
        .store()
        .unwrap()
        .code_link_rows(kind)
        .unwrap()
        .into_iter()
        .map(|r| (r.doc_file, r.target))
        .collect();
    rows.sort();
    rows
}

fn pair(doc: &str, target: &str) -> (String, String) {
    (doc.to_string(), target.to_string())
}

#[test]
fn a_documents_entries_and_mentions_are_recorded_with_their_text_and_lines() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "docs/g.md", GLOSSARY);
    let idx = index(dir.path());

    assert_eq!(
        targets(&idx, LinkKind::Documents),
        [
            pair("docs/g.md", "src/a.py::f"),
            pair("docs/g.md", "src/a.py::g")
        ]
    );
    let f = idx
        .store()
        .unwrap()
        .code_links_for(LinkKind::Documents, &["src/a.py::f".to_string()])
        .unwrap();
    assert_eq!(f.len(), 1);
    assert_eq!((f[0].line_start, f[0].line_end), (3, 9));
    assert!(f[0].text.contains("Inside f."), "{:?}", f[0].text);
    assert!(!f[0].text.contains("Why g."));

    assert_eq!(
        targets(&idx, LinkKind::Mentions),
        [
            pair("docs/g.md", "Engine::parse"),
            pair("docs/g.md", "src/a.py::f")
        ]
    );
    let scans = idx.store().unwrap().code_link_scans().unwrap();
    assert_eq!(scans.get("docs/g.md"), Some(&CODE_LINKS_VERSION));
}

#[test]
fn reindexing_a_changed_document_replaces_only_its_own_links() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "docs/g.md", GLOSSARY);
    write(dir.path(), "docs/o.md", OTHER);
    drop(index(dir.path()));

    write(
        dir.path(),
        "docs/g.md",
        "# Glossary\n\n### `src/a.py::renamed`\n\nWhy.\n",
    );
    let idx = index(dir.path());

    assert_eq!(
        targets(&idx, LinkKind::Documents),
        [
            pair("docs/g.md", "src/a.py::renamed"),
            pair("docs/o.md", "src/b.py::h")
        ]
    );
    assert!(
        targets(&idx, LinkKind::Mentions).is_empty(),
        "the old mentions went with the old text"
    );
}

#[test]
fn a_deleted_document_takes_its_links_and_its_scan_with_it() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "docs/g.md", GLOSSARY);
    write(dir.path(), "docs/o.md", OTHER);
    drop(index(dir.path()));

    std::fs::remove_file(dir.path().join("docs/g.md")).unwrap();
    let idx = index(dir.path());

    assert_eq!(
        targets(&idx, LinkKind::Documents),
        [pair("docs/o.md", "src/b.py::h")]
    );
    assert!(targets(&idx, LinkKind::Mentions).is_empty());
    let scans = idx.store().unwrap().code_link_scans().unwrap();
    assert!(!scans.contains_key("docs/g.md"), "{scans:?}");
    assert!(scans.contains_key("docs/o.md"));
}

#[test]
fn a_store_no_build_scanned_is_scanned_by_the_next_index_without_a_content_change() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "docs/g.md", GLOSSARY);
    drop(index(dir.path()));

    // What a store from before code links looks like: documents, no scans.
    {
        let store = DocStore::open(&dir.path().join(".infigraph/docs.kuzu")).unwrap();
        let conn = store.connection().unwrap();
        conn.query("MATCH (l:CodeLink) DELETE l").unwrap();
        conn.query("MATCH (s:CodeLinkScan) DELETE s").unwrap();
    }
    let mut before = DocIndex::open(dir.path()).unwrap();
    before.init().unwrap();
    assert!(before
        .store()
        .unwrap()
        .code_link_scans()
        .unwrap()
        .is_empty());
    drop(before);

    let idx = index(dir.path()); // docs/g.md has not changed
    assert_eq!(targets(&idx, LinkKind::Documents).len(), 2);
    assert_eq!(
        idx.store()
            .unwrap()
            .code_link_scans()
            .unwrap()
            .get("docs/g.md"),
        Some(&CODE_LINKS_VERSION)
    );
}

#[test]
fn a_document_that_is_not_markdown_is_scanned_and_has_no_links() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "docs/n.txt",
        "### `src/a.py::f`\n\nnot markdown\n",
    );
    let idx = index(dir.path());

    assert!(targets(&idx, LinkKind::Documents).is_empty());
    assert!(idx
        .store()
        .unwrap()
        .code_link_scans()
        .unwrap()
        .contains_key("docs/n.txt"));
}

#[test]
fn a_document_found_by_following_a_link_outside_the_doc_root_is_scanned() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write(
        &repo,
        "docs/index.md",
        "# Index\n\nSee [notes](../NOTES.md).\n",
    );
    write(
        &repo,
        "NOTES.md",
        "# Notes\n\n### `src/a.py::f`\n\nWhy f.\n",
    );

    let idx = index(&repo.join("docs"));

    let found = targets(&idx, LinkKind::Documents);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].1, "src/a.py::f");
}
