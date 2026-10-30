//! A full rebuild wiped `.infigraph/` before its open could fail, so a refused
//! open (another process held the graph) left the live store gone.

mod support;

fn local_command(
    root: &std::path::Path,
    home: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    // The command as a user's shell runs it: no test switch suppresses the
    // auto-watch, and the CI markers are cleared so nothing else does either.
    support::infigraph()
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .env_remove("INFIGRAPH_NO_WATCH")
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("JENKINS_URL")
        .env_remove("BUILDKITE")
        .env_remove("GITLAB_CI")
        .output()
        .expect("failed to run infigraph")
}

fn project() -> (tempfile::TempDir, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("hello.py"), "def hello():\n    pass\n").unwrap();
    (root, tempfile::tempdir().unwrap())
}

/// A rebuild whose graph is held by another process is refused before anything
/// is wiped: the lock is the only thing wrong, so the store must survive it.
#[test]
fn a_full_rebuild_refused_on_a_held_lock_leaves_the_store_untouched() {
    let (root, home) = project();
    support::assert_ok(
        &support::run(
            root.path(),
            home.path(),
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "first index",
    );
    let graph = root
        .path()
        .join(".infigraph")
        .join(infigraph_core::graph::names::STEM);
    let held = infigraph_core::graph::GraphStore::open(&graph).expect("hold the graph");
    let snapshots = root.path().join(".infigraph").join("snapshots");
    let count = |dir: &std::path::Path| std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0);
    let snapshots_before = count(&snapshots);

    let out = local_command(root.path(), home.path(), &["index", "--full", "--no-embed"]);
    drop(held);
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "a held graph must refuse the rebuild: {stderr}"
    );
    assert!(
        stderr.contains("locked by another infigraph process"),
        "{stderr}"
    );
    assert!(
        graph.exists(),
        "the refused rebuild must not have removed the graph"
    );
    assert_eq!(
        count(&snapshots),
        snapshots_before,
        "a rebuild that never started takes no snapshot"
    );
}
