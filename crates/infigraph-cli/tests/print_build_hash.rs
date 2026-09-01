use std::process::Command;

fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_infigraph")
}

#[test]
fn prints_the_real_build_hash_by_default() {
    let output = Command::new(cli_bin())
        .arg("print-build-hash")
        .env_remove("INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE")
        .output()
        .expect("failed to run infigraph print-build-hash");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), infigraph_core::build_hash());
}

#[test]
fn prints_the_override_file_contents_when_set() {
    let dir = tempfile::tempdir().unwrap();
    let override_path = dir.path().join("fake-hash.txt");
    std::fs::write(&override_path, "fake-build-hash-123\n").unwrap();

    let output = Command::new(cli_bin())
        .arg("print-build-hash")
        .env("INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE", &override_path)
        .output()
        .expect("failed to run infigraph print-build-hash");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "fake-build-hash-123");
}
