//! scip-typescript names a member `file::Owner::...::name`. Tree-sitter must
//! give the same member the same id, or the import makes a second symbol
//! instead of enriching the first (the fixture's index is checked in:
//! regenerate it with `scip-typescript index` in a copy of the directory).

use infigraph_core::extract::extract_file;

#[test]
fn every_owned_scip_member_of_the_fixture_is_a_tree_sitter_symbol() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scip_ts_properties");
    let source = std::fs::read(dir.join("a.ts")).unwrap();
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_extension(".ts").unwrap();
    let extracted = extract_file("a.ts", &source, pack).unwrap();
    let tree_sitter: std::collections::HashSet<&str> =
        extracted.symbols.iter().map(|s| s.id.as_str()).collect();

    let scip = std::fs::read(dir.join("scip.bin")).unwrap();
    let members: Vec<String> = infigraph_core::scip::definition_ids(&scip, "a.ts")
        .unwrap()
        .into_iter()
        .filter(|id| id.trim_start_matches("a.ts::").contains("::"))
        .collect();
    assert!(members.len() >= 10, "the fixture has members: {members:?}");
    let missing: Vec<&String> = members
        .iter()
        .filter(|id| !tree_sitter.contains(id.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "SCIP would make a second symbol for {missing:?}; tree-sitter has {tree_sitter:?}"
    );
}
