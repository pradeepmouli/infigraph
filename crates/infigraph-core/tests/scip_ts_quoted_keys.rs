//! Quoted property names in TypeScript (`fixtures/scip_ts_quoted_keys`):
//! `readonly 'c-strings': true` in an interface, a quoted key of a nested type
//! literal (which owns its own members), and a quoted class field. Tree-sitter
//! gives each one the name without its quotes, which is the name SCIP gives it,
//! so an import enriches it rather than adding a second.

use std::path::Path;
use std::sync::Arc;

use infigraph_core::extract::extract_file;
use infigraph_core::graph::{names, GraphStore};
use infigraph_core::scip::{definition_ids, import_scip_index, use_language_registry};

const FIXTURE: &str = "tests/fixtures/scip_ts_quoted_keys";

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

fn extracted() -> Vec<(String, String)> {
    let source = std::fs::read(fixture().join("a.ts")).unwrap();
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_extension(".ts").unwrap();
    extract_file("a.ts", &source, pack)
        .unwrap()
        .symbols
        .into_iter()
        .map(|s| (s.id, format!("{:?}", s.kind)))
        .collect()
}

fn graph_rows(scip: Option<&str>) -> Vec<(String, String)> {
    use_language_registry(Arc::new(infigraph_languages::bundled_registry().unwrap()));
    let source = std::fs::read(fixture().join("a.ts")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.ts"), &source).unwrap();
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_extension(".ts").unwrap();
    let extracted = extract_file("a.ts", &source, pack).unwrap();
    let store = GraphStore::open(&dir.path().join(names::STEM)).unwrap();
    store.upsert_file(&extracted).unwrap();
    if let Some(file) = scip {
        import_scip_index(&fixture().join(file), &store, Some(dir.path())).unwrap();
    }
    let conn = store.connection().unwrap();
    conn.query("MATCH (s:Symbol) WHERE s.file = 'a.ts' RETURN s.id, s.kind")
        .unwrap()
        .map(|row| (row[0].to_string(), row[1].to_string()))
        .collect()
}

fn ids_of(file: &str) -> std::collections::BTreeSet<String> {
    let bytes = std::fs::read(fixture().join(file)).unwrap();
    definition_ids(&bytes, "a.ts")
        .unwrap()
        .into_iter()
        .collect()
}

const QUOTED: &[&str] = &[
    "a.ts::Flags::c-strings",
    "a.ts::Cfg::a-b",
    "a.ts::Cfg::a-b::c-d",
    "a.ts::Settings::x-y",
];

#[test]
fn a_quoted_key_is_a_variable_named_without_its_quotes() {
    let symbols = extracted();
    for id in QUOTED {
        let found: Vec<&String> = symbols
            .iter()
            .filter(|(i, _)| i == id)
            .map(|(_, k)| k)
            .collect();
        assert_eq!(found, ["Variable"], "{id}: {symbols:?}");
    }
    assert!(
        symbols.iter().all(|(id, _)| !id.contains('\'')),
        "a quote survived in an id: {symbols:?}"
    );
}

#[test]
fn a_quoted_key_owns_the_members_of_its_type_literal() {
    // `'a-b': { 'c-d': string }`: `c-d` is `a-b`'s member, under the name `a-b`.
    let symbols = extracted();
    assert!(
        symbols.iter().any(|(id, _)| id == "a.ts::Cfg::a-b::c-d"),
        "{symbols:?}"
    );
}

#[test]
fn tree_sitter_and_both_indexers_agree_on_the_quoted_ids() {
    let tree_sitter: std::collections::BTreeSet<String> =
        extracted().into_iter().map(|(id, _)| id).collect();
    for file in ["scip.bin", "scip-tsrs.bin"] {
        let scip = ids_of(file);
        for id in QUOTED {
            assert!(scip.contains(*id), "{file} lacks {id}: {scip:?}");
            assert!(tree_sitter.contains(*id), "tree-sitter lacks {id}");
        }
        // Every SCIP id is a tree-sitter id, but the file-level definition.
        let only_scip: Vec<&String> = scip
            .iter()
            .filter(|id| !tree_sitter.contains(*id) && id.as_str() != "a.ts::a.ts")
            .collect();
        assert!(only_scip.is_empty(), "{file}: SCIP-only ids {only_scip:?}");
    }
}

#[test]
fn an_import_enriches_each_quoted_key_into_one_symbol() {
    let before = graph_rows(None);
    for file in ["scip.bin", "scip-tsrs.bin"] {
        let after = graph_rows(Some(file));
        for id in QUOTED {
            let rows: Vec<&String> = after
                .iter()
                .filter(|(i, _)| i == id)
                .map(|(_, k)| k)
                .collect();
            assert_eq!(rows, ["Variable"], "{file}: {id} after the import");
        }
        assert!(
            after.iter().all(|(id, _)| !id.contains('#')),
            "{file}: an ordinal appeared: {after:?}"
        );
        for row in &before {
            assert!(after.contains(row), "{file}: {row:?} changed on import");
        }
    }
}
