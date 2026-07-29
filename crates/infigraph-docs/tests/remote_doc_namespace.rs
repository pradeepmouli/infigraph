//! Integration tests for doc-store namespace isolation against a live Neo4j instance.
//!
//! Commit 2d4bd66 added `DocIndex::set_namespace` (crates/infigraph-docs/src/lib.rs)
//! so multiple repos sharing one Neo4j instance in remote/group mode don't collide
//! on identical relative paths (e.g. every repo has a `README.md`).
//!
//! Two complementary tests: one drives `Neo4jDocStore` directly with
//! pre-namespaced ids/files (store-layer isolation), the other drives the real
//! `DocIndex::set_namespace` + `index()` production entrypoint end-to-end
//! (confirms `index()`'s namespacing logic itself works). The standalone
//! `infigraph index` / `index-docs` CLI paths did not call `set_namespace` in
//! remote mode (unlike `group build`, which always did) — see the doc comment
//! on the second test for that finding and how it was fixed.
//!
//! Requires: `docker run -d -p 7687:7687 -e NEO4J_AUTH=neo4j/testpass neo4j:5-community`
//! Run: `NEO4J_URI=127.0.0.1:7687 NEO4J_USER=neo4j NEO4J_PASSWORD=testpass cargo test -p infigraph-docs --features remote --test remote_doc_namespace -- --ignored --test-threads=1`
//!
//! Tests share a single Neo4j instance and use `DETACH DELETE` for isolation,
//! so they MUST run with `--test-threads=1`.

#![cfg(feature = "remote")]

use infigraph_docs::backend::DocBackend;
use infigraph_docs::chunk::Chunk;
use infigraph_docs::extract::{DocFormat, ExtractedDoc};
use infigraph_docs::neo4j_store::Neo4jDocStore;

fn connect() -> Neo4jDocStore {
    Neo4jDocStore::connect_from_env().expect("Neo4j connection — is Docker running?")
}

fn clear_store(store: &Neo4jDocStore) {
    store.clear_all().expect("clear docs/chunks");
}

/// Builds a README doc + one chunk for `repo`, pre-namespaced the same way
/// `fixture_namespaced` in `infigraph-core/tests/neo4j_backend.rs` pre-namespaces
/// code-graph fixtures: by baking `{repo}/` into the id/file at the call site.
/// This exercises `Neo4jDocStore` directly, one layer below `DocIndex::index()`'s
/// own namespacing logic (which the second test below exercises end-to-end).
fn namespaced_readme(repo: &str, body: &str) -> (ExtractedDoc, Chunk) {
    let file = format!("{repo}/README.md");
    let doc = ExtractedDoc {
        file: file.clone(),
        title: Some("README".into()),
        content_hash: format!("hash-{repo}"),
        format: DocFormat::Markdown,
        text: body.to_string(),
        page_count: None,
    };
    let chunk = Chunk {
        id: format!("{file}::chunk_0"),
        doc_file: file,
        content_hash: format!("hash-{repo}"),
        index: 0,
        heading: None,
        text: body.to_string(),
        start_offset: 0,
        end_offset: body.len(),
        page: None,
    };
    (doc, chunk)
}

/// Two repos both have a `README.md`. Driven through the real `Neo4jDocStore`
/// write path (`upsert_docs`) with pre-namespaced ids/files, exercising the
/// store layer directly (complements `test_docindex_set_namespace_isolates_end_to_end`
/// below, which drives the same scenario through the real `DocIndex::set_namespace`
/// and `index()` production entrypoint). Asserts both docs coexist without
/// collision, mirroring `test_neo4j_namespace_isolation`'s assertion style:
/// counts, content isolation, remove-one-keep-other.
#[test]
#[ignore]
fn test_neo4j_doc_namespace_prevents_collision() {
    let store = connect();
    clear_store(&store);

    let (doc_a, chunk_a) = namespaced_readme("repo-a", "Repo A readme content");
    let (doc_b, chunk_b) = namespaced_readme("repo-b", "Repo B readme content");

    store
        .upsert_docs(&[&doc_a], &[&chunk_a])
        .expect("upsert repo-a README");
    store
        .upsert_docs(&[&doc_b], &[&chunk_b])
        .expect("upsert repo-b README");

    let stats = store.stats().expect("stats");
    assert_eq!(
        stats.document_count, 2,
        "both repos' README.md should coexist as 2 distinct Document nodes"
    );
    assert_eq!(
        stats.chunk_count, 2,
        "both repos' chunks should coexist as 2 distinct Chunk nodes"
    );

    // Content must not be cross-contaminated: each namespaced file's hash
    // resolves to that repo's own content, not the other repo's.
    let hashes = store.get_doc_hashes().expect("doc hashes");
    assert_eq!(
        hashes.get("repo-a/README.md").map(String::as_str),
        Some("hash-repo-a"),
        "repo-a's README hash must be its own, not repo-b's"
    );
    assert_eq!(
        hashes.get("repo-b/README.md").map(String::as_str),
        Some("hash-repo-b"),
        "repo-b's README hash must be its own, not repo-a's"
    );

    // Remove repo-a's doc — repo-b's must survive untouched.
    store
        .delete_docs_by_ids(&["repo-a/README.md"])
        .expect("delete repo-a README");

    let stats_after = store.stats().expect("stats after delete");
    assert_eq!(
        stats_after.document_count, 1,
        "only repo-b's README should remain"
    );
    assert_eq!(
        stats_after.chunk_count, 1,
        "only repo-b's chunk should remain"
    );

    let hashes_after = store.get_doc_hashes().expect("doc hashes after delete");
    assert!(
        !hashes_after.contains_key("repo-a/README.md"),
        "repo-a's README should be gone"
    );
    assert_eq!(
        hashes_after.get("repo-b/README.md").map(String::as_str),
        Some("hash-repo-b"),
        "repo-b's README must survive removal of repo-a's"
    );

    clear_store(&store);
}

