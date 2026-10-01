//! A SCIP output that cannot enrich anything is refused (#73), end to end
//! through the real `infigraph scip-import`, in process and through the
//! daemon. Importing one used to stamp the graph as enriched, so a rebuilt,
//! unenriched graph read as done and nothing retried it.

mod support;

use std::path::Path;

use support::{assert_ok, run, start_daemon};

/// An index with a metadata message and no documents: what `scip-python` and
/// `scip-go` write, exiting 0, for a project with none of their language.
const METADATA_ONLY_INDEX: &[u8] = &[0x0a, 0x00];

/// The same, plus one document for `hello.py`.
fn index_with_a_document() -> Vec<u8> {
    let mut bytes = vec![0x0a, 0x00, 0x12, 0x0a, 0x0a, 0x08];
    bytes.extend_from_slice(b"hello.py");
    bytes
}

/// A project whose code graph exists (SCIP has never run on it) and a
/// scratch HOME.
fn indexed_project() -> (tempfile::TempDir, tempfile::TempDir) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n").unwrap();
    assert_ok(
        &run(
            project.path(),
            home.path(),
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "index",
    );
    (project, home)
}

/// The graph's SCIP generation, read after the processes have gone (the
/// daemon variant stops its daemon first by dropping it).
fn scip_generation(root: &Path) -> i64 {
    infigraph_core::graph::GraphStore::open(&root.join(".infigraph").join("graph"))
        .unwrap()
        .current_scip_generation()
        .unwrap()
}

fn import(root: &Path, home: &Path, backend: &str, index: &Path) -> std::process::Output {
    run(
        root,
        home,
        backend,
        &["scip-import", "--index", &index.to_string_lossy()],
    )
}

#[test]
fn importing_a_metadata_only_index_fails_and_stamps_nothing() {
    let (project, home) = indexed_project();
    let (root, home) = (project.path(), home.path());
    let empty = root.join("empty.scip");
    std::fs::write(&empty, METADATA_ONLY_INDEX).unwrap();

    let out = import(root, home, infigraph_core::LOCAL_BACKEND, &empty);

    assert!(!out.status.success(), "an empty index was accepted");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no documents"), "{stderr}");
    assert_eq!(
        scip_generation(root),
        0,
        "a refused import stamped the graph"
    );
}

#[test]
fn importing_a_zero_byte_file_fails_with_its_own_message() {
    let (project, home) = indexed_project();
    let (root, home) = (project.path(), home.path());
    let empty = root.join("zero.scip");
    std::fs::write(&empty, b"").unwrap();

    let out = import(root, home, infigraph_core::LOCAL_BACKEND, &empty);

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("empty file"), "{stderr}");
    assert_eq!(scip_generation(root), 0);
}

#[test]
fn an_index_with_a_document_still_imports_and_stamps() {
    let (project, home) = indexed_project();
    let (root, home) = (project.path(), home.path());
    let good = root.join("good.scip");
    std::fs::write(&good, index_with_a_document()).unwrap();

    let out = import(root, home, infigraph_core::LOCAL_BACKEND, &good);

    assert_ok(&out, "scip-import of an index with a document");
    assert!(scip_generation(root) > 0, "a good import did not stamp");
}

/// The daemon runs the import; the refusal reaches the person who asked.
#[test]
fn the_daemon_refuses_a_metadata_only_index_and_says_why() {
    let (project, home) = indexed_project();
    let (root, home) = (project.path(), home.path());
    let empty = root.join("empty.scip");
    std::fs::write(&empty, METADATA_ONLY_INDEX).unwrap();
    let mut daemon = start_daemon(root, home);

    let out = import(root, home, "daemon", &empty);
    // Stop it the orderly way and wait for it to go, so the graph is closed
    // cleanly before this test reads the stamp.
    assert_ok(&run(root, home, "daemon", &["daemon-stop"]), "daemon-stop");
    daemon.0.wait().unwrap();

    assert!(!out.status.success(), "the daemon accepted an empty index");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no documents"), "{stderr}");
    assert_eq!(scip_generation(root), 0);
}
