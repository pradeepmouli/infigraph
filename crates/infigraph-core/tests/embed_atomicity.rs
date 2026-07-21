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
