// Step 5 ("group build") doc-indexing coverage, local mode.
//
// Companion to crates/infigraph-core/tests/group_build_steps.rs, which covers
// Steps 1-4 (index_group, sync_group_contracts, link_cross_service_calls,
// build_combined_graph) — that crate has no dependency on infigraph-docs, so
// DocIndex/build_combined_docs aren't reachable from its tests. This file fills
// the Step 5 gap using the same `index_doc` seeding pattern as combined_docs.rs.
//
// Finding this test locks in: `group build`'s LOCAL-mode Step 5 calls
// build_combined_docs(&registry, &group), which reads each repo's OWN
// pre-built doc store (.infigraph/docs.kuzu) — it does NOT index docs itself.
// A repo whose own doc store was never populated contributes zero documents to
// the combined store, even if it has real doc files on disk. `group build`
// alone does not freshen a repo's doc store in local mode; something else
// (a standalone `infigraph index`, or a watcher) has to do that first.

use infigraph_core::embed::{load_embeddings, save_embeddings};
use infigraph_core::multi::{Group, Registry, RepoEntry};
use infigraph_docs::chunk::{chunk_document, ChunkStrategy};
use infigraph_docs::combined::build_combined_docs;
use infigraph_docs::extract::{DocFormat, ExtractedDoc};
use infigraph_docs::store::DocStore;
use std::collections::HashMap;
use std::sync::Mutex;

static GROUP_BUILD_DOCS_LOCK: Mutex<()> = Mutex::new(());

fn index_doc(root: &std::path::Path, file: &str, text: &str) {
    let full_path = root.join(file);
    std::fs::create_dir_all(full_path.parent().unwrap()).unwrap();
    std::fs::write(&full_path, text).unwrap();

    let doc = ExtractedDoc {
        file: file.to_string(),
        title: Some(file.to_string()),
        content_hash: format!("hash-{file}"),
        format: DocFormat::Markdown,
        text: text.to_string(),
        page_count: None,
    };
    let chunks = chunk_document(&doc, file, &doc.content_hash, ChunkStrategy::HeadingBounded);
    let chunk_refs: Vec<_> = chunks.iter().collect();

    let db_path = root.join(".infigraph").join("docs.kuzu");
    let store = DocStore::open(&db_path).unwrap();
    store.upsert_all_parquet(&[&doc], &chunk_refs).unwrap();
    drop(store);

    let embeddings_path = root.join(".infigraph").join("docs_embeddings.bin");
    let mut embeddings = if embeddings_path.exists() {
        load_embeddings(&embeddings_path).unwrap()
    } else {
        Vec::new()
    };
    embeddings.extend(
        chunks
            .iter()
            .map(|chunk| (chunk.id.clone(), vec![0.1; 384]))
            .collect::<Vec<_>>(),
    );
    save_embeddings(&embeddings_path, &embeddings).unwrap();
}

fn two_repo_registry(repo_a: &std::path::Path, repo_b: &std::path::Path) -> Registry {
    let mut repos = HashMap::new();
    for (name, path) in [("order-service", repo_a), ("payment-service", repo_b)] {
        repos.insert(
            name.to_string(),
            RepoEntry {
                name: name.to_string(),
                path: path.to_path_buf(),
                languages: Vec::new(),
                symbol_count: 0,
                module_count: 0,
                last_indexed_commit: None,
            },
        );
    }
    let mut groups = HashMap::new();
    groups.insert(
        "docs-steps-group".to_string(),
        Group {
            name: "docs-steps-group".to_string(),
            org: String::new(),
            repos: vec!["order-service".to_string(), "payment-service".to_string()],
            contracts: Vec::new(),
        },
    );
    Registry { repos, groups }
}

/// Step 5, local mode: a repo with real doc files on disk contributes NOTHING
/// to the combined doc store until its own doc store has actually been built.
/// `group build`'s Step 5 does not do that indexing itself in local mode.
#[test]
fn test_step5_combined_docs_empty_until_repo_doc_indexed() {
    let _guard = GROUP_BUILD_DOCS_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    // Real doc file written to disk, but its repo's own .infigraph/docs.kuzu
    // store is never populated — this is the gap.
    std::fs::write(
        repo_a.path().join("README.md"),
        "# Order Service\n\nHandles order payments.",
    )
    .unwrap();

    let registry = two_repo_registry(repo_a.path(), repo_b.path());

    let stats_before = build_combined_docs(&registry, "docs-steps-group")
        .expect("build_combined_docs should succeed even with nothing indexed");
    assert_eq!(
        stats_before.documents, 0,
        "combined docs should be empty — order-service's README exists on disk \
         but its own doc store was never built, so group build's Step 5 (local \
         mode) has nothing to pull from"
    );

    if let Some(h) = old_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
}

/// Step 5, local mode: once a repo's own doc store is current (e.g. from a
/// standalone `infigraph index` run), build_combined_docs picks it up correctly.
#[test]
fn test_step5_combined_docs_includes_repo_once_its_doc_store_is_built() {
    let _guard = GROUP_BUILD_DOCS_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    index_doc(
        repo_a.path(),
        "README.md",
        "# Order Service\n\nHandles order payments.",
    );

    let registry = two_repo_registry(repo_a.path(), repo_b.path());

    let stats_after = build_combined_docs(&registry, "docs-steps-group")
        .expect("build_combined_docs should succeed");
    assert_eq!(
        stats_after.documents, 1,
        "combined docs should include order-service's README once its own doc \
         store is current"
    );

    if let Some(h) = old_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
}