/// End-to-end test of the real production entrypoint: `DocIndex::open` +
/// `set_namespace` + `init` + `index`, against live Neo4j. This exercises
/// `DocIndex::index()`'s actual namespacing logic (crates/infigraph-docs/src/lib.rs,
/// `let ns = self.namespace.as_deref();` then `rel = format!("{prefix}/{raw_rel}")`,
/// which feeds `ExtractedDoc.file` and is also used to scope the stale-prune and
/// link-extraction steps to this repo's own namespace) — confirming the write
/// path DOES correctly prefix ids/files when a namespace is set.
///
/// Note: whether anything in production actually CALLS `set_namespace` for the
/// doc-indexing path used to be a separate open question from what this test
/// checks — it has since been fixed. `infigraph-cli/src/group_commands.rs`'s
/// `cmd_group` (the `group build` Step 5 loop) already called `set_namespace`
/// correctly and was never affected. The actual gap was in the standalone
/// `infigraph index` / `infigraph index-docs` paths: `cmd_index_docs`
/// (`infigraph-cli/src/info_commands.rs`) took no namespace parameter, so
/// `crates/infigraph-cli/src/index.rs::cmd_index` (which already computes
/// `remote_ns` from the registry for the code graph) never passed it through
/// to doc indexing, and the standalone `index-docs` CLI subcommand
/// (`main.rs`) had no namespace resolution at all. Both are now fixed:
/// `cmd_index_docs` takes `namespace: Option<&str>` and calls
/// `idx.set_namespace(ns)` when set, and both call sites resolve it the same
/// way `cmd_index` does (`Registry::resolve_repo_namespace`) before calling in.
/// `infigraph-mcp/src/tools/docs.rs::open_doc_index` remains a separate,
/// unfixed MCP-side path (used by search/pipeline tools, not group/remote doc
/// indexing) — out of scope here. This test still covers only the mechanism
/// itself, exercised via raw `DocIndex` calls rather than through the CLI
/// entrypoint (`cmd_index_docs` is `pub(crate)` in a different crate and
/// exercising it here would need live Neo4j plus cross-crate test plumbing;
/// a CLI-level regression test for this fix would belong in
/// `infigraph-cli/src/info_commands.rs` or `index.rs`'s own test module).
#[test]
#[ignore]
fn test_docindex_set_namespace_isolates_end_to_end() {
    let dir_a = tempfile::tempdir().expect("tempdir a");
    let dir_b = tempfile::tempdir().expect("tempdir b");
    std::fs::write(dir_a.path().join("README.md"), "Repo A readme content")
        .expect("write repo-a README");
    std::fs::write(dir_b.path().join("README.md"), "Repo B readme content")
        .expect("write repo-b README");

    // Ensure Neo4j remote mode is selected for DocIndex::init().
    std::env::set_var("INFIGRAPH_BACKEND", "neo4j");

    let store_for_cleanup = connect();
    clear_store(&store_for_cleanup);

    let mut idx_a = infigraph_docs::DocIndex::open(dir_a.path()).expect("open a");
    idx_a.set_namespace("repo-a");
    idx_a.set_skip_file_embeddings(true);
    idx_a.init().expect("init a");
    idx_a.index().expect("index a");

    let mut idx_b = infigraph_docs::DocIndex::open(dir_b.path()).expect("open b");
    idx_b.set_namespace("repo-b");
    idx_b.set_skip_file_embeddings(true);
    idx_b.init().expect("init b");
    idx_b.index().expect("index b");

    let stats = store_for_cleanup.stats().expect("stats");
    assert_eq!(
        stats.document_count, 2,
        "both repos' README.md should coexist as 2 distinct Document nodes \
         when indexed through DocIndex::set_namespace + index()"
    );

    let hashes = store_for_cleanup.get_doc_hashes().expect("doc hashes");
    assert!(
        hashes.contains_key("repo-a/README.md"),
        "repo-a's README should be namespaced to 'repo-a/README.md'"
    );
    assert!(
        hashes.contains_key("repo-b/README.md"),
        "repo-b's README should be namespaced to 'repo-b/README.md'"
    );
    assert!(
        !hashes.contains_key("README.md"),
        "unprefixed 'README.md' should not exist — both repos' docs must be namespaced"
    );

    clear_store(&store_for_cleanup);
    std::env::remove_var("INFIGRAPH_BACKEND");
}
