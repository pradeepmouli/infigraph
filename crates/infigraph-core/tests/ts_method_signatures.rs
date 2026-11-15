//! TypeScript method signatures (`fixtures/scip_ts_method_signatures`): an
//! interface's or type literal's `m(): T;` and a class's `abstract m(): T;` are
//! Method symbols named as scip-typescript and scip-tsrs name them, so the SCIP
//! import enriches one row each. A `method_signature` directly in a class body
//! is an overload signature above its implementation and stays folded into it,
//! so the implementation's id does not move (no ID_SCHEME bump).

use std::collections::BTreeSet;

mod common;
use common::ts_fixture::{base, TsFixture, SCIP_FILES};

fn fx() -> TsFixture {
    TsFixture::new("scip_ts_method_signatures")
}

fn ids(file: &str) -> BTreeSet<String> {
    fx().ids(file)
}

fn kind_of(file: &str, id: &str) -> String {
    fx().kind_of(file, id)
}

fn extracted(file: &str) -> Vec<(String, String)> {
    fx().extracted(file)
}

fn graph_ids(scip: Option<&str>) -> Vec<(String, String)> {
    fx().graph_rows("a.ts", scip)
}

#[test]
fn an_interface_signature_is_a_method_owned_by_the_interface() {
    let ids = ids("a.ts");
    for id in [
        "a.ts::ConcreteSeam::sightings",
        // a quoted name is named without its quotes, as both indexers name it
        "a.ts::ConcreteSeam::quoted-name",
        "a.ts::ConcreteSeam::optional",
        "a.ts::Lit::run",
        // a signature in a type literal nested in a property is owned through it
        "a.ts::Nested::inner::go",
    ] {
        assert!(ids.contains(id), "{id} missing from {ids:?}");
        assert_eq!(kind_of("a.ts", id), "Method", "{id}");
    }
}

/// A property beside a method keeps the id it had, and so does the property
/// after it: a signature adds ids, it moves none.
#[test]
fn a_signature_beside_a_property_renumbers_nothing() {
    let ids = ids("a.ts");
    for id in [
        "a.ts::ConcreteSeam::count",
        "a.ts::ConcreteSeam::tail",
        "a.ts::Lit::flag",
        "a.ts::Nested::inner",
    ] {
        assert!(ids.contains(id), "{id} missing from {ids:?}");
        assert_eq!(kind_of("a.ts", id), "Variable", "{id}");
    }
}

/// Existing overloads get `#N` on every member of the set, from 1 in source
/// order (`extract_entities`): the interface's two `over`s do too, and the two
/// `m`s of two declarations of one interface.
#[test]
fn overloaded_signatures_take_the_ordinals_overloads_take_elsewhere() {
    let ids = ids("a.ts");
    for id in [
        "a.ts::ConcreteSeam::over#1",
        "a.ts::ConcreteSeam::over#2",
        "a.ts::Merged::m#1",
        "a.ts::Merged::m#2",
    ] {
        assert!(ids.contains(id), "{id} missing from {ids:?}");
    }
    assert!(!ids.contains("a.ts::ConcreteSeam::over"));
    assert!(!ids.contains("a.ts::Merged::m"));
    assert!(ids.contains("a.ts::Merged::other"));
}

#[test]
fn an_abstract_method_and_its_implementation_have_different_owners() {
    let ids = ids("a.ts");
    for id in [
        "a.ts::Base::area",
        "a.ts::Base::name",
        "a.ts::Impl::area",
        "a.ts::Impl::name",
    ] {
        assert!(ids.contains(id), "{id} missing from {ids:?}");
        assert_eq!(kind_of("a.ts", id), "Method");
    }
    assert!(!ids.iter().any(|id| id.contains("area#")));
}

/// What nothing names is no symbol: call, construct and index signatures, and a
/// parameter's inline type.
#[test]
fn unnamed_signatures_and_parameter_types_are_no_symbols() {
    let ids = ids("a.ts");
    let leaks: Vec<&String> = ids
        .iter()
        .filter(|id| id.contains("::takes::") || id.contains("::new") || id.contains("::key"))
        .collect();
    assert!(leaks.is_empty(), "{leaks:?}");
    assert!(ids.contains("a.ts::takes"));
    // `takes(p: { inner(x): void })`: the `inner` that has a symbol is Nested's.
    let inners: Vec<&String> = ids.iter().filter(|id| id.ends_with("inner")).collect();
    assert_eq!(inners, ["a.ts::Nested::inner"], "{ids:?}");
    let members_of_seam = ids
        .iter()
        .filter(|id| id.starts_with("a.ts::ConcreteSeam::"))
        .count();
    // sightings, count, quoted-name, optional, over#1, over#2, tail
    assert_eq!(members_of_seam, 7, "{ids:?}");
}

/// The behaviour kept on purpose: an overload signature above a class method's
/// implementation is not a row, so the implementation keeps its bare id.
#[test]
fn a_class_overload_set_stays_one_symbol_with_its_bare_id() {
    let rows: Vec<(String, String)> = extracted("a.ts")
        .into_iter()
        .filter(|(id, _)| base(id) == "a.ts::Over::foo")
        .collect();
    assert_eq!(
        rows,
        [("a.ts::Over::foo".to_string(), "Method".to_string())]
    );
}

