use infigraph_core::embed::{
    fnv1a64, load_embeddings, load_embeddings_hashed, save_embeddings, save_embeddings_hashed,
};

fn entries() -> Vec<(String, Vec<f32>, u64)> {
    vec![
        (
            "a.py::foo".to_string(),
            vec![0.1f32, 0.2, 0.3],
            fnv1a64(b"foo-text"),
        ),
        ("a.py::bar".to_string(), vec![0.4, 0.5, 0.6], 0), // unknown hash is representable
    ]
}

#[test]
fn fnv1a64_is_stable_and_never_zero() {
    assert_eq!(fnv1a64(b"hello"), fnv1a64(b"hello"));
    assert_ne!(fnv1a64(b"hello"), fnv1a64(b"hellp"));
    assert_ne!(fnv1a64(b""), 0, "0 is reserved for 'unknown'");
}

#[test]
fn v3_roundtrip_preserves_ids_vectors_and_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("embeddings.bin");
    save_embeddings_hashed(&path, &entries()).unwrap();

    let data = std::fs::read(&path).unwrap();
    assert_eq!(&data[0..4], b"IGE1");
    assert_eq!(data[4], 3, "hashed save must write format version 3");

    let loaded = load_embeddings_hashed(&path).unwrap();
    assert_eq!(loaded, entries());

    // The plain loader reads v3 too, stripping hashes.
    let plain = load_embeddings(&path).unwrap();
    assert_eq!(plain.len(), 2);
    assert_eq!(plain[0].0, "a.py::foo");
    assert!((plain[0].1[1] - 0.2).abs() < 1e-6);
}

#[test]
fn v2_file_loads_with_unknown_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("embeddings.bin");
    let pairs = vec![("a.py::foo".to_string(), vec![0.1f32, 0.2])];
    save_embeddings(&path, &pairs).unwrap(); // still writes v2

    let data = std::fs::read(&path).unwrap();
    assert_eq!(data[4], 2, "plain save must keep writing v2");

    let loaded = load_embeddings_hashed(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].2, 0, "v2 entries carry 'unknown' hash");
}

#[test]
fn v3_checksum_detects_corruption() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("embeddings.bin");
    save_embeddings_hashed(&path, &entries()).unwrap();
    let mut data = std::fs::read(&path).unwrap();
    let mid = data.len() / 2;
    data[mid] ^= 0xFF;
    std::fs::write(&path, &data).unwrap();
    assert!(load_embeddings_hashed(&path).is_err());
    assert!(load_embeddings(&path).is_err());
}

#[test]
fn unknown_version_still_rejected_by_both_loaders() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("embeddings.bin");
    let mut data = b"IGE1".to_vec();
    data.push(9); // unknown version
    data.extend_from_slice(&1u32.to_le_bytes());
    std::fs::write(&path, &data).unwrap();
    assert!(load_embeddings(&path).is_err());
    assert!(load_embeddings_hashed(&path).is_err());
}

#[test]
fn embedding_count_reads_v3() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();
    let path = tmp.path().join(".infigraph").join("embeddings.bin");
    save_embeddings_hashed(&path, &entries()).unwrap();
    assert_eq!(infigraph_core::embed::embedding_count(tmp.path()), 2);
}
