//! The sibling marker that records which embedder built a sidecar (#75, D1).
//! It sits beside the 8-byte generation marker and leaves that one's format
//! alone: a daemon and an MCP worker can be different builds for a while.

use infigraph_core::embed::{
    embedder_marker_after, read_embedder_marker, read_generation_marker, write_embedder_marker,
    write_generation_marker,
};

#[test]
fn round_trips_the_embedder_name() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("embeddings.bin");
    std::fs::write(&sidecar, b"stub").unwrap();

    write_embedder_marker(&sidecar, "trigram").unwrap();
    assert_eq!(read_embedder_marker(&sidecar).as_deref(), Some("trigram"));
}

#[test]
fn a_missing_or_empty_marker_reads_as_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("embeddings.bin");
    assert_eq!(read_embedder_marker(&sidecar), None);

    std::fs::write(dir.path().join("embeddings.bin.embedder"), b"  \n").unwrap();
    assert_eq!(read_embedder_marker(&sidecar), None);
}

#[test]
fn the_generation_marker_is_untouched_by_the_embedder_marker() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("embeddings.bin");
    write_generation_marker(&sidecar, 42).unwrap();
    let before = std::fs::read(dir.path().join("embeddings.bin.generation")).unwrap();

    write_embedder_marker(&sidecar, "model2vec").unwrap();

    let after = std::fs::read(dir.path().join("embeddings.bin.generation")).unwrap();
    assert_eq!(before, after);
    assert_eq!(before.len(), 8, "the generation marker stays 8 bytes");
    assert_eq!(read_generation_marker(&sidecar), Some(42));
}

/// What the marker says after a write, from what it said before and how much
/// of the file this embedder produced.
#[test]
fn the_marker_names_one_embedder_only_when_every_vector_came_from_it() {
    // Every vector re-embedded: this embedder, whatever was recorded.
    assert_eq!(
        embedder_marker_after(Some("trigram"), "model2vec", 10, 10).as_deref(),
        Some("model2vec")
    );
    assert_eq!(
        embedder_marker_after(None, "trigram", 10, 10).as_deref(),
        Some("trigram")
    );
    // Some kept, same embedder as before: unchanged.
    assert_eq!(
        embedder_marker_after(Some("model2vec"), "model2vec", 2, 10).as_deref(),
        Some("model2vec")
    );
    // Some kept from a different embedder: the file is mixed.
    assert_eq!(
        embedder_marker_after(Some("model2vec"), "trigram", 2, 10).as_deref(),
        Some("mixed")
    );
    assert_eq!(
        embedder_marker_after(Some("mixed"), "model2vec", 2, 10).as_deref(),
        Some("mixed")
    );
    // Some kept and nothing recorded about them: unknown stays unknown.
    assert_eq!(embedder_marker_after(None, "trigram", 2, 10), None);
}
