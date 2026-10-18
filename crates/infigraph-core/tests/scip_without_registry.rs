//! A SCIP import in a process that never installed a language registry
//! (`scip::use_language_registry`) cannot ask who owns an object-literal
//! member. That must be reported, not silent: the members keep the indexer's
//! counter-suffixed names. This binary never installs one.

use infigraph_core::degraded::{self, SCIP_LITERAL_OWNERS_UNAVAILABLE};
use infigraph_core::graph::GraphStore;
use infigraph_core::scip::import_scip_index;
use protobuf::Message;
use scip::types::{Document, Index, Occurrence, SymbolInformation, SymbolRole};

const FILE: &str = "src/config.ts";

#[test]
fn literal_members_met_without_a_registry_are_reported_as_degraded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join(FILE),
        "export const CONFIG = {\n  semi: true,\n};\n",
    )
    .unwrap();
    let store = GraphStore::open(&dir.path().join("graph")).unwrap();
    store
        .connection()
        .unwrap()
        .query(&format!(
            "CREATE (:File {{id: '{FILE}', name: '{FILE}', path: '{FILE}', \
             language: 'typescript', symbol_count: 0}})"
        ))
        .unwrap();
    let sym = "scip-typescript npm test 1.0.0 src/`config.ts`/semi0:".to_string();
    let doc = Document {
        relative_path: FILE.to_string(),
        occurrences: vec![Occurrence {
            range: vec![1, 2, 1, 6],
            symbol: sym.clone(),
            symbol_roles: SymbolRole::Definition as i32,
            ..Default::default()
        }],
        symbols: vec![SymbolInformation {
            symbol: sym,
            ..Default::default()
        }],
        ..Default::default()
    };
    let path = dir.path().join("index.scip");
    std::fs::write(
        &path,
        Index {
            documents: vec![doc],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap(),
    )
    .unwrap();

    import_scip_index(&path, &store, Some(dir.path())).unwrap();

    let keys: Vec<_> = degraded::live::for_root(dir.path())
        .into_iter()
        .map(|n| n.key)
        .collect();
    assert!(
        keys.iter().any(|k| k == SCIP_LITERAL_OWNERS_UNAVAILABLE),
        "{keys:?}"
    );
}
