use infigraph_core::clone::clone_infigraph_dir;
use std::fs;

fn write_file(path: &std::path::Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn clone_copies_graph_and_sidecars() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    write_file(&src.path().join(".infigraph/graph/data.kz"), "graph-bytes");
    write_file(&src.path().join(".infigraph/embeddings.bin"), "emb-bytes");
    write_file(
        &src.path().join(".infigraph/docs_embeddings.bin"),
        "doc-emb-bytes",
    );

    clone_infigraph_dir(src.path(), dst.path()).unwrap();

    assert_eq!(
        fs::read_to_string(dst.path().join(".infigraph/graph/data.kz")).unwrap(),
        "graph-bytes"
    );
    assert_eq!(
        fs::read_to_string(dst.path().join(".infigraph/embeddings.bin")).unwrap(),
        "emb-bytes"
    );
    assert_eq!(
        fs::read_to_string(dst.path().join(".infigraph/docs_embeddings.bin")).unwrap(),
        "doc-emb-bytes"
    );
}

#[test]
fn clone_excludes_lock_files_and_logs() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    write_file(&src.path().join(".infigraph/graph.lock"), "pid:123");
    write_file(&src.path().join(".infigraph/watch.lock"), "pid:123");
    write_file(&src.path().join(".infigraph/mcp.lock"), "pid:123");
    write_file(&src.path().join(".infigraph/index.lock"), "pid:123");
    write_file(&src.path().join(".infigraph/logs/watch.log"), "log line");
    write_file(&src.path().join(".infigraph/embeddings.bin"), "emb-bytes");

    clone_infigraph_dir(src.path(), dst.path()).unwrap();

    assert!(!dst.path().join(".infigraph/graph.lock").exists());
    assert!(!dst.path().join(".infigraph/watch.lock").exists());
    assert!(!dst.path().join(".infigraph/mcp.lock").exists());
    assert!(!dst.path().join(".infigraph/index.lock").exists());
    assert!(!dst.path().join(".infigraph/logs").exists());
    assert!(dst.path().join(".infigraph/embeddings.bin").exists());
}

#[test]
fn clone_leaves_source_untouched() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    write_file(&src.path().join(".infigraph/embeddings.bin"), "emb-bytes");

    clone_infigraph_dir(src.path(), dst.path()).unwrap();

    assert_eq!(
        fs::read_to_string(src.path().join(".infigraph/embeddings.bin")).unwrap(),
        "emb-bytes"
    );
}

#[test]
fn clone_errors_clearly_when_source_has_no_infigraph_dir() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    let err = clone_infigraph_dir(src.path(), dst.path()).unwrap_err();
    assert!(
        err.to_string().contains(".infigraph"),
        "error should mention .infigraph: {err}"
    );
}

/// `docs-op.lock`, the document store's identity lock and the config writer's
/// lock are locks like the rest: a copy would claim the destination is held
/// by the source's process. `config.toml` and the store itself do travel.
#[test]
fn clone_excludes_the_docs_and_config_locks() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    write_file(&src.path().join(".infigraph/docs-op.lock"), "pid:123");
    write_file(&src.path().join(".infigraph/docs.kuzu.lock"), "pid:123");
    write_file(&src.path().join(".infigraph/config.lock"), "pid:123");
    write_file(&src.path().join(".infigraph/config.toml"), "[docs]\n");
    write_file(&src.path().join(".infigraph/docs.kuzu"), "store");

    clone_infigraph_dir(src.path(), dst.path()).unwrap();

    for lock in ["docs-op.lock", "docs.kuzu.lock", "config.lock"] {
        assert!(
            !dst.path().join(".infigraph").join(lock).exists(),
            "{lock} was cloned"
        );
    }
    assert!(dst.path().join(".infigraph/config.toml").exists());
    assert!(dst.path().join(".infigraph/docs.kuzu").exists());
}
