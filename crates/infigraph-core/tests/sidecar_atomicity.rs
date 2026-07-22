use infigraph_core::embed::build_hnsw_index;
use infigraph_core::search::BM25Index;

#[test]
fn bm25_save_leaves_no_temp_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bm25_cache.bin");
    let mut idx = BM25Index::default();
    idx.add_document("doc1".to_string(), "hello world".to_string());
    idx.save(&path).unwrap();
    assert!(path.exists());
    assert!(!dir.path().join("bm25_cache.bin.tmp").exists());
    let loaded = BM25Index::load(&path).unwrap();
    assert_eq!(loaded.doc_count(), 1);
}

#[test]
fn hnsw_build_leaves_no_temp_files_behind() {
    let dir = tempfile::tempdir().unwrap();
    let embeddings_path = dir.path().join("embeddings.bin");
    let index_path = dir.path().join("hnsw_index.usearch");
    let embeddings = vec![
        ("a".to_string(), vec![1.0_f32, 0.0, 0.0, 0.0]),
        ("b".to_string(), vec![0.0_f32, 1.0, 0.0, 0.0]),
    ];
    infigraph_core::embed::save_embeddings(&embeddings_path, &embeddings).unwrap();
    let n = build_hnsw_index(&embeddings, &index_path, &embeddings_path).unwrap();
    assert_eq!(n, 2);
    assert!(index_path.exists());
    let meta_path = index_path.with_extension("meta");
    assert!(meta_path.exists());
    assert!(!dir.path().join("hnsw_index.usearch.tmp").exists());
    assert!(!dir.path().join("hnsw_index.meta.tmp").exists());
}
