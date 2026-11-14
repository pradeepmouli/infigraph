//! TypeScript namespaces as tree-sitter symbols (`fixtures/scip_ts_namespaces`).
//! Before this, a namespace was no symbol of its own, only its members were, so
//! `Declaration::Interface` in sittir existed only as a SCIP import.

use infigraph_core::extract::extract_file;

fn extracted() -> Vec<(String, String)> {
    let source = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/scip_ts_namespaces/a.ts"),
    )
    .unwrap();
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_extension(".ts").unwrap();
    extract_file("a.ts", &source, pack)
        .unwrap()
        .symbols
        .into_iter()
        .map(|s| (s.id, format!("{:?}", s.kind)))
        .collect()
}

fn kinds_of(symbols: &[(String, String)], id: &str) -> Vec<String> {
    symbols
        .iter()
        .filter(|(i, _)| i == id)
        .map(|(_, kind)| kind.clone())
        .collect()
}

#[test]
fn a_namespace_is_a_module_symbol_named_by_its_path() {
    let symbols = extracted();
    for id in [
        "a.ts::Clause",
        "a.ts::Declaration",
        "a.ts::Declaration::Interface",
        "a.ts::Element",
        "a.ts::Ambient",
        "a.ts::Dotted::Inner",
    ] {
        assert_eq!(kinds_of(&symbols, id), ["Module"], "{id}: {symbols:?}");
    }
}

#[test]
fn a_member_of_a_namespace_is_still_under_it() {
    let symbols = extracted();
    assert_eq!(
        kinds_of(&symbols, "a.ts::Declaration::Interface::Props"),
        ["Class"],
        "{symbols:?}"
    );
    assert_eq!(
        kinds_of(&symbols, "a.ts::Element::tag"),
        ["Variable"],
        "{symbols:?}"
    );
}

#[test]
fn a_namespace_merged_with_a_class_or_an_interface_is_that_declaration() {
    let symbols = extracted();
    // One symbol, the class's kind; its namespace's member lives under it.
    assert_eq!(kinds_of(&symbols, "a.ts::Box"), ["Class"], "{symbols:?}");
    assert_eq!(kinds_of(&symbols, "a.ts::Box::depth"), ["Variable"]);
    assert_eq!(kinds_of(&symbols, "a.ts::Opts"), ["Class"], "{symbols:?}");
    assert_eq!(kinds_of(&symbols, "a.ts::Opts::Mode"), ["Class"]);
    // Nothing in the file gets an ordinal: the merge must not renumber a declaration.
    assert!(
        symbols.iter().all(|(id, _)| !id.contains('#')),
        "{symbols:?}"
    );
}

#[test]
fn several_blocks_of_one_namespace_are_one_symbol_at_the_first_block() {
    let symbols = extracted();
    assert_eq!(
        kinds_of(&symbols, "a.ts::Repeated"),
        ["Module"],
        "{symbols:?}"
    );
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/scip_ts_namespaces/a.ts"),
    )
    .unwrap();
    let first_block = source
        .lines()
        .position(|line| line.starts_with("export namespace Repeated"))
        .unwrap()
        + 1;
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_extension(".ts").unwrap();
    let extracted = extract_file("a.ts", source.as_bytes(), pack).unwrap();
    let repeated: Vec<u32> = extracted
        .symbols
        .iter()
        .filter(|s| s.id == "a.ts::Repeated")
        .map(|s| s.span.start_line)
        .collect();
    assert_eq!(repeated, [first_block as u32], "{repeated:?}");
}

#[test]
fn a_dotted_namespace_declares_each_segment_as_a_namespace() {
    // `namespace Dotted.Inner` declares `Dotted` too; both are Module symbols.
    let symbols = extracted();
    assert_eq!(
        kinds_of(&symbols, "a.ts::Dotted"),
        ["Module"],
        "{symbols:?}"
    );
    assert_eq!(
        kinds_of(&symbols, "a.ts::Dotted::Inner"),
        ["Module"],
        "{symbols:?}"
    );
}

#[test]
fn dotted_segments_merge_across_blocks_like_any_namespace() {
    // Two blocks that share the prefix `A`: one `A`, one `B`, one `C`.
    let source = b"namespace A.B { export const x = 1; }\nnamespace A.C { export const y = 2; }\n";
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_extension(".ts").unwrap();
    let symbols: Vec<(String, String)> = extract_file("a.ts", source, pack)
        .unwrap()
        .symbols
        .into_iter()
        .map(|s| (s.id, format!("{:?}", s.kind)))
        .collect();
    for id in ["a.ts::A", "a.ts::A::B", "a.ts::A::C"] {
        assert_eq!(kinds_of(&symbols, id), ["Module"], "{id}: {symbols:?}");
    }
}

#[test]
fn a_declare_module_named_by_a_string_is_no_namespace() {
    // `declare module 'external-lib'` names a package, not a namespace of this
    // file: out of scope, so it gets no symbol of its own.
    let symbols = extracted();
    assert!(
        symbols
            .iter()
            .all(|(id, _)| !id.starts_with("a.ts::external-lib")),
        "{symbols:?}"
    );
}
