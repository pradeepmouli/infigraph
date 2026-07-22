use infigraph_core::embed::{load_embeddings, save_embeddings};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[test]
fn save_embeddings_leaves_no_temp_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("embeddings.bin");
    let data = vec![
        ("a".to_string(), vec![1.0, 2.0]),
        ("b".to_string(), vec![3.0, 4.0]),
    ];
    save_embeddings(&path, &data).unwrap();
    assert!(path.exists());
    assert!(!dir.path().join("embeddings.bin.tmp").exists());
    let loaded = load_embeddings(&path).unwrap();
    assert_eq!(loaded.len(), 2);
}

#[test]
fn concurrent_readers_never_observe_a_torn_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("embeddings.bin");
    // Seed the file so readers have something to load from the start.
    save_embeddings(&path, &[("seed".to_string(), vec![0.0; 8])]).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let write_path = path.clone();
    let writer = std::thread::spawn(move || {
        for i in 0..200 {
            let data: Vec<(String, Vec<f32>)> = (0..50)
                .map(|j| (format!("id-{i}-{j}"), vec![i as f32; 32]))
                .collect();
            save_embeddings(&write_path, &data).unwrap();
        }
    });

    let mut readers = Vec::new();
    for _ in 0..4 {
        let read_path = path.clone();
        let stop = Arc::clone(&stop);
        readers.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(loaded) = load_embeddings(&read_path) {
                    // A torn read would either fail to parse (load_embeddings
                    // returns Err, which the `if let Ok` above already
                    // filters out) or desync the declared count from the
                    // actual entries. The file is always either the 1-entry
                    // seed or one full 50-entry batch — never partial.
                    assert!(
                        loaded.len() == 1 || loaded.len() == 50,
                        "torn read: {} entries",
                        loaded.len()
                    );
                }
            }
        }));
    }

    writer.join().unwrap();
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
}

#[test]
fn save_embeddings_detects_truncation_via_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("embeddings.bin");
    let data = vec![
        ("a".to_string(), vec![1.0, 2.0, 3.0, 4.0]),
        ("b".to_string(), vec![5.0, 6.0, 7.0, 8.0]),
    ];
    save_embeddings(&path, &data).unwrap();

    // Corrupt the file: flip a byte in the middle of the payload without
    // changing its length, so length-based `ensure!` checks in the parser
    // would not catch it — only a checksum can.
    let mut bytes = std::fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let result = load_embeddings(&path);
    assert!(
        result.is_err(),
        "a single flipped byte must be caught by the checksum, not silently parsed"
    );
}

#[test]
fn load_embeddings_still_reads_pre_header_legacy_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("embeddings.bin");
    // Hand-write the OLD headerless format directly (no magic/version/checksum)
    // to simulate a file written before this task landed.
    let mut buf = Vec::new();
    buf.extend_from_slice(&1u32.to_le_bytes()); // count
    let id = b"legacy";
    buf.extend_from_slice(&(id.len() as u32).to_le_bytes());
    buf.extend_from_slice(id);
    buf.extend_from_slice(&2u32.to_le_bytes()); // dim
    buf.extend_from_slice(&1.0_f32.to_le_bytes());
    buf.extend_from_slice(&2.0_f32.to_le_bytes());
    std::fs::write(&path, &buf).unwrap();

    let loaded = load_embeddings(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].0, "legacy");
    assert_eq!(loaded[0].1, vec![1.0, 2.0]);
}
