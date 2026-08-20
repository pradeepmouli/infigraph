//! R4.3.1 (#77): a single bad file must neither abort a full index run
//! nor be silently skipped -- it lands in `IndexResult.skipped_errors`
//! while every healthy file still indexes.

#![cfg(unix)]

use infigraph_languages::bundled_registry;
use std::os::unix::fs::PermissionsExt;

#[test]
fn unreadable_file_is_reported_not_silently_skipped_and_does_not_abort() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("good.py"), "def fine():\n    pass\n").unwrap();
    let bad = tmp.path().join("bad.py");
    std::fs::write(&bad, "def broken():\n    pass\n").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();

    let mut prism =
        infigraph_core::Infigraph::open(tmp.path(), bundled_registry().unwrap()).unwrap();
    prism.init().unwrap();
    let result = prism.index().expect("one bad file must not abort the run");

    // Restore permissions so the tempdir can be cleaned up.
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert!(
        result.extractions.iter().any(|e| e.file == "good.py"),
        "healthy files must still index: {:?}",
        result
            .extractions
            .iter()
            .map(|e| &e.file)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        result.skipped_errors.len(),
        1,
        "the unreadable file must be reported: {:?}",
        result.skipped_errors
    );
    assert!(
        result.skipped_errors[0].starts_with("bad.py:"),
        "the report names the file and reason: {:?}",
        result.skipped_errors
    );
}
