//! `[scip] disable`: a name no indexer serves is refused by `infigraph index`
//! before any work, and the message names what is accepted.

mod support;

use std::fs;

#[test]
fn index_refuses_a_disable_name_no_indexer_serves() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join(".infigraph")).unwrap();
    fs::write(
        project.path().join(".infigraph/config.toml"),
        "[scip]\ndisable = [\"rustt\"]\n",
    )
    .unwrap();
    fs::write(project.path().join("a.rs"), "fn a() {}\n").unwrap();

    let home = tempfile::tempdir().unwrap();
    let out = support::infigraph()
        .arg("--root")
        .arg(project.path())
        .arg("index")
        .arg("--no-embed")
        .env("HOME", home.path())
        .env("INFIGRAPH_NO_WATCH", "1")
        .output()
        .unwrap();

    assert!(!out.status.success(), "a typo in disable was accepted");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("rustt"), "{err}");
    assert!(err.contains("rust"), "accepted names not listed: {err}");
}
