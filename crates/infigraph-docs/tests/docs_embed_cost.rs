//! What one document edit costs `update_doc_embeddings` as the store grows:
//! the cost the document sink pays on every edit it indexes. Ignored by
//! default (it builds stores of up to tens of thousands of chunks):
//!
//! `cargo test -p infigraph-docs --test docs_embed_cost -- --ignored --nocapture`

use std::time::Instant;

use infigraph_docs::chunk::Chunk;
use infigraph_docs::extract::{DocFormat, ExtractedDoc};
use infigraph_docs::DocIndex;

fn doc(file: &str, text: &str) -> ExtractedDoc {
    ExtractedDoc {
        file: file.to_string(),
        title: None,
        content_hash: format!("hash-{file}"),
        format: DocFormat::Markdown,
        text: text.to_string(),
        page_count: None,
    }
}

fn chunk(file: &str, index: usize) -> Chunk {
    Chunk {
        id: format!("{file}::chunk_{index}"),
        doc_file: file.to_string(),
        content_hash: format!("hash-{file}"),
        index,
        heading: Some(format!("Heading {index}")),
        text: format!("{file} section {index}: some ordinary prose about the system"),
        start_offset: 0,
        end_offset: 60,
        page: None,
    }
}

/// Builds a store of `docs` documents of 5 chunks, with a real
/// `docs_embeddings.bin` beside it, then times one edit of one document.
fn one_edit_at(docs: usize) {
    std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();
    let mut idx = DocIndex::open(&root).unwrap();
    idx.init().unwrap();
    let store = idx.store().unwrap();

    let mut all_chunks = Vec::new();
    let mut extracted = Vec::new();
    for d in 0..docs {
        let file = format!("docs/d{d}.md");
        extracted.push(doc(&file, "body"));
        for c in 0..5 {
            all_chunks.push(chunk(&file, c));
        }
    }
    let doc_refs: Vec<&ExtractedDoc> = extracted.iter().collect();
    let chunk_refs: Vec<&Chunk> = all_chunks.iter().collect();
    let built = Instant::now();
    store.upsert_docs(&doc_refs, &chunk_refs).unwrap();
    let upserted = built.elapsed();

    // Embeddings for every chunk: the real embedder on 256 chunks to learn the
    // dimension, then that vector repeated (the cost is I/O and bookkeeping,
    // not the model).
    let embedder = infigraph_core::embed::doc_embedder();
    let probe = embedder.embed_batch(&["probe text"]).unwrap();
    let dim = probe[0].len();
    let vectors: Vec<(String, Vec<f32>)> = all_chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut v = probe[0].clone();
            v[i % dim] += 1.0;
            (c.id.clone(), v)
        })
        .collect();
    let emb_path = root.join(".infigraph").join("docs_embeddings.bin");
    infigraph_core::embed::save_embeddings(&emb_path, &vectors).unwrap();
    let on_disk = std::fs::metadata(&emb_path).unwrap().len();

    // One edit: three chunks of one document.
    let edited = chunk("docs/d0.md", 0);
    let new_chunks: Vec<Chunk> = (0..3).map(|i| chunk("docs/d0.md", i)).collect();
    let refs: Vec<&Chunk> = new_chunks.iter().collect();
    let _ = edited;
    let started = Instant::now();
    infigraph_docs::embed::update_doc_embeddings(store, &root, &refs, &["docs/d0.md"]).unwrap();
    let edit = started.elapsed();
    println!(
        "docs={docs:>6} chunks={:>7} dim={dim} embeddings.bin={:>6.1}MB upsert={upserted:>8.2?} \
         one-edit update_doc_embeddings={edit:>8.2?}",
        all_chunks.len(),
        on_disk as f64 / 1e6
    );
}

#[test]
#[ignore = "builds stores of up to 40,000 chunks"]
fn what_one_edit_costs_as_the_document_store_grows() {
    for docs in [200, 2_000, 8_000] {
        one_edit_at(docs);
    }
}
