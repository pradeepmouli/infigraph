//! Object literals under `as`, `satisfies` and parentheses, named by a real
//! SCIP import of the checked-in index (`fixtures/scip_ts_wrapped_literals`:
//! `a.ts` and `b.tsx`, indexed by scip-typescript).
//!
//! `export const X = {..} as const satisfies Record<..>` binds `X` to the
//! literal under two wrappers, and the literal is what its members belong to.
//! Before this, only one wrapper was recognised, so these members came out
//! with no owner (`a.ts::a-b`), and the document keys that name them did not
//! resolve.

use std::path::Path;
use std::sync::Arc;

use infigraph_core::extract::extract_file;
use infigraph_core::graph::{names, GraphStore};
use infigraph_core::scip::{import_scip_index, use_language_registry};

/// `(id, kind)` of every symbol of the fixture's files after the SCIP import.
fn imported_symbols() -> Vec<(String, String)> {
    use_language_registry(Arc::new(infigraph_languages::bundled_registry().unwrap()));
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scip_ts_wrapped_literals");
    let registry = infigraph_languages::bundled_registry().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = GraphStore::open(&dir.path().join(names::STEM)).unwrap();
    for file in ["a.ts", "b.tsx"] {
        let source = std::fs::read(fixture.join(file)).unwrap();
        std::fs::write(dir.path().join(file), &source).unwrap();
        let ext = Path::new(file).extension().unwrap().to_str().unwrap();
        let pack = registry.for_extension(&format!(".{ext}")).unwrap();
        let extracted = extract_file(file, &source, pack).unwrap();
        store.upsert_file(&extracted).unwrap();
    }

    import_scip_index(&fixture.join("scip.bin"), &store, Some(dir.path())).unwrap();

    let conn = store.connection().unwrap();
    conn.query("MATCH (s:Symbol) RETURN s.id, s.kind")
        .unwrap()
        .map(|row| (row[0].to_string(), row[1].to_string()))
        .collect()
}

fn assert_variables(symbols: &[(String, String)], ids: &[&str]) {
    for id in ids {
        let kind = symbols
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, k)| k.as_str());
        assert_eq!(
            kind,
            Some("Variable"),
            "{id} is missing or not a variable: {symbols:?}"
        );
    }
}

#[test]
fn a_literal_under_as_and_satisfies_owns_its_members() {
    let symbols = imported_symbols();
    // `{ .. } as const satisfies Record<..>`: two wrappers.
    assert_variables(&symbols, &["a.ts::X::a-b", "a.ts::X::c"]);
}

#[test]
fn a_nested_literal_under_wrappers_is_owned_by_the_key_above_it() {
    let symbols = imported_symbols();
    // `c: { d: 2 }` inside X: the nested literal is owned by `c`, which is owned by X.
    assert_variables(&symbols, &["a.ts::X::c::d"]);
}

#[test]
fn a_literal_under_satisfies_then_as_owns_its_members() {
    let symbols = imported_symbols();
    // `{ .. } as const satisfies Record<..> as const`: three wrappers.
    assert_variables(&symbols, &["a.ts::Y::e-f"]);
}

#[test]
fn a_nested_satisfies_under_a_wrapped_literal_nests_the_owners() {
    let symbols = imported_symbols();
    // `'g-h': { k: 5 } satisfies Record<..>` inside a wrapped W.
    assert_variables(&symbols, &["a.ts::W::g-h", "a.ts::W::g-h::k"]);
}

#[test]
fn parentheses_around_wrappers_do_not_hide_the_literal_in_tsx() {
    let symbols = imported_symbols();
    // `({ .. } satisfies Record<..>) as const`: parentheses, then two wrappers.
    assert_variables(&symbols, &["b.tsx::T::tsx-key", "b.tsx::U::tsx-two"]);
}

#[test]
fn no_wrapped_literal_member_keeps_its_quotes_or_scip_counter() {
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
