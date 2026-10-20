//! The clone-bodies sidecar: built next to `embeddings.bin` by every embeddings
//! update, from the bytes of the file the graph was written from.
//!
//! Hermetic: bodies are embedded by a deterministic word-hashing embedder
//! defined here, so no model is loaded or downloaded.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use infigraph_core::clone_detect::bodies::{self, BodyEntry, BODIES_FILE};
use infigraph_core::embed::EmbedProvider;
use infigraph_core::CodeIndex;
use infigraph_languages::bundled_registry;

/// Embeds a text as its normalized word counts, hashed into 64 buckets:
/// equal texts get equal vectors, texts sharing most words get close ones.
struct WordEmbedder(Arc<AtomicUsize>);

impl EmbedProvider for WordEmbedder {
    fn name(&self) -> &'static str {
        "test-words"
    }
    fn dimension(&self) -> usize {
        64
    }
    fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.0.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts
            .iter()
            .map(|text| {
                let mut v = vec![0f32; 64];
                for word in text.split_whitespace() {
                    v[infigraph_core::embed::fnv1a64(word.as_bytes()) as usize % 64] += 1.0;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1.0);
                v.iter().map(|x| x / norm).collect()
            })
            .collect())
    }
}

struct Failing;

impl EmbedProvider for Failing {
    fn dimension(&self) -> usize {
        64
    }
    fn embed_batch(&self, _: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("no model")
    }
}

const TOTAL: &str = "def total(items):\n    result = 0\n    for item in items:\n        if item > 0:\n            result += item * 2\n    return result\n";
/// `TOTAL` with every name and literal changed: the same code, copied.
const GRAND: &str = "def grand(rows):\n    acc = 5\n    for row in rows:\n        if row > 9:\n            acc += row * 7\n    return acc\n";
const OTHER: &str = "def shout(text):\n    return text.upper() + '!'\n";

fn write(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, CodeIndex) {
    let dir = tempfile::tempdir().unwrap();
    for (file, text) in files {
        write(dir.path(), file, text);
    }
    let mut ig = CodeIndex::open(dir.path(), bundled_registry().unwrap()).unwrap();
    ig.init().unwrap();
    ig.index().unwrap();
    (dir, ig)
}

/// One embeddings update with the counting embedder; returns the embed calls.
fn update(ig: &CodeIndex, root: &Path, changed: &[&str]) -> usize {
    let calls = Arc::new(AtomicUsize::new(0));
    infigraph_core::embed::update_embeddings_with(ig.backend().unwrap(), root, changed, &|| {
        Box::new(WordEmbedder(Arc::clone(&calls)))
    })
    .unwrap();
    calls.load(Ordering::SeqCst)
}

fn entries(root: &Path) -> HashMap<String, BodyEntry> {
    bodies::load(&root.join(".infigraph").join(BODIES_FILE))
        .unwrap()
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect()
}

#[test]
fn every_function_gets_tokens_a_sketch_and_a_body_embedding() {
    let (dir, ig) = project(&[("src/a.py", TOTAL), ("src/b.py", OTHER)]);
    update(&ig, dir.path(), &[]);
    let built = entries(dir.path());
    for id in ["src/a.py::total", "src/b.py::shout"] {
        let entry = built.get(id).unwrap_or_else(|| panic!("no entry for {id}"));
        assert!(entry.token_count > 5, "{id}: {}", entry.token_count);
        assert_eq!(entry.embedding.len(), 64, "{id}");
        assert_ne!(entry.input_hash, 0, "{id}");
    }
}

#[test]
fn a_renamed_copy_has_the_same_embedding_input_and_a_different_function_does_not() {
    let (dir, ig) = project(&[
        ("src/a.py", TOTAL),
        ("src/b.py", GRAND),
        ("src/c.py", OTHER),
    ]);
    update(&ig, dir.path(), &[]);
    let built = entries(dir.path());
    let (a, b, c) = (
        &built["src/a.py::total"],
        &built["src/b.py::grand"],
        &built["src/c.py::shout"],
    );
    assert_eq!(a.input_hash, b.input_hash);
    assert_eq!(a.embedding, b.embedding);
    assert_eq!(a.sketch, b.sketch);
    assert_ne!(a.input_hash, c.input_hash);
}

#[test]
fn an_unchanged_body_is_not_embedded_again() {
    let (dir, ig) = project(&[("src/a.py", TOTAL), ("src/b.py", OTHER)]);
    assert!(update(&ig, dir.path(), &[]) > 0);
    let before = std::fs::read(dir.path().join(".infigraph").join(BODIES_FILE)).unwrap();
    assert_eq!(update(&ig, dir.path(), &[]), 0, "nothing changed");
    assert_eq!(
        std::fs::read(dir.path().join(".infigraph").join(BODIES_FILE)).unwrap(),
        before
    );
}

#[test]
fn a_file_rewritten_after_the_graph_write_yields_no_entry_and_no_panic() {
    let (dir, ig) = project(&[("src/a.py", TOTAL), ("src/b.py", OTHER)]);
    // The graph still says `total` spans lines 1-6. The file is now one line.
    write(dir.path(), "src/a.py", "x = 1\n");
    update(&ig, dir.path(), &[]);
    let built = entries(dir.path());
    assert!(
        !built.contains_key("src/a.py::total"),
        "an entry was built from a file the graph was not written from"
    );
    assert!(built.contains_key("src/b.py::shout"), "{:?}", built.keys());
}

