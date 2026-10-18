//! A member of an object literal bound to a name is owned by it in the id,
//! whatever scip-typescript's moniker (which has no owner and a per-file
//! counter) says. Needs the real TypeScript pack, hence an integration test.

use std::sync::Arc;

use infigraph_core::graph::GraphStore;
use infigraph_core::scip::{import_scip_index, use_language_registry};
use protobuf::Message;
use scip::types::{Document, Index, Occurrence, SymbolInformation, SymbolRole};

const FILE: &str = "src/config.ts";
const SOURCE: &str = "\
export const CONFIG = {
  semi: true,
  nested: { inner: 1 },
};
call({ semi: false });
call({ cwd: 1 });
call({ cwd: 2 });
";

fn moniker(descriptor: &str) -> String {
    format!("scip-typescript npm test 1.0.0 src/`config.ts`/{descriptor}")
}

/// Import definitions `(moniker, 0-based line, 0-based column)` into a fresh
/// graph over a project whose `src/config.ts` is `SOURCE`; ids by line.
fn import(defs: &[(String, i32, i32)]) -> Vec<String> {
    import_in(SOURCE, &[], defs)
}

/// `import` over another `source`, with `existing` symbol rows
/// `(id, name, start_line, end_line, kind)` already in the graph, as
/// tree-sitter leaves them.
fn import_in(
    source: &str,
    existing: &[(&str, &str, u32, u32, &str)],
    defs: &[(String, i32, i32)],
) -> Vec<String> {
    import_with_refs(source, existing, defs, &[]).1.ids
}

