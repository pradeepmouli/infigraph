//! A class body's method signatures (`fixtures/scip_ts_declare_class`). One that
//! has an implementation of its name in the body is an overload and stays folded
//! into it (the implementation's id does not move); one with none -- every
//! method of a `declare class` or of a class in a `.d.ts` -- has nothing to fold
//! into and is a Method symbol like an interface's member, which both indexers
//! define. Overloads of such a method take the ordinals.

mod common;
use common::ts_fixture::{base, TsFixture, SCIP_FILES};

fn fx() -> TsFixture {
    TsFixture::new("scip_ts_declare_class")
}

const SIGNATURES: &[&str] = &[
    "a.ts::X::foo",
    "a.ts::X::bar#1",
    "a.ts::X::bar#2",
    "a.ts::X::make",
    "a.ts::X::quoted-name",
    "a.ts::Y::area",
    "a.ts::Y::describe",
    "a.ts::Mixed::only",
];

#[test]
fn a_method_of_a_declare_class_is_a_symbol() {
    let ids = fx().ids("a.ts");
    for id in SIGNATURES {
        assert!(ids.contains(*id), "{id} missing from {ids:?}");
        assert_eq!(fx().kind_of("a.ts", id), "Method", "{id}");
    }
    assert!(!ids.contains("a.ts::X::bar"), "overloads take ordinals");
}

#[test]
fn a_method_of_a_class_in_a_d_ts_file_is_a_symbol() {
    let ids = fx().ids("lib.d.ts");
    for id in [
        "lib.d.ts::Z::run#1",
        "lib.d.ts::Z::run#2",
        "lib.d.ts::Z::stop",
    ] {
        assert!(ids.contains(id), "{id} missing from {ids:?}");
    }
}

/// The rule's other half, unchanged: overloads above an implementation stay
/// folded, the implementation keeps its bare id.
#[test]
fn overloads_above_an_implementation_stay_folded() {
    let rows: Vec<String> = fx()
        .ids("a.ts")
        .into_iter()
        .filter(|id| base(id) == "a.ts::Fold::fold" || base(id) == "a.ts::A1::go")
        .collect();
    assert_eq!(rows, ["a.ts::A1::go", "a.ts::Fold::fold"]);
}

#[test]
fn both_indexers_define_every_symbol_tree_sitter_has_in_a_class_body() {
    let tree_sitter = fx().ids("a.ts");
    let bases: std::collections::BTreeSet<&str> = tree_sitter.iter().map(|i| base(i)).collect();
    for file in SCIP_FILES {
        let scip = fx().scip_ids(file, "a.ts");
        for id in &tree_sitter {
            assert!(
                scip.contains(base(id)),
                "{file}: no SCIP definition of {id}"
            );
        }
        let only_scip: Vec<&String> = scip
            .iter()
            .filter(|id| id.as_str() != "a.ts::a.ts" && !bases.contains(id.as_str()))
            .collect();
        assert!(only_scip.is_empty(), "{file}: SCIP-only {only_scip:?}");
    }
}

#[test]
fn an_import_enriches_each_signature_and_adds_no_second_row() {
    let before = fx().graph_rows("a.ts", None);
    for file in SCIP_FILES {
        let after = fx().graph_rows("a.ts", Some(file));
        for row in &before {
            assert!(after.contains(row), "{file}: {row:?} changed on import");
        }
        for id in SIGNATURES {
            let n = after.iter().filter(|(i, _)| i == id).count();
            assert_eq!(n, 1, "{file}: {id}");
        }
        let rows_of = |name: &str| -> Vec<&str> {
            after
                .iter()
                .map(|(i, _)| i.as_str())
                .filter(|i| base(i) == name)
                .collect()
        };
        // A class overload set whose name nothing else in the file shares: SCIP's
        // three definitions fold onto the one row.
        assert_eq!(rows_of("a.ts::Fold::fold"), ["a.ts::Fold::fold"], "{file}");
        // The same set under a name another class shares: the import has no lone
        // candidate to fold onto, so SCIP's other two definitions are rows of
        // their own, as they are with no method signature symbols at all (checked
        // by running this test with every class-body signature skipped).
        assert_eq!(
            rows_of("a.ts::A1::go"),
            ["a.ts::A1::go", "a.ts::A1::go#2", "a.ts::A1::go#3"],
            "{file}"
        );
    }
}
