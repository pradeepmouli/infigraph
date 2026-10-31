//! Object-literal members with quoted keys, named by a real SCIP import.
//!
//! scip-typescript gives a quoted key a moniker descriptor of the form
//! `'key'N` (or `"key"N`), with no owner; a bare key is `keyN`. The import
//! names a member from the source instead (`extract::literal_members`), so a
//! quoted key of a literal bound to a name is `file::Const::key`, and a literal
//! nothing binds is `file::key`. The fixture's index is checked in: regenerate
//! it with `scip-typescript index` in a copy of `fixtures/scip_ts_literal_keys`.

use std::path::Path;
use std::sync::Arc;

use infigraph_core::extract::extract_file;
use infigraph_core::graph::{names, GraphStore};
use infigraph_core::scip::{import_scip_index, use_language_registry};

/// `(id, kind)` of every symbol of `a.ts` after the SCIP import.
fn imported_symbols() -> Vec<(String, String)> {
    use_language_registry(Arc::new(infigraph_languages::bundled_registry().unwrap()));
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scip_ts_literal_keys");
    let source = std::fs::read(fixture.join("a.ts")).unwrap();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.ts"), &source).unwrap();
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_extension(".ts").unwrap();
    let extracted = extract_file("a.ts", &source, pack).unwrap();
    let store = GraphStore::open(&dir.path().join(names::STEM)).unwrap();
    store.upsert_file(&extracted).unwrap();

    import_scip_index(&fixture.join("scip.bin"), &store, Some(dir.path())).unwrap();

    let conn = store.connection().unwrap();
    conn.query("MATCH (s:Symbol) WHERE s.file = 'a.ts' RETURN s.id, s.kind")
        .unwrap()
        .map(|row| (row[0].to_string(), row[1].to_string()))
        .collect()
}

fn kind_of(symbols: &[(String, String)], id: &str) -> Option<String> {
    symbols
        .iter()
        .find(|(i, _)| i == id)
        .map(|(_, kind)| kind.clone())
}

#[test]
fn a_quoted_key_of_a_named_literal_is_owned_by_it_and_named_by_its_key() {
    let symbols = imported_symbols();
    for id in [
        "a.ts::ENRICH_SHAPES::unfielded-symbol-slot",
        "a.ts::ENRICH_SHAPES::it\\'s",
        "a.ts::ENRICH_SHAPES::sp ace",
        "a.ts::ENRICH_SHAPES::dq-key",
        "a.ts::ENRICH_SHAPES::bare",
    ] {
        assert_eq!(
            kind_of(&symbols, id).as_deref(),
            Some("Variable"),
            "{id} is missing or not a variable: {symbols:?}"
        );
    }
}

/// A literal nothing binds has no owner, and its quoted key is named by the key.
#[test]
fn a_quoted_key_of_a_literal_nothing_names_has_no_owner_and_no_quotes() {
    let symbols = imported_symbols();
    for id in ["a.ts::inner-key", "a.ts::z"] {
        assert_eq!(
            kind_of(&symbols, id).as_deref(),
            Some("Variable"),
            "{id} is missing: {symbols:?}"
        );
    }
}

#[test]
fn no_literal_member_id_keeps_its_quotes_or_scip_counter() {
    let symbols = imported_symbols();
    let bad: Vec<&String> = symbols
        .iter()
        .map(|(id, _)| id)
        .filter(|id| {
            let last = id.rsplit("::").next().unwrap_or("");
            last.starts_with(['\'', '"']) || last.ends_with(['\'', '"'])
        })
        .collect();
    assert!(bad.is_empty(), "quoted or counted names: {bad:?}");
}
