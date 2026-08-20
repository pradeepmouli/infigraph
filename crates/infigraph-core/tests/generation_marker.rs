use infigraph_core::embed::{read_generation_marker, write_generation_marker};

#[test]
fn round_trips_a_real_generation() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("embeddings.bin");
    std::fs::write(&sidecar, b"fake-sidecar-content").unwrap();

    write_generation_marker(&sidecar, 42).unwrap();

    assert_eq!(read_generation_marker(&sidecar), Some(42));
}

#[test]
fn overwriting_replaces_the_previous_value() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("embeddings.bin");

    write_generation_marker(&sidecar, 1).unwrap();
    write_generation_marker(&sidecar, 2).unwrap();

    assert_eq!(read_generation_marker(&sidecar), Some(2));
}

#[test]
fn zero_or_negative_generation_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("embeddings.bin");

    // 0 is GraphBackend::current_generation's sentinel for "unsupported/
    // unknown" -- writing a marker for it would make a later staleness
    // check falsely conclude the sidecar is behind every real generation.
    write_generation_marker(&sidecar, 0).unwrap();
    assert_eq!(read_generation_marker(&sidecar), None);

    write_generation_marker(&sidecar, -1).unwrap();
    assert_eq!(read_generation_marker(&sidecar), None);
}

#[test]
fn missing_marker_reads_as_none_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("embeddings.bin");

    assert_eq!(read_generation_marker(&sidecar), None);
}

#[test]
fn malformed_marker_file_reads_as_none() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("embeddings.bin");
    // Fewer than the 8 bytes a real marker holds.
    std::fs::write(sidecar.with_file_name("embeddings.bin.generation"), b"bad").unwrap();

    assert_eq!(read_generation_marker(&sidecar), None);
}

#[test]
fn marker_lives_beside_the_sidecar_not_inside_it() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("embeddings.bin");

    write_generation_marker(&sidecar, 7).unwrap();

    assert!(
        !sidecar.exists(),
        "write_generation_marker must not create the sidecar file itself"
    );
    assert!(dir.path().join("embeddings.bin.generation").exists());
}
