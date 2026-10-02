use infigraph_core::embed;

/// Forge an embeddings.bin whose count header claims `count` vectors.
/// embedding_count reads only the leading LE u32, so a bare header is
/// enough. This constructs the legacy/pre-header format specifically
/// (`[count:u32][entries...]`, no magic/version) — `save_embeddings` itself
/// no longer produces this format since the magic+version+checksum header
/// was added, but `embedding_count`/`load_embeddings` must still read it.
fn write_embeddings_header(dir: &std::path::Path, count: u32) {
    let tg = dir.join(".infigraph");
    std::fs::create_dir_all(&tg).unwrap();
    std::fs::write(tg.join("embeddings.bin"), count.to_le_bytes()).unwrap();
}

#[test]
fn hnsw_gap_only_above_threshold() {
    let dir = tempfile::tempdir().unwrap();

    // No embeddings at all: not degraded.
    assert!(!embed::hnsw_expected_but_unusable(dir.path()));

    // Below threshold: linear scan is the *designed* fast path, not a gap.
    write_embeddings_header(dir.path(), 1_000);
    assert!(!embed::hnsw_expected_but_unusable(dir.path()));

    // At threshold with no index file: degraded.
    write_embeddings_header(dir.path(), embed::HNSW_THRESHOLD as u32);
    assert!(embed::hnsw_expected_but_unusable(dir.path()));

    // The index without its `.meta` is still a linear scan: `search_hnsw`
    // answers `None` when either file is absent (#75).
    std::fs::write(
        dir.path().join(".infigraph").join("hnsw_index.usearch"),
        b"stub",
    )
    .unwrap();
    assert!(embed::hnsw_expected_but_unusable(dir.path()));

    // A `.meta` naming some other `embeddings.bin` is a stale index: search
    // refuses it and scans linearly, so it is the same degradation.
    write_meta_naming(dir.path(), 1);
    assert!(embed::hnsw_expected_but_unusable(dir.path()));

    // Index and a `.meta` naming the current `embeddings.bin`: healthy again.
    write_meta_naming(dir.path(), embeddings_mtime_secs(dir.path()));
    assert!(!embed::hnsw_expected_but_unusable(dir.path()));
}

fn embeddings_mtime_secs(dir: &std::path::Path) -> u64 {
    std::fs::metadata(dir.join(".infigraph").join("embeddings.bin"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A binary sidecar header ([version][count][dim][emb_mtime_secs]) and no ids.
fn write_meta_naming(dir: &std::path::Path, emb_mtime_secs: u64) {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(&emb_mtime_secs.to_le_bytes());
    std::fs::write(dir.join(".infigraph").join("hnsw_index.meta"), bytes).unwrap();
}

#[test]
fn trigram_flag_defaults_false_and_latches() {
    // Default: no fallback observed (this test binary never inits an
    // embedder before this point).
    assert!(!embed::trigram_fallback_active());
    embed::note_trigram_fallback();
    assert!(embed::trigram_fallback_active());
}

/// Regression test for the header-format bug: `embedding_count` must be
/// magic-aware, not assume the first 4 bytes are always a raw count. Against
/// the buggy version this read the `b"IGE1"` magic as a little-endian u32
/// (~827 million) instead of the real count.
#[test]
fn embedding_count_reads_current_header_format_after_save() {
    let dir = tempfile::tempdir().unwrap();
    let tg = dir.path().join(".infigraph");
    std::fs::create_dir_all(&tg).unwrap();
    let path = tg.join("embeddings.bin");

    let embeddings = vec![
        ("a".to_string(), vec![1.0, 2.0]),
        ("b".to_string(), vec![3.0, 4.0]),
        ("c".to_string(), vec![5.0, 6.0]),
    ];
    embed::save_embeddings(&path, &embeddings).unwrap();

    assert_eq!(embed::embedding_count(dir.path()), 3);
}

/// `embedding_count` must still support the legacy headerless format
/// (pre-existing files written before the magic/version header existed).
#[test]
fn embedding_count_reads_legacy_headerless_format() {
    let dir = tempfile::tempdir().unwrap();
    write_embeddings_header(dir.path(), 42);
    assert_eq!(embed::embedding_count(dir.path()), 42);
}

/// A new-format file (magic present) with an unrecognized version byte must
/// be rejected, not silently misread as if the version byte were part of the
/// count — mirroring `load_embeddings`, which errors on a bad version via
/// the same shared header-offset helper. `embedding_count` has no `Result`
/// to propagate an error through, so it falls back to 0, the same value it
/// already returns for any other unreadable/corrupt file.
#[test]
fn embedding_count_rejects_unrecognized_version_byte() {
    let dir = tempfile::tempdir().unwrap();
    let tg = dir.path().join(".infigraph");
    std::fs::create_dir_all(&tg).unwrap();
    let path = tg.join("embeddings.bin");

    let embeddings = vec![
        ("a".to_string(), vec![1.0, 2.0]),
        ("b".to_string(), vec![3.0, 4.0]),
        ("c".to_string(), vec![5.0, 6.0]),
    ];
    embed::save_embeddings(&path, &embeddings).unwrap();
    assert_eq!(embed::embedding_count(dir.path()), 3);

    // Flip the version byte (offset 4, right after the 4-byte magic) to a
    // value that will never match EMBEDDINGS_FORMAT_VERSION.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[4] = 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    assert_eq!(embed::embedding_count(dir.path()), 0);
    // load_embeddings must reject the same file with an error, not a wrong count.
    assert!(embed::load_embeddings(&path).is_err());
}
