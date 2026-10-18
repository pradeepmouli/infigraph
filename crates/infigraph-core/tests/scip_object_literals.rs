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
";

fn moniker(descriptor: &str) -> String {
    format!("scip-typescript npm test 1.0.0 src/`config.ts`/{descriptor}")
}

/// Import definitions `(moniker, 0-based line, 0-based column)` into a fresh
/// graph over a project whose `src/config.ts` is `SOURCE`; ids by line.
fn import(defs: &[(String, i32, i32)]) -> Vec<String> {
    use_language_registry(Arc::new(infigraph_languages::bundled_registry().unwrap()));
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join(FILE), SOURCE).unwrap();
    let store = GraphStore::open(&dir.path().join("graph")).unwrap();
    let conn = store.connection().unwrap();
    conn.query(&format!(
        "CREATE (:File {{id: '{FILE}', name: '{FILE}', path: '{FILE}', \
         language: 'typescript', symbol_count: 0}})"
    ))
    .unwrap();

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
fn a_member_of_a_literal_nobody_names_is_left_as_its_moniker_gives_it() {
    // `call({ semi: false })`: this commit only qualifies named literals
    let ids = import(&[(moniker("semi1:"), 4, 8)]);
    assert_eq!(ids, vec!["src/config.ts::semi1 | semi1"]);
}

#[test]
fn a_key_the_source_does_not_have_at_that_position_is_left_alone() {
    // the file changed since the indexer ran: no member starts at (0, 0)
    let ids = import(&[(moniker("semi0:"), 0, 0)]);
    assert_eq!(ids, vec!["src/config.ts::semi0 | semi0"]);
}