/// `import_in` plus reference occurrences `(moniker, 0-based line, column)`;
/// returns the graph after the first import and after the second (an import
/// that finds its own rows again must change nothing, but a defect of the first
/// can hide behind the second's repair, so both are checked).
fn import_with_refs(
    source: &str,
    existing: &[(&str, &str, u32, u32, &str)],
    defs: &[(String, i32, i32)],
    refs: &[(String, i32, i32)],
) -> (Snapshot, Snapshot) {
    use_language_registry(Arc::new(infigraph_languages::bundled_registry().unwrap()));
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join(FILE), source).unwrap();
    let store = GraphStore::open(&dir.path().join("graph")).unwrap();
    let conn = store.connection().unwrap();
    conn.query(&format!(
        "CREATE (:File {{id: '{FILE}', name: '{FILE}', path: '{FILE}', \
         language: 'typescript', symbol_count: 0}})"
    ))
    .unwrap();

    for (id, name, start, end, kind) in existing {
        conn.query(&format!(
            "CREATE (:Symbol {{id: '{id}', name: '{name}', kind: '{kind}', file: '{FILE}', \
             start_line: {start}, end_line: {end}, signature_hash: '', language: 'typescript', \
             visibility: 'public', parent: '', docstring: '', complexity: 0, parameters: '', \
             return_type: ''}})"
        ))
        .unwrap();
    }

    let doc = Document {
        relative_path: FILE.to_string(),
        occurrences: defs
            .iter()
            .map(|(sym, line, col)| Occurrence {
                range: vec![*line, *col, *line, *col + 3],
                symbol: sym.clone(),
                symbol_roles: SymbolRole::Definition as i32,
                ..Default::default()
            })
            .chain(refs.iter().map(|(sym, line, col)| Occurrence {
                range: vec![*line, *col, *line, *col + 3],
                symbol: sym.clone(),
                symbol_roles: 0,
                ..Default::default()
            }))
            .collect(),
        symbols: defs
            .iter()
            .map(|(sym, ..)| SymbolInformation {
                symbol: sym.clone(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let index = Index {
        documents: vec![doc],
        ..Default::default()
    };
    let path = dir.path().join("index.scip");
    std::fs::write(&path, index.write_to_bytes().unwrap()).unwrap();
    import_scip_index(&path, &store, Some(dir.path())).unwrap();
    let first = snapshot(&conn);
    // matched rows stay matched: the same index again adds nothing
    let again = import_scip_index(&path, &store, Some(dir.path())).unwrap();
    assert_eq!(
        again.symbols_added, 0,
        "a second import must match what the first made"
    );

    for (sym, ..) in defs {
        let rows: Vec<_> = conn
            .query(&format!(
                "MATCH (s:Symbol) WHERE s.scip_id = '{sym}' RETURN s.id"
            ))
            .unwrap()
            .collect();
        assert_eq!(rows.len(), 1, "exactly one row carries {sym}");
    }
    (first, snapshot(&conn))
}
/// What the graph holds: ids, CALLS edges and each row's moniker.
struct Snapshot {
    ids: Vec<String>,
    calls: Vec<String>,
    /// `id => scip_id`
    monikers: Vec<String>,
}

fn snapshot(conn: &kuzu::Connection<'_>) -> Snapshot {
    let strip = |v: &kuzu::Value| v.to_string().trim_matches('"').to_string();
    let rows = |q: &str, sep: &str| -> Vec<String> {
        conn.query(q)
            .unwrap()
            .map(|r| format!("{}{sep}{}", strip(&r[0]), strip(&r[1])))
            .collect()
    };
    Snapshot {
        ids: rows(
            "MATCH (s:Symbol) RETURN s.id, s.name ORDER BY s.start_line, s.id",
            " | ",
        ),
        calls: rows(
            "MATCH (a:Symbol)-[:CALLS]->(b:Symbol) RETURN a.id, b.id ORDER BY a.id, b.id",
            " -> ",
        ),
        monikers: rows(
            "MATCH (s:Symbol) RETURN s.id, s.scip_id ORDER BY s.id",
            " => ",
        ),
    }
}

#[test]
fn a_member_of_a_named_literal_is_named_under_it_without_the_counter() {
    let ids = import(&[
        (moniker("semi0:"), 1, 2),
        (moniker("nested0:"), 2, 2),
        (moniker("inner0:"), 2, 12),
    ]);
    assert_eq!(
        ids,
        vec![
            "src/config.ts::CONFIG::semi | semi",
            "src/config.ts::CONFIG::nested | nested",
            "src/config.ts::CONFIG::nested::inner | inner",
        ]
    );
}

#[test]
fn a_member_of_a_literal_nobody_names_loses_the_counter_and_keeps_its_key() {
    // `call({ semi: false })` has no owner, and `CONFIG.semi` is another id
    let ids = import(&[(moniker("semi0:"), 1, 2), (moniker("semi1:"), 4, 7)]);
    assert_eq!(
        ids,
        vec![
            "src/config.ts::CONFIG::semi | semi",
            "src/config.ts::semi | semi",
        ]
    );
}

#[test]
fn unowned_members_with_one_key_are_numbered_in_source_order_not_by_the_indexers_counter() {
    // the indexer's counters (here reversed) say nothing about order in the file
    let ids = import(&[(moniker("cwd5:"), 5, 7), (moniker("cwd2:"), 6, 7)]);
    assert_eq!(
        ids,
        vec!["src/config.ts::cwd#1 | cwd", "src/config.ts::cwd#2 | cwd"]
    );
}

#[test]
fn a_key_the_source_does_not_have_at_that_position_is_left_alone() {
    // the file changed since the indexer ran: no member starts at (0, 0)
    let ids = import(&[(moniker("semi0:"), 0, 0)]);
    assert_eq!(ids, vec!["src/config.ts::semi0 | semi0"]);
}

#[test]
fn a_symbol_that_only_encloses_a_key_is_not_that_member() {
    let source = "function value() {\n  return { value: 1 };\n}\n";
    // tree-sitter's `value` function spans lines 1-3; the key is on line 2
    let ids = import_in(
        source,
        &[("src/config.ts::value", "value", 1, 3, "function")],
        &[(moniker("value0:"), 1, 11)],
    );
    assert_eq!(
        ids,
        vec![
            "src/config.ts::value | value",
            "src/config.ts::value#2 | value"
        ],
        "the function keeps its row and the member is its own symbol, numbered past it: {ids:?}"
    );
}

#[test]
fn two_members_with_one_key_on_one_line_are_two_symbols() {
    // generated one-line records: the second `w` starts on the first one's line
    let source = "export const R = { A: { w: 1 }, B: { w: 2 } };\n";
    let ids = import_in(
        source,
        &[],
        &[(moniker("w0:"), 0, 24), (moniker("w1:"), 0, 37)],
    );
    assert_eq!(
        ids,
        vec!["src/config.ts::R::A::w | w", "src/config.ts::R::B::w | w",],
        "a literal member never merges into one this same import made: {ids:?}"
    );
}

#[test]
fn a_method_tree_sitter_made_is_enriched_by_its_scip_row_not_duplicated() {
    // tree-sitter extracted `helper() {}` as CONFIG::helper on line 2
    let source = "export const CONFIG = {\n  helper() {},\n};\n";
    let ids = import_in(
        source,
        &[("src/config.ts::CONFIG::helper", "helper", 2, 2, "method")],
        &[(moniker("helper0:"), 1, 2)],
    );
    assert_eq!(
        ids,
        vec!["src/config.ts::CONFIG::helper | helper"],
        "the SCIP row finds the tree-sitter row by its line: {ids:?}"
    );
}

#[test]
fn a_key_after_non_ascii_text_is_found_by_scips_utf16_column() {
    // SCIP counts UTF-16 units from the line start; tree-sitter counts bytes.
    // `é`, `→` and `日` are 1 unit each and 2, 3 and 3 bytes.
    let source = "export const R = { a: \"é→日\", key: 2 };\n";
    let ids = import_in(source, &[], &[(moniker("key0:"), 0, 29)]);
    assert_eq!(ids, vec!["src/config.ts::R::key | key"], "{ids:?}");
}

// ---- review of the import's matching ------------------------------------------------

#[test]
fn a_second_import_keeps_two_same_line_members_on_their_own_rows() {
    // `Box#empty.` and the `empty` inside its type literal: two non-literal
    // monikers, one name, one line. The second import must hand each occurrence
    // the row that carries its moniker, not the first row that contains it.
    let source = "export class Box { empty = 1; shape: { empty: number } }\n";
    let ids = import_in(
        source,
        &[],
        &[
            (moniker("Box#empty."), 0, 19),
            (moniker("Box#empty.typeLiteral12:empty."), 0, 39),
        ],
    );
    assert_eq!(
        ids,
        vec![
            "src/config.ts::Box::empty | empty",
            "src/config.ts::Box::empty::empty | empty"
        ],
        "{ids:?}"
    );
}

#[test]
fn a_function_that_encloses_a_key_on_one_line_is_not_that_member() {
    // the same shape as the multi-line case, all on one line: the function
    // starts on the key's line, so a line match alone would take it
    let source = "function value() { return { value: 1 }; }\n";
    let ids = import_in(
        source,
        &[("src/config.ts::value", "value", 1, 1, "function")],
        &[(moniker("value()."), 0, 9), (moniker("value0:"), 0, 28)],
    );
    assert_eq!(
        ids,
        vec![
            "src/config.ts::value | value",
            "src/config.ts::value#2 | value"
        ],
        "{ids:?}"
    );
}

#[test]
fn same_line_methods_with_one_name_each_keep_their_own_moniker() {
    let source = "export const f = { a: { k() {} }, b: { k() {} } };\n";
    let (first, second) = import_with_refs(
        source,
        &[
            ("src/config.ts::f::a::k", "k", 1, 1, "method"),
            ("src/config.ts::f::b::k", "k", 1, 1, "method"),
        ],
        // the indexer's counters run against source order
        &[(moniker("k1:"), 0, 39), (moniker("k0:"), 0, 24)],
        &[],
    );
    let expected = vec![
        format!("src/config.ts::f::a::k => {}", moniker("k0:")),
        format!("src/config.ts::f::b::k => {}", moniker("k1:")),
    ];
    assert_eq!(
        first.monikers, expected,
        "each tree-sitter row is enriched by its own member, not both by the first"
    );
    assert_eq!(second.monikers, expected);
}

#[test]
fn a_key_the_file_no_longer_has_there_keeps_the_indexers_name() {
    // the file shifted after the indexer ran: (0, 25) now starts `cwd`, the
    // moniker says `semi`
    let source = "export const R = { semi: 1, cwd: 2 };\n";
    let ids = import_in(source, &[], &[(moniker("semi0:"), 0, 28)]);
    assert_eq!(ids, vec!["src/config.ts::semi0 | semi0"], "{ids:?}");
}

#[test]
fn a_call_from_a_renamed_row_keeps_its_container() {
    // two unowned `k` members get `k#1` and `k#2`; a call on the second's
    // line is made by `k#2`
    let source = "call({ k: 1 });\ncall({ k: helper() });\nfunction helper() {}\n";
    let (first, second) = import_with_refs(
        source,
        &[],
        &[
            (moniker("k0:"), 0, 7),
            (moniker("k1:"), 1, 7),
            (moniker("helper()."), 2, 9),
        ],
        &[(moniker("helper()."), 1, 10)],
    );
    let edge = vec!["src/config.ts::k#2 -> src/config.ts::helper".to_string()];
    assert!(second.ids.contains(&"src/config.ts::k#2 | k".to_string()));
    assert_eq!(
        first.calls, edge,
        "the first import's edge, not the second's"
    );
    assert_eq!(second.calls, edge);
}