/// An abstract signature and a concrete method of one name in one class body is
/// not valid TypeScript, but parses; the ordinal pass names them, nothing
/// special-cases it.
#[test]
fn an_abstract_signature_and_a_concrete_method_of_one_name_get_ordinals() {
    let ids = ids("weird.ts");
    assert!(ids.contains("weird.ts::Weird::w#1"), "{ids:?}");
    assert!(ids.contains("weird.ts::Weird::w#2"), "{ids:?}");
    assert!(!ids.contains("weird.ts::Weird::w"));
}

#[test]
fn tsx_gives_its_signatures_too() {
    let ids = ids("b.tsx");
    assert!(ids.contains("b.tsx::Props::onPick"), "{ids:?}");
    assert_eq!(kind_of("b.tsx", "b.tsx::Props::onPick"), "Method");
    assert!(ids.contains("b.tsx::Props::label"));
}

/// Both indexers define every owned member the fixture has, in either grammar's
/// terms: each tree-sitter signature id is one of their definitions (by base id,
/// the indexers' repeats being the overloads' ordinals).
#[test]
fn both_indexers_define_every_signature_tree_sitter_names() {
    let signatures: BTreeSet<&str> = [
        "a.ts::ConcreteSeam::sightings",
        "a.ts::ConcreteSeam::quoted-name",
        "a.ts::ConcreteSeam::optional",
        "a.ts::ConcreteSeam::over",
        "a.ts::Merged::m",
        "a.ts::Merged::other",
        "a.ts::Lit::run",
        "a.ts::Nested::inner::go",
        "a.ts::Base::area",
        "a.ts::Base::name",
        "a.ts::Impl::area",
        "a.ts::Impl::name",
    ]
    .into_iter()
    .collect();
    let tree_sitter = ids("a.ts");
    for file in SCIP_FILES {
        let scip = fx().scip_ids(file, "a.ts");
        for id in &signatures {
            assert!(scip.contains(*id), "{file}: no SCIP definition of {id}");
        }
        // And the other way: every owned SCIP definition that is not a class
        // overload is a tree-sitter symbol (by base id).
        let tree_bases: BTreeSet<&str> = tree_sitter.iter().map(|i| base(i)).collect();
        let missing: Vec<&String> = scip
            .iter()
            .filter(|id| id.as_str() != "a.ts::a.ts")
            .filter(|id| !tree_bases.contains(id.as_str()))
            .collect();
        assert!(missing.is_empty(), "{file}: SCIP-only ids {missing:?}");
    }
}

/// Enrichment leaves exactly one row per signature, under the id tree-sitter
/// gave it, and no row tree-sitter had changes or goes away.
#[test]
fn an_import_enriches_each_signature_and_adds_no_second_row() {
    let before = graph_ids(None);
    for file in SCIP_FILES {
        let after = graph_ids(Some(file));
        for (id, kind) in &before {
            assert!(
                after.contains(&(id.clone(), kind.clone())),
                "{file}: {id} ({kind}) changed or went away on import"
            );
        }
        let count =
            |after: &[(String, String)], id: &str| after.iter().filter(|(i, _)| i == id).count();
        for id in [
            "a.ts::ConcreteSeam::sightings",
            "a.ts::ConcreteSeam::quoted-name",
            "a.ts::ConcreteSeam::optional",
            "a.ts::ConcreteSeam::over#1",
            "a.ts::ConcreteSeam::over#2",
            "a.ts::Merged::m#1",
            "a.ts::Merged::m#2",
            "a.ts::Lit::run",
            "a.ts::Nested::inner::go",
            "a.ts::Base::area",
            "a.ts::Impl::area",
        ] {
            assert_eq!(count(&after, id), 1, "{file}: {id}");
        }
        // No third `over` appeared next to the two the interface has.
        assert!(
            !after
                .iter()
                .any(|(i, _)| i.starts_with("a.ts::ConcreteSeam::over#3")),
            "{file}: {after:?}"
        );
    }
}

/// Pinned, not accidental: SCIP defines `Over::foo` three times and the import
/// adds the rows tree-sitter does not have (the ordinals past the bare id),
/// exactly as it did before method signatures were symbols; the implementation
/// keeps its bare id, one row.
#[test]
fn a_class_overload_set_keeps_its_scip_only_rows_after_import() {
    for file in SCIP_FILES {
        let after = graph_ids(Some(file));
        let foo: Vec<&str> = after
            .iter()
            .map(|(i, _)| i.as_str())
            .filter(|i| base(i) == "a.ts::Over::foo")
            .collect();
        assert_eq!(foo.iter().filter(|i| **i == "a.ts::Over::foo").count(), 1);
        println!("{file}: {foo:?}");
        assert_eq!(foo, EXPECTED_OVER_FOO, "{file}");
    }
}

const EXPECTED_OVER_FOO: &[&str] = &["a.ts::Over::foo"];
