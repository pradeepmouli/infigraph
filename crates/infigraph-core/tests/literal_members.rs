//! `extract::literal_members`: who owns the members of an object literal.
//! (An integration test: it needs the real TypeScript pack, and
//! infigraph-languages is a dev-dependency of this crate.)

use infigraph_core::extract::literal_members;

const SRC: &str = "\
export const OXFMT_CONFIG: FormatConfig = {
  semi: true,
  ignorePatterns: ['a'],
  nested: { inner: 1, deeper: { leaf: 2 } },
  helper() {},
  short,
  'quoted-key': 3,
};
foo({ anon: 1 });
const list = [{ inArray: 1 }];
const wrapped = { a: { b: 1 } } as const;
const viaArray = { items: [{ x: 1 }] };
var legacy = { old: 1 };
export default function f() {
  const local = { z: 1 };
}
export const exported = { e: 1 };
";

fn members() -> Vec<(String, Vec<String>)> {
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_file("a.ts").unwrap();
    let mut found: Vec<_> = literal_members(SRC.as_bytes(), pack).into_iter().collect();
    found.sort_by_key(|(at, _)| *at);
    found.into_iter().map(|(_, m)| (m.name, m.owners)).collect()
}

fn owners_of(name: &str) -> Vec<String> {
    members()
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no member {name}"))
        .1
}

#[test]
fn a_literal_bound_to_a_name_owns_its_members() {
    for name in [
        "semi",
        "ignorePatterns",
        "nested",
        "helper",
        "short",
        "quoted-key",
    ] {
        assert_eq!(owners_of(name), ["OXFMT_CONFIG"], "{name}");
    }
}

#[test]
fn a_nested_literal_is_owned_by_the_chain_of_keys_to_it() {
    assert_eq!(owners_of("inner"), ["OXFMT_CONFIG", "nested"]);
    assert_eq!(owners_of("leaf"), ["OXFMT_CONFIG", "nested", "deeper"]);
    assert_eq!(
        owners_of("b"),
        ["wrapped", "a"],
        "as const is looked through"
    );
}

#[test]
fn a_literal_nobody_names_has_no_owner() {
    assert!(owners_of("anon").is_empty(), "a call argument");
    assert!(owners_of("inArray").is_empty(), "an array element");
    assert!(
        owners_of("x").is_empty(),
        "a literal inside an array under a key"
    );
    assert_eq!(
        owners_of("items"),
        ["viaArray"],
        "the key itself is still a member"
    );
}

#[test]
fn a_member_is_found_by_where_its_key_starts() {
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_file("a.ts").unwrap();
    let found = literal_members(SRC.as_bytes(), pack);
    // line 2 (0-based 1), after two spaces: `semi`
    assert_eq!(found[&(1, 2)].name, "semi");
    assert_eq!(found[&(1, 2)].owners, ["OXFMT_CONFIG"]);
}

#[test]
fn a_pack_without_member_captures_finds_nothing() {
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_file("a.py").unwrap();
    assert!(literal_members(b"x = {'a': 1}", pack).is_empty());
}

#[test]
fn tsx_files_are_read_the_same_way() {
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_file("a.tsx").expect("a tsx pack");
    let found = literal_members(SRC.as_bytes(), pack);
    assert_eq!(found[&(1, 2)].owners, ["OXFMT_CONFIG"]);
}

#[test]
fn the_captures_emit_no_symbol() {
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_file("a.ts").unwrap();
    let extraction = infigraph_core::extract::extract_file("a.ts", SRC.as_bytes(), pack).unwrap();
    let names: Vec<_> = extraction.symbols.iter().map(|s| s.name.as_str()).collect();
    // (`helper() {}` is a method_definition, which the older method pattern
    // already emits; the new captures add nothing to it.)
    for member in ["semi", "ignorePatterns", "inner", "leaf", "anon"] {
        assert!(
            !names.contains(&member),
            "{member} must not become a symbol: {names:?}"
        );
    }
    assert!(
        names.contains(&"OXFMT_CONFIG"),
        "the binding itself still does: {names:?}"
    );
}

#[test]
fn only_a_module_level_binding_names_a_literal() {
    assert_eq!(owners_of("old"), ["legacy"], "var counts");
    assert_eq!(owners_of("e"), ["exported"], "export counts");
    assert!(
        owners_of("z").is_empty(),
        "a literal bound inside a function has no stable owner name"
    );
}

#[test]
fn a_method_of_a_named_literal_is_a_symbol_under_its_owner() {
    let src = "\
export const api = {
  helper() {},
  nested: { deep() {}, value: 1 },
};
function f() {
  const local = { inner() {} };
}
const dup = { run() {}, other: { run() {} } };
";
    let registry = infigraph_languages::bundled_registry().unwrap();
    let pack = registry.for_file("a.ts").unwrap();
    let extraction = infigraph_core::extract::extract_file("a.ts", src.as_bytes(), pack).unwrap();
    let mut ids: Vec<_> = extraction
        .symbols
        .iter()
        .filter(|s| s.kind == infigraph_core::model::SymbolKind::Method)
        .map(|s| (s.id.as_str(), s.parent.as_deref()))
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            ("a.ts::api::helper", Some("a.ts::api")),
            ("a.ts::api::nested::deep", Some("a.ts::api::nested")),
            ("a.ts::dup::other::run", Some("a.ts::dup::other")),
            ("a.ts::dup::run", Some("a.ts::dup")),
            // a literal bound inside a function has no owner
            ("a.ts::inner", None),
        ]
    );
}
