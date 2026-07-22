use infigraph_core::quarantine::quarantine_graph;
use std::fs;

#[test]
fn quarantine_renames_instead_of_deleting() {
    let dir = tempfile::tempdir().unwrap();
    let ig = dir.path().join(".infigraph");
    fs::create_dir_all(ig.join("graph")).unwrap();
    fs::write(ig.join("graph").join("catalog.kz"), b"fake db content").unwrap();

    let quarantine_path = quarantine_graph(&ig, "graph").unwrap();

    assert!(
        !ig.join("graph").exists(),
        "original graph dir must be gone from its live path"
    );
    assert!(quarantine_path.exists(), "quarantine dir must exist");
    assert!(
        quarantine_path.join("catalog.kz").exists(),
        "quarantined content must be preserved, not deleted"
    );
    assert_eq!(
        fs::read(quarantine_path.join("catalog.kz")).unwrap(),
        b"fake db content"
    );
}

#[test]
fn quarantine_evicts_oldest_beyond_bound_of_two() {
    let dir = tempfile::tempdir().unwrap();
    let ig = dir.path().join(".infigraph");
    fs::create_dir_all(&ig).unwrap();

    // Pre-seed two quarantine dirs with distinct, ordered timestamps so
    // eviction order is deterministic regardless of wall-clock speed.
    fs::create_dir_all(ig.join("graph.corrupt.100")).unwrap();
    fs::create_dir_all(ig.join("graph.corrupt.200")).unwrap();

    fs::create_dir_all(ig.join("graph")).unwrap();
    fs::write(ig.join("graph").join("marker"), b"third").unwrap();
    let third = quarantine_graph(&ig, "graph").unwrap();

    assert!(
        !ig.join("graph.corrupt.100").exists(),
        "oldest quarantine dir (100) must be evicted to keep the bound at N=2"
    );
    assert!(
        ig.join("graph.corrupt.200").exists(),
        "newer pre-existing quarantine dir (200) must survive"
    );
    assert!(third.exists(), "the newly quarantined dir must exist");

    let remaining: Vec<_> = fs::read_dir(&ig)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("graph.corrupt.")
        })
        .collect();
    assert_eq!(remaining.len(), 2, "quarantine pool must never exceed N=2");
}
