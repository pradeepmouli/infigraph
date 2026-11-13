//! A type reference is a `references` edge in Cozo as in Kuzu: written, counted,
//! and read by the dependency queries, so a function that names a type is a
//! caller of it and a change to the type reaches the function.

use infigraph_core::graph::CozoStore;
use tempfile::TempDir;

fn make_cozo() -> (TempDir, CozoStore) {
    let dir = TempDir::new().unwrap();
    let store = CozoStore::open(&dir.path().join("test_cozo.db")).unwrap();
    (dir, store)
}

fn insert_symbol(store: &CozoStore, id: &str, name: &str, kind: &str, file: &str, line: i64) {
    store
        .import_symbols(&[(
            id.into(),
            name.into(),
            kind.into(),
            file.into(),
            line,
            line + 1,
            "".into(),
            "rust".into(),
            "".into(),
            "".into(),
            "".into(),
            0,
            "".into(),
            "".into(),
        )])
        .unwrap();
}

#[test]
fn a_type_reference_round_trips_through_cozo() {
    let (_dir, store) = make_cozo();
    insert_symbol(
        &store,
        "src/lib.rs::make",
        "make",
        "Function",
        "src/lib.rs",
        3,
    );
    insert_symbol(
        &store,
        "src/lib.rs::Point",
        "Point",
        "Struct",
        "src/lib.rs",
        2,
    );

    store
        .import_edges(
            "references",
            &[(
                "src/lib.rs::make".to_string(),
                "src/lib.rs::Point".to_string(),
            )],
        )
        .unwrap();

    // Written and counted: the table holds exactly the one edge.
    let counted = store
        .raw_query("?[count(referrer)] := *references{referrer, target}")
        .unwrap();
    assert_eq!(counted[0][0], "1", "one references edge after the import");

    // Read by the dependency queries, in both directions.
    let callers = store.find_all_references("src/lib.rs::Point").unwrap();
    assert!(
        callers.iter().any(|r| r.caller_id == "src/lib.rs::make"),
        "make names Point, so it must be a reference of Point: {callers:?}"
    );
    let affected = store.transitive_impact("src/lib.rs::Point", 1).unwrap();
    assert!(
        affected.iter().any(|a| a.id == "src/lib.rs::make"),
        "changing Point affects make: {affected:?}"
    );
}
