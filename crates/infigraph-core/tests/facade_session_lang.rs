use infigraph_core::graph::SessionStore;
use infigraph_core::lang::{CustomEdgeDef, LanguagePack};

// ==================== SessionStore ====================

#[test]
fn test_session_store_open_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("my_sessions");
    let store = SessionStore::open_dir(&dir).unwrap();
    assert!(dir.exists());
    assert_eq!(store.sessions_dir(), dir.as_path());
}

#[test]
fn test_session_store_sessions_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("sess");
    let store = SessionStore::open_dir(&dir).unwrap();
    assert_eq!(store.sessions_dir(), dir.as_path());
}

// ==================== LanguagePack::new_with_custom_edges ====================

#[test]
fn test_language_pack_new_with_custom_edges() {
    let grammar = tree_sitter_python::LANGUAGE.into();

    let custom_edges = vec![CustomEdgeDef {
        name: "DECORATED_BY".to_string(),
        capture: "decorates".to_string(),
    }];

    let entity_query = "(function_definition name: (identifier) @function.name) @function.def";
    let relation_query = "(call function: (identifier) @call.ref)";

    let pack = LanguagePack::new_with_custom_edges(
        "python_custom",
        vec![".pyc"],
        grammar,
        entity_query,
        relation_query,
        custom_edges,
    );
    assert!(pack.is_ok());
    let p = pack.unwrap();
    assert_eq!(p.name, "python_custom");
    assert!(!p.custom_edges.is_empty());
    assert_eq!(p.custom_edges[0].name, "DECORATED_BY");
}

#[test]
fn test_language_pack_new_basic() {
    let grammar = tree_sitter_python::LANGUAGE.into();
    let entity_query = "(function_definition name: (identifier) @function.name) @function.def";
    let relation_query = "(call function: (identifier) @call.ref)";

    let pack = LanguagePack::new(
        "test_python",
        vec![".py"],
        grammar,
        entity_query,
        relation_query,
    );
    assert!(pack.is_ok());
    let p = pack.unwrap();
    assert_eq!(p.name, "test_python");
    assert!(p.custom_edges.is_empty());
}
