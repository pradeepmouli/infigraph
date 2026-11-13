//! A function that uses a type or a constant depends on it. The import has to
//! keep that dependency whatever SCIP kind the target carries, and the
//! dependency queries have to follow it: `find_all_references` and
//! `transitive_impact` must both return the function. rust-analyzer sets
//! `SymbolInformation.kind` (struct, constant, function), so this is not only
//! a TypeScript producer's problem.

use std::sync::Arc;

use infigraph_core::graph::{GraphBackend, GraphStore, KuzuBackend};
use infigraph_core::scip::{import_scip_index, use_language_registry};
use protobuf::Message;
use scip::types::symbol_information::Kind;
use scip::types::{Document, Index, Occurrence, SymbolInformation, SymbolRole};

const FILE: &str = "src/lib.rs";
// `make` is on one line: the importer attributes a reference to the symbol
// whose span contains it, so its body must share the definition's line.
const SOURCE: &str = "\
pub const LIMIT: usize = 3;
pub struct Point { pub x: i32 }
pub fn make() -> Point { Point { x: LIMIT as i32 } }
";

const POINT: &str = "src/lib.rs::Point";
const LIMIT_ID: &str = "src/lib.rs::LIMIT";
const MAKE: &str = "src/lib.rs::make";

fn moniker(descriptor: &str) -> String {
    format!("rust-analyzer cargo rafix 0.1.0 src/`lib.rs`/{descriptor}")
}

/// Import a rust-analyzer-shaped index over `SOURCE`: definitions carry their
/// kind, and `make` references `Point` (its return type and its struct
/// literal) and `LIMIT` as plain reads, with no access role, as rust-analyzer
/// emits them for a type and a constant.
fn import_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    use_language_registry(Arc::new(infigraph_languages::bundled_registry().unwrap()));
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join(FILE), SOURCE).unwrap();
    let graph = dir.path().join(infigraph_core::graph::names::STEM);
    let store = GraphStore::open(&graph).unwrap();
    store
        .connection()
        .unwrap()
        .query(&format!(
            "CREATE (:File {{id: '{FILE}', name: '{FILE}', path: '{FILE}', \
             language: 'rust', symbol_count: 0}})"
        ))
        .unwrap();

    // (moniker, kind, 0-based line, 0-based column)
    let defs = [
        (moniker("LIMIT."), Kind::Constant, 0, 10),
        (moniker("Point#"), Kind::Struct, 1, 11),
        (moniker("make()."), Kind::Function, 2, 7),
    ];
    // (moniker, 0-based line, 0-based column)
    let refs = [
        (moniker("Point#"), 2, 17),
        (moniker("Point#"), 2, 25),
        (moniker("LIMIT."), 2, 36),
    ];
    let index = Index {
        documents: vec![Document {
            relative_path: FILE.to_string(),
            occurrences: defs
                .iter()
                .map(|(sym, _, line, col)| Occurrence {
                    range: vec![*line, *col, *line, *col + 3],
                    symbol: sym.clone(),
                    symbol_roles: SymbolRole::Definition as i32,
                    ..Default::default()
                })
                .chain(refs.iter().map(|(sym, line, col)| Occurrence {
                    range: vec![*line, *col, *line, *col + 3],
                    symbol: sym.clone(),
                    symbol_roles: 0,
                    ..Default::default()
                }))
                .collect(),
            symbols: defs
                .iter()
                .map(|(sym, kind, ..)| SymbolInformation {
                    symbol: sym.clone(),
                    kind: (*kind).into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let path = dir.path().join("index.scip");
    std::fs::write(&path, index.write_to_bytes().unwrap()).unwrap();
    import_scip_index(&path, &store, Some(dir.path())).unwrap();
    drop(store);
    (dir, graph)
}

fn callers_of(backend: &KuzuBackend, id: &str) -> Vec<String> {
    backend
        .find_all_references(id)
        .unwrap()
        .into_iter()
        .map(|r| r.caller_id)
        .collect()
}

fn affected_by(backend: &KuzuBackend, id: &str) -> Vec<String> {
    backend
        .transitive_impact(id, 1)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect()
}

#[test]
fn a_function_that_uses_a_type_is_a_reference_of_it() {
    let (_dir, graph) = import_fixture();
    let callers = callers_of(&KuzuBackend::open(&graph).unwrap(), POINT);
    assert!(
        callers.iter().any(|c| c == MAKE),
        "make uses Point, so it must be a reference of Point: {callers:?}"
    );
}

#[test]
fn a_function_that_uses_a_constant_is_a_reference_of_it() {
    let (_dir, graph) = import_fixture();
    let callers = callers_of(&KuzuBackend::open(&graph).unwrap(), LIMIT_ID);
    assert!(
        callers.iter().any(|c| c == MAKE),
        "make uses LIMIT, so it must be a reference of LIMIT: {callers:?}"
    );
}

#[test]
fn changing_a_type_affects_the_function_that_uses_it() {
    let (_dir, graph) = import_fixture();
    let affected = affected_by(&KuzuBackend::open(&graph).unwrap(), POINT);
    assert!(
        affected.iter().any(|a| a == MAKE),
        "changing Point affects make: {affected:?}"
    );
}

#[test]
fn changing_a_constant_affects_the_function_that_uses_it() {
    let (_dir, graph) = import_fixture();
    let affected = affected_by(&KuzuBackend::open(&graph).unwrap(), LIMIT_ID);
    assert!(
        affected.iter().any(|a| a == MAKE),
        "changing LIMIT affects make: {affected:?}"
    );
}
