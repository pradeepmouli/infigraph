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
/// `(id, name, start_line, end_line)` already in the graph, as tree-sitter
/// leaves them.
fn import_in(
    source: &str,
    existing: &[(&str, &str, u32, u32)],
    defs: &[(String, i32, i32)],
) -> Vec<String> {
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

    for (id, name, start, end) in existing {
        conn.query(&format!(
            "CREATE (:Symbol {{id: '{id}', name: '{name}', kind: 'function', file: '{FILE}', \
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
    conn.query("MATCH (s:Symbol) RETURN s.id, s.name ORDER BY s.start_line, s.id")
        .unwrap()
        .map(|r| {
            format!(
                "{} | {}",
                r[0].to_string().trim_matches('"'),
                r[1].to_string().trim_matches('"')
            )
        })
        .collect()
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
        &[("src/config.ts::value", "value", 1, 3)],
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
        &[("src/config.ts::CONFIG::helper", "helper", 2, 2)],
        &[(moniker("helper0:"), 1, 2)],
    );
    assert_eq!(
        ids,
        vec!["src/config.ts::CONFIG::helper | helper"],
        "the SCIP row finds the tree-sitter row by its line: {ids:?}"
    );
}
