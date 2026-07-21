use infigraph_core::embed;

/// Forge an embeddings.bin whose count header claims `count` vectors.
/// embedding_count reads only the leading LE u32, so a bare header is
/// enough (format per save_embeddings: [count:u32][entries...]).
fn write_embeddings_header(dir: &std::path::Path, count: u32) {
    let tg = dir.join(".infigraph");
    std::fs::create_dir_all(&tg).unwrap();
    std::fs::write(tg.join("embeddings.bin"), count.to_le_bytes()).unwrap();
}

#[test]
fn hnsw_gap_only_above_threshold() {
    let dir = tempfile::tempdir().unwrap();

    // No embeddings at all: not degraded.
    assert!(!embed::hnsw_expected_but_missing(dir.path()));

    // Below threshold: linear scan is the *designed* fast path, not a gap.
    write_embeddings_header(dir.path(), 1_000);
    assert!(!embed::hnsw_expected_but_missing(dir.path()));

    // At threshold with no index file: degraded.
    write_embeddings_header(dir.path(), embed::HNSW_THRESHOLD as u32);
    assert!(embed::hnsw_expected_but_missing(dir.path()));

    // Index file present: healthy again.
    std::fs::write(
        dir.path().join(".infigraph").join("hnsw_index.usearch"),
        b"stub",
    )
    .unwrap();
    assert!(!embed::hnsw_expected_but_missing(dir.path()));
}

#[test]
fn trigram_flag_defaults_false_and_latches() {
    // Default: no fallback observed (this test binary never inits an
    // embedder before this point).
    assert!(!embed::trigram_fallback_active());
    embed::note_trigram_fallback();
    assert!(embed::trigram_fallback_active());
}