#[test]
fn a_rewritten_file_keeps_nothing_it_cannot_vouch_for_after_a_reindex() {
    let (dir, ig) = project(&[("src/a.py", TOTAL), ("src/b.py", OTHER)]);
    update(&ig, dir.path(), &[]);
    let old = entries(dir.path())["src/a.py::total"].clone();
    write(dir.path(), "src/a.py", GRAND);
    ig.index().unwrap();
    update(&ig, dir.path(), &["src/a.py"]);
    let built = entries(dir.path());
    assert!(!built.contains_key("src/a.py::total"), "the symbol is gone");
    assert!(built.contains_key("src/a.py::grand"));
    assert_eq!(built["src/a.py::grand"].input_hash, old.input_hash);
}

#[test]
fn a_missing_sidecar_is_built_whole_even_when_only_one_file_changed() {
    let (dir, ig) = project(&[("src/a.py", TOTAL), ("src/b.py", OTHER)]);
    update(&ig, dir.path(), &[]);
    std::fs::remove_file(dir.path().join(".infigraph").join(BODIES_FILE)).unwrap();
    update(&ig, dir.path(), &["src/a.py"]);
    let built = entries(dir.path());
    assert!(built.contains_key("src/a.py::total") && built.contains_key("src/b.py::shout"));
}

#[test]
fn a_failed_embedder_still_leaves_the_sketch() {
    let (dir, ig) = project(&[("src/a.py", TOTAL)]);
    infigraph_core::embed::update_embeddings_with(ig.backend().unwrap(), dir.path(), &[], &|| {
        Box::new(Failing)
    })
    .unwrap();
    let entry = &entries(dir.path())["src/a.py::total"];
    assert!(entry.embedding.is_empty());
    assert!(entry.token_count > 5);
    let live = infigraph_core::degraded::live::for_root(dir.path());
    assert!(
        live.iter().any(|n| n.key == "clone-bodies-partial"),
        "{live:?}"
    );
}

#[test]
fn deleted_functions_are_pruned() {
    let (dir, ig) = project(&[("src/a.py", TOTAL), ("src/b.py", OTHER)]);
    update(&ig, dir.path(), &[]);
    std::fs::remove_file(dir.path().join("src/b.py")).unwrap();
    ig.index().unwrap();
    update(&ig, dir.path(), &[]);
    assert!(!entries(dir.path()).contains_key("src/b.py::shout"));
}

#[test]
fn a_sidecar_of_an_older_format_is_rebuilt_whole() {
    let (dir, ig) = project(&[("src/a.py", TOTAL), ("src/b.py", OTHER)]);
    update(&ig, dir.path(), &[]);
    let path = dir.path().join(".infigraph").join(BODIES_FILE);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4] = 1; // what the previous normalization wrote
    std::fs::write(&path, bytes).unwrap();
    assert!(bodies::load(&path).is_err());

    assert!(update(&ig, dir.path(), &[]) > 0, "nothing was re-embedded");
    assert_eq!(entries(dir.path()).len(), 2);
}

/// A symbol only a SCIP indexer found has the line of its identifier and no
/// body: sittir's graph holds one per file (named after the file, 1-1), and no
/// update can ever give them an entry.
fn add_scip_namespace(ig: &CodeIndex, file: &str) {
    ig.backend()
        .unwrap()
        .raw_query(&format!(
            "CREATE (:Symbol {{id: '{file}::{file}', name: '{file}', kind: 'Function', \
             file: '{file}', start_line: 1, end_line: 1, signature_hash: '', language: 'scip', \
             visibility: '', parent: '', docstring: '', complexity: 0, parameters: '', \
             return_type: ''}})"
        ))
        .unwrap();
}

#[test]
fn a_symbol_with_no_body_span_is_not_counted_as_missing_its_body_signal() {
    let (dir, ig) = project(&[("src/a.py", TOTAL), ("src/b.py", OTHER)]);
    // A file the indexer analysed that tree-sitter never stored (a types-only
    // module): the graph has its namespace symbol and no content hash for it.
    write(
        dir.path(),
        "src/types.ts",
        "export interface Item { id: number }\n",
    );
    add_scip_namespace(&ig, "src/types.ts");
    let report = infigraph_core::clone_detect::build::update_bodies(
        ig.backend().unwrap(),
        dir.path(),
        &[],
        &|| Box::new(WordEmbedder(Arc::new(AtomicUsize::new(0)))),
    )
    .unwrap();
    // Denominator: the functions that can have a body. The namespace cannot,
    // so "0 of 2 functions have no body signal", not "1 of 3".
    assert_eq!((report.symbols, report.embedded), (2, 2), "{report:?}");
    assert_eq!(entries(dir.path()).len(), 2);
    let live = infigraph_core::degraded::live::for_root(dir.path());
    assert!(
        !live.iter().any(|n| n.key == "clone-bodies-partial"),
        "{live:?}"
    );
}

#[test]
fn the_clone_report_leaves_a_symbol_with_no_body_span_out_of_its_counts() {
    let (dir, ig) = project(&[("src/a.py", TOTAL), ("src/b.py", OTHER)]);
    write(
        dir.path(),
        "src/types.ts",
        "export interface Item { id: number }\n",
    );
    add_scip_namespace(&ig, "src/types.ts");
    update(&ig, dir.path(), &[]);
    let report = infigraph_core::clone_detect::find::find_clones_with(
        ig.backend().unwrap(),
        dir.path(),
        &infigraph_core::clone_detect::find::Options {
            min_tokens: 1,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let notes = report.caveats().join("\n");
    assert!(!notes.contains("no body signal"), "{notes}");
    assert_eq!(report.symbols, 2, "{notes}");
}
