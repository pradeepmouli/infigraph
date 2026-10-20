//! An indexer reads a file at one moment and its output is imported at
//! another. A tree-sitter reindex between the two removed a symbol the file no
//! longer defines; importing the old output must not put it back.

use std::sync::Arc;

use infigraph_core::graph::GraphStore;
use infigraph_core::scip::{import_scip_index, use_language_registry, write_snapshot};
use protobuf::Message;
use scip::types::{Document, Index, Occurrence, SymbolInformation, SymbolRole};

const FILE: &str = "src/a.ts";

fn moniker(descriptor: &str) -> String {
    format!("scip-typescript npm test 1.0.0 src/`a.ts`/{descriptor}")
}

/// Names of the symbols in `FILE` after importing an index that was made when
/// the file still defined `gone()` (line 0) and `kept()` (line 2), into a
/// graph whose tree-sitter pass has since seen the file as only `kept()`.
fn names_after_importing_an_index_older_than_the_file() -> Vec<String> {
    use_language_registry(Arc::new(infigraph_languages::bundled_registry().unwrap()));
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    // The file as the indexer read it.
    std::fs::write(
        dir.path().join(FILE),
        "export function gone() {}\n\nexport function kept() {}\n",
    )
    .unwrap();
    let store = GraphStore::open(&dir.path().join(infigraph_core::graph::names::STEM)).unwrap();
    let conn = store.connection().unwrap();
    conn.query(&format!(
        "CREATE (:File {{id: '{FILE}', name: '{FILE}', path: '{FILE}', \
         language: 'typescript', symbol_count: 1}})"
    ))
    .unwrap();
    conn.query(&format!(
        "CREATE (:Symbol {{id: '{FILE}::kept', name: 'kept', kind: 'function', file: '{FILE}', \
         start_line: 0, end_line: 0, signature_hash: '', language: 'typescript', \
         visibility: 'public', parent: '', docstring: '', complexity: 0, parameters: '', \
         return_type: ''}})"
    ))
    .unwrap();

    let def = |descriptor: &str, line: i32| Occurrence {
        range: vec![line, 16, line, 20],
        symbol: moniker(descriptor),
        symbol_roles: SymbolRole::Definition as i32,
        ..Default::default()
    };
    let doc = Document {
        relative_path: FILE.to_string(),
        occurrences: vec![def("gone().", 0), def("kept().", 2)],
        symbols: ["gone().", "kept()."]
            .iter()
            .map(|d| SymbolInformation {
                symbol: moniker(d),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let index = Index {
        documents: vec![doc],
        ..Default::default()
    };
    let path = dir.path().join("index.scip");
    std::fs::write(&path, index.write_to_bytes().unwrap()).unwrap();
    // The snapshot is taken when the indexer starts; then `gone()` is deleted.
    write_snapshot(dir.path(), &path).unwrap();
    std::fs::write(dir.path().join(FILE), "export function kept() {}\n").unwrap();
    import_scip_index(&path, &store, Some(dir.path())).unwrap();

    let mut names: Vec<String> = conn
        .query("MATCH (s:Symbol) RETURN s.name ORDER BY s.name")
        .unwrap()
        .map(|r| r[0].to_string().trim_matches('"').to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn an_index_older_than_the_file_does_not_bring_back_a_symbol_the_file_dropped() {
    assert_eq!(
        names_after_importing_an_index_older_than_the_file(),
        vec!["kept"],
        "`gone` was deleted from the file after the indexer read it"
    );
}
