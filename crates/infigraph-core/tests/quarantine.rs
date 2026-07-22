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

/// Companion to the test above: pre-seeds FILE-type (not directory-type)
/// old quarantine entries, matching how this codebase actually produces
/// them -- Kuzu's on-disk graph is typically a single FILE at `db_path`
/// (see `wipe_graph`'s doc comment in lib.rs), so the realistic quarantine
/// target is a file, not a directory. `remove_dir_all` errors (and does NOT
/// delete) when given a plain file, so eviction must dispatch on the actual
/// entry type rather than assuming a directory.
#[test]
fn quarantine_evicts_oldest_file_type_beyond_bound_of_two() {
    let dir = tempfile::tempdir().unwrap();
    let ig = dir.path().join(".infigraph");
    fs::create_dir_all(&ig).unwrap();

    // Pre-seed two FILE-type quarantine entries with distinct, ordered
    // timestamps so eviction order is deterministic.
    fs::write(ig.join("graph.corrupt.100"), b"old-1").unwrap();
    fs::write(ig.join("graph.corrupt.200"), b"old-2").unwrap();

    fs::write(ig.join("graph"), b"third").unwrap();
    let third = quarantine_graph(&ig, "graph").unwrap();

    assert!(
        !ig.join("graph.corrupt.100").exists(),
        "oldest quarantine FILE (100) must be evicted to keep the bound at N=2"
    );
    assert!(
        ig.join("graph.corrupt.200").exists(),
        "newer pre-existing quarantine file (200) must survive"
    );
    assert!(third.exists(), "the newly quarantined file must exist");
    assert!(
        third.is_file(),
        "the newly quarantined entry must be a file, matching the source"
    );

    let remaining: Vec<_> = fs::read_dir(&ig)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("graph.corrupt.")
        })
        .collect();
    assert_eq!(
        remaining.len(),
        2,
        "quarantine pool must never exceed N=2 even when entries are files, not directories"
    );
}

/// Exercises WAL-sibling quarantine end to end: a base graph FILE plus a
/// `.wal` sibling and a `.wal.checkpoint`-style sibling must all end up
/// quarantined (not left behind at their original paths, where a caller's
/// post-quarantine fallback cleanup would destroy them) with their content
/// byte-for-byte intact.
#[test]
fn quarantine_moves_wal_family_siblings_with_content_intact() {
    let dir = tempfile::tempdir().unwrap();
    let ig = dir.path().join(".infigraph");
    fs::create_dir_all(&ig).unwrap();

    fs::write(ig.join("graph"), b"base graph content").unwrap();
    fs::write(ig.join("graph.wal"), b"wal content").unwrap();
    fs::write(ig.join("graph.wal.checkpoint"), b"checkpoint content").unwrap();

    let quarantine_path = quarantine_graph(&ig, "graph").unwrap();

    // Originals must be gone from their live paths -- otherwise a caller's
    // unconditional post-quarantine cleanup would delete them there,
    // destroying evidence rather than preserving it.
    assert!(
        !ig.join("graph").exists(),
        "base graph must be gone from its live path"
    );
    assert!(
        !ig.join("graph.wal").exists(),
        "wal sibling must be gone from its live path"
    );
    assert!(
        !ig.join("graph.wal.checkpoint").exists(),
        "wal checkpoint sibling must be gone from its live path"
    );

    // Base graph quarantined with content intact.
    assert!(
        quarantine_path.exists(),
        "quarantined base graph must exist"
    );
    assert_eq!(
        fs::read(&quarantine_path).unwrap(),
        b"base graph content",
        "quarantined base graph content must be preserved"
    );

    // WAL-family siblings quarantined alongside it (as sibling files sharing
    // its "graph.corrupt.<ts>" stem, since quarantine_path is a plain file
    // here and siblings can't be nested "inside" a file) with content intact.
    let stem = quarantine_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let wal_dest = ig.join(format!("{stem}.wal"));
    let checkpoint_dest = ig.join(format!("{stem}.wal.checkpoint"));

    assert!(wal_dest.exists(), "quarantined .wal sibling must exist");
    assert_eq!(
        fs::read(&wal_dest).unwrap(),
        b"wal content",
        "quarantined .wal sibling content must be preserved"
    );

    assert!(
        checkpoint_dest.exists(),
        "quarantined .wal.checkpoint sibling must exist"
    );
    assert_eq!(
        fs::read(&checkpoint_dest).unwrap(),
        b"checkpoint content",
        "quarantined .wal.checkpoint sibling content must be preserved"
    );
}
