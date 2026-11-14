//! TypeScript namespaces, tree-sitter against a SCIP import (`fixtures/scip_ts_namespaces`).
//!
//! SCIP already gives these namespace symbols, so an enriched graph had them and
//! an unenriched one did not. Tree-sitter gives them now, so the import has to
//! enrich the one symbol it finds, not add a second, and the kind must agree on
//! both sides. `scip.bin` is scip-typescript's output, `scip-tsrs.bin` is
//! scip-tsrs's output for the same file; both are checked.

use std::path::Path;
use std::sync::Arc;

use infigraph_core::extract::extract_file;
use infigraph_core::graph::{names, GraphStore};
use infigraph_core::scip::{definition_ids, import_scip_index, use_language_registry};

const FIXTURE: &str = "tests/fixtures/scip_ts_namespaces";

/// What the SCIP import gives a symbol that tree-sitter does not, and the reason:
/// the file-level definition, and `declare module 'external-lib'` (out of scope,
/// see the ruling in the branch's design). The implicit `Dotted` of
/// `namespace Dotted.Inner` is tree-sitter's now.
const SCIP_ONLY: &[&str] = &[
    "a.ts::a.ts",
    "a.ts::external-lib",
    "a.ts::external-lib::shape",
];

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

/// Tree-sitter's `(id, kind)` for `a.ts`.
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

/// The graph's `(id, kind)` rows for `a.ts`: tree-sitter's symbols, and after
/// importing `scip` (a file of the fixture) when one is given.
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

/// Every namespace and merged declaration the fixture has, with its kind.
const NAMESPACE_KINDS: &[(&str, &str)] = &[
    ("a.ts::Clause", "Module"),
    ("a.ts::Declaration", "Module"),
    ("a.ts::Declaration::Interface", "Module"),
    ("a.ts::Element", "Module"),
    ("a.ts::Ambient", "Module"),
    ("a.ts::Dotted", "Module"),
    ("a.ts::Dotted::Inner", "Module"),
    ("a.ts::Repeated", "Module"),
    ("a.ts::Box", "Class"),
    ("a.ts::Opts", "Class"),
];

#[test]
fn every_tree_sitter_id_is_a_scip_id_by_both_indexers() {
    let tree_sitter: std::collections::BTreeSet<String> =
        extracted().into_iter().map(|(id, _)| id).collect();
    for file in ["scip.bin", "scip-tsrs.bin"] {
        let scip = ids_of(file);
        // `a.ts::shape` is `export const shape` inside `declare module
        // 'external-lib'`: tree-sitter files it at the top level, a pre-existing
        // mismatch this change does not touch (declare module is out of scope).
        let missing: Vec<&String> = tree_sitter
            .iter()
            .filter(|id| !scip.contains(*id) && id.as_str() != "a.ts::shape")
            .collect();
        assert!(
            missing.is_empty(),
            "{file}: tree-sitter ids SCIP lacks: {missing:?}"
        );
    }
}

#[test]
fn the_ids_scip_has_and_tree_sitter_does_not_are_the_known_gaps() {
    let tree_sitter: std::collections::BTreeSet<String> =
        extracted().into_iter().map(|(id, _)| id).collect();
    for file in ["scip.bin", "scip-tsrs.bin"] {
        let only_scip: Vec<String> = ids_of(file)
            .into_iter()
            .filter(|id| !tree_sitter.contains(id))
            .collect();
        let mut expected: Vec<String> = SCIP_ONLY.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(only_scip, expected, "{file}");
    }
}

#[test]
fn an_unenriched_graph_holds_every_namespace_with_its_kind() {
    let rows = graph_rows(None);
    for (id, kind) in NAMESPACE_KINDS {
        let found: Vec<&String> = rows
            .iter()
            .filter(|(i, _)| i == id)
            .map(|(_, k)| k)
            .collect();
        assert_eq!(found, [&kind.to_string()], "{id}: {rows:?}");
    }
}

#[test]
fn an_import_enriches_each_namespace_symbol_and_adds_no_second() {
    let before = graph_rows(None);
    for file in ["scip.bin", "scip-tsrs.bin"] {
        let after = graph_rows(Some(file));
        for (id, kind) in NAMESPACE_KINDS {
            let rows: Vec<&String> = after
                .iter()
                .filter(|(i, _)| i == id)
                .map(|(_, k)| k)
                .collect();
            assert_eq!(
                rows,
                [&kind.to_string()],
                "{file}: {id} must be one symbol of kind {kind} after the import"
            );
        }
        assert!(
            after.iter().all(|(id, _)| !id.contains('#')),
            "{file}: an ordinal appeared: {after:?}"
        );
        // Enrichment moves no kind, so every row before is still there as before.
        for row in &before {
            assert!(
                after.contains(row),
                "{file}: {row:?} changed or went away on import"
            );
        }
    }
}

/// What the import adds to the graph of this file. `external-lib::shape` is not
/// among them: the import adds no symbol for a member of an ambient module (the
/// same out-of-scope declare module case, observed, not designed).
const GRAPH_ADDED: &[&str] = &["a.ts::a.ts", "a.ts::external-lib"];

#[test]
fn an_import_adds_only_the_known_gaps_to_the_graph() {
    let before: std::collections::BTreeSet<(String, String)> =
        graph_rows(None).into_iter().collect();
    let after: std::collections::BTreeSet<(String, String)> =
        graph_rows(Some("scip.bin")).into_iter().collect();
    let mut added: Vec<String> = after
        .difference(&before)
        .map(|(id, _)| id.clone())
        .collect();
    added.sort();
    let mut expected: Vec<String> = GRAPH_ADDED.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(added, expected);
}
