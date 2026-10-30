//! A member's name must be the same in the tree-sitter symbol and in the SCIP
//! one, or the import makes a second symbol for it. scip-typescript spells a
//! member's name as the source has it (`$kind`), or in backticks with its
//! quotes when the name is not an identifier (`'c-strings'`); the canonical
//! name is the one the source spells, so the quotes are not part of it.
//! The fixtures' indexes are checked in: regenerate one with `scip-typescript
//! index` in a copy of its directory.

use infigraph_core::extract::extract_file;
use std::collections::HashSet;
use std::path::Path;

/// Tree-sitter's symbol ids for `a.ts` in `fixture`, and the ids the SCIP
/// import would give its definitions in the same file.
fn ids(fixture: &str) -> (HashSet<String>, Vec<String>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let source = std::fs::read(dir.join("a.ts")).unwrap();
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_extension(".ts").unwrap();
    let extracted = extract_file("a.ts", &source, pack).unwrap();
    let tree_sitter = extracted.symbols.iter().map(|s| s.id.clone()).collect();
    let scip = std::fs::read(dir.join("scip.bin")).unwrap();
    let scip_ids = infigraph_core::scip::definition_ids(&scip, "a.ts").unwrap();
    (tree_sitter, scip_ids)
}

#[test]
fn a_dollar_prefixed_member_is_one_symbol() {
    let (tree_sitter, scip_ids) = ids("scip_ts_member_names");
    assert!(
        scip_ids.iter().any(|id| id == "a.ts::As::$kind"),
        "SCIP names the member `$kind`: {scip_ids:?}"
    );
    let kinds: Vec<&String> = tree_sitter
        .iter()
        .filter(|id| id.ends_with("::$kind"))
        .collect();
    assert_eq!(kinds, [&"a.ts::As::$kind".to_string()], "{tree_sitter:?}");
    let members: Vec<&String> = scip_ids
        .iter()
        .filter(|id| id.trim_start_matches("a.ts::").contains("::"))
        .collect();
    let missing: Vec<&&String> = members
        .iter()
        .filter(|id| !tree_sitter.contains(**id))
        .collect();
    assert!(missing.is_empty(), "SCIP-only members: {missing:?}");
}

#[test]
fn a_quoted_member_name_is_named_without_its_quotes() {
    let (_, scip_ids) = ids("scip_ts_quoted_name");
    assert!(
        scip_ids.iter().any(|id| id == "a.ts::Flags::c-strings"),
        "SCIP names the member `'c-strings'` unquoted: {scip_ids:?}"
    );
    assert!(
        !scip_ids.iter().any(|id| id.contains('\'')),
        "no quote is part of a member's name: {scip_ids:?}"
    );
}
