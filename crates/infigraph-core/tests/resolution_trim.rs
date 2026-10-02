//! #150: a full reindex keeps every extraction alive through cross-file
//! resolution, which reads only a small part of each. Reducing them to that
//! part after the graph write must not change a single resolved edge.

use std::collections::BTreeSet;
use std::path::Path;

use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

/// Every edge of the graph built from the microservices fixtures, the way
/// the daemon's full reindex builds it: scan, bulk write, resolve.
fn edges_of_a_full_build(reduce_before_resolution: bool) -> BTreeSet<String> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/microservices");
    let source = tempfile::tempdir().unwrap();
    copy_tree(&fixtures, source.path());
    let mut parsed = Infigraph::open(source.path(), bundled_registry().unwrap()).unwrap();
    parsed.init().unwrap();
    let mut extractions = parsed.index().unwrap().extractions;
    assert!(extractions.len() > 10, "the fixtures were not indexed");

    // A second, empty graph, written the way the daemon's rebuild writes it.
    let project = tempfile::tempdir().unwrap();
    let mut infigraph = Infigraph::open(project.path(), bundled_registry().unwrap()).unwrap();
    infigraph.init().unwrap();
    let backend = infigraph.backend().unwrap();
    backend.upsert_files_bulk(&extractions, true).unwrap();
    if reduce_before_resolution {
        for extraction in &mut extractions {
            extraction.reduce_to_resolution_inputs();
        }
    }
    backend.resolve_calls(&extractions, None).unwrap();

    let mut edges = BTreeSet::new();
    for kind in ["CALLS", "INHERITS", "CONTAINS", "IMPORTS"] {
        let rows = backend
            .raw_query(&format!("MATCH (a)-[r:{kind}]->(b) RETURN a.id, b.id"))
            .unwrap_or_default();
        for row in rows {
            edges.insert(format!("{kind} {} -> {}", row[0], row[1]));
        }
    }
    let total = backend
        .raw_query("MATCH ()-[r]->() RETURN count(r)")
        .unwrap();
    edges.insert(format!("TOTAL {}", total[0][0]));
    edges
}

#[test]
fn reducing_extractions_before_resolution_changes_no_edge() {
    let whole = edges_of_a_full_build(false);
    let reduced = edges_of_a_full_build(true);
    assert!(
        whole.iter().any(|e| e.starts_with("CALLS ")),
        "the fixtures resolved no calls, so this compares nothing"
    );
    let missing: Vec<_> = whole.difference(&reduced).collect();
    let extra: Vec<_> = reduced.difference(&whole).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "resolution reads something the reduction removed.\nmissing: {missing:#?}\nextra: {extra:#?}"
    );
}
