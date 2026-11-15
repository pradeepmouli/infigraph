//! `embed_in_batches` and the unembedded-count marker: a batch an embedder
//! fails or short-changes is reported with its count and reason, never dropped.

use anyhow::Result;
use infigraph_core::embed::{
    embed_in_batches, read_unembedded_count, write_unembedded_count, EmbedProvider,
};

/// Fails every batch whose first text starts with `bad`, returns one vector too
/// few for one starting with `short`, and a unit vector otherwise.
struct Scripted;

impl EmbedProvider for Scripted {
    fn dimension(&self) -> usize {
        2
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        match texts.first() {
            Some(t) if t.starts_with("bad") => anyhow::bail!("model unavailable"),
            Some(t) if t.starts_with("short") => Ok(vec![vec![1.0, 0.0]; texts.len() - 1]),
            _ => Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect()),
        }
    }
    fn name(&self) -> &'static str {
        "scripted"
    }
}

#[test]
fn every_input_gets_its_own_position() {
    let texts: Vec<String> = (0..700).map(|i| format!("t{i}")).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let got = embed_in_batches(&Scripted, &refs);
    assert!(got.failure.is_none());
    let mut positions: Vec<usize> = got.vectors.iter().map(|(i, _)| *i).collect();
    positions.sort_unstable();
    assert_eq!(positions, (0..700).collect::<Vec<_>>());
    assert_eq!(got.shortfall(700), 0);
}

#[test]
fn a_failed_batch_costs_its_inputs_and_says_why() {
    // 256 per batch: the first batch fails, the other 44 embed.
    let mut texts: Vec<String> = (0..300).map(|i| format!("t{i}")).collect();
    texts[0] = "bad".to_string();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let got = embed_in_batches(&Scripted, &refs);
    assert_eq!(got.shortfall(300), 256);
    assert!(got.vectors.iter().all(|(i, _)| *i >= 256));
    assert!(got.failure.unwrap().contains("model unavailable"));
}

#[test]
fn too_few_vectors_is_a_failure_too() {
    let texts = ["short a", "b", "c"];
    let got = embed_in_batches(&Scripted, &texts);
    assert_eq!(got.shortfall(3), 1);
    assert!(got.failure.unwrap().contains("2 vectors for 3 texts"));
}

#[test]
fn the_unembedded_count_is_a_file_and_zero_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("docs_embeddings.bin");
    assert_eq!(read_unembedded_count(&sidecar), 0, "absent reads as zero");
    write_unembedded_count(&sidecar, 7).unwrap();
    // Read through a path built from scratch: nothing in this process holds it.
    assert_eq!(
        read_unembedded_count(&dir.path().join("docs_embeddings.bin")),
        7
    );
    assert!(dir.path().join("docs_embeddings.bin.unembedded").exists());
    write_unembedded_count(&sidecar, 0).unwrap();
    assert!(!dir.path().join("docs_embeddings.bin.unembedded").exists());
    write_unembedded_count(&sidecar, 0).unwrap();
    std::fs::write(dir.path().join("docs_embeddings.bin.unembedded"), "junk").unwrap();
    assert_eq!(
        read_unembedded_count(&sidecar),
        0,
        "junk is not a shortfall"
    );
}
