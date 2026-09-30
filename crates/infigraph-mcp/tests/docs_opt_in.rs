//! MCP reads of a project that has not opted in to documents (docs opt-in).

use serde_json::json;

use infigraph_mcp::tools::docs::{open_doc_index, tool_search_docs};

/// `open_doc_index` is what `search` with `scope = "all"` calls on every
/// search: it must never create a document index.
#[test]
fn reading_documents_of_a_project_that_has_not_opted_in_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_string_lossy().to_string();

    let err = open_doc_index(&json!({ "path": &path }))
        .err()
        .expect("nothing to open");
    assert!(
        err.is::<infigraph_core::docs_switch::DocsNotIndexed>(),
        "{err}"
    );

    let out = tool_search_docs(&json!({ "path": &path, "query": "zebra" })).unwrap();
    assert_eq!(out, infigraph_core::docs_switch::DOCS_NOT_INDEXED);

    assert!(
        !dir.path().join(".infigraph").exists(),
        "a read must not create .infigraph/, let alone docs.kuzu"
    );
}
