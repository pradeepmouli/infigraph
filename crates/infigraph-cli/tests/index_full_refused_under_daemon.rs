//! `infigraph index --full` under `INFIGRAPH_BACKEND=daemon` must refuse
//! loudly rather than wipe `.infigraph/graph` unlocked out from under a
//! daemon that may hold a persistent connection on it. See
//! https://github.com/pradeepmouli/infigraph/issues/50.

use std::process::Command;

#[test]
fn full_reindex_refuses_under_daemon_backend_without_wiping_graph() {
    let project = tempfile::tempdir().expect("failed to create project temp dir");
    let fake_home = tempfile::tempdir().expect("failed to create fake home temp dir");

    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n")
        .expect("failed to write fixture file");

    // Seed a fake existing graph directory with a marker file, so a wipe
    // (which this test must prove does NOT happen) is directly observable.
    let graph_dir = project.path().join(".infigraph").join("graph");
    std::fs::create_dir_all(&graph_dir).expect("failed to create fake graph dir");
    let marker = graph_dir.join("marker.txt");
    std::fs::write(&marker, "pre-existing graph data").expect("failed to write marker file");

    let output = Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .arg("--root")
        .arg(project.path())
        .arg("index")
        .arg("--full")
        .arg("--no-embed")
        .env("HOME", fake_home.path())
        .env("INFIGRAPH_NO_WATCH", "1")
        .env("INFIGRAPH_BACKEND", "daemon")
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("JENKINS_URL")
        .env_remove("BUILDKITE")
        .env_remove("GITLAB_CI")
        .output()
        .expect("failed to run infigraph index --full");

    if !output.status.success() && output.stdout.is_empty() && output.stderr.is_empty() {
        eprintln!("Skipping: infigraph binary could not execute in this environment");
        return;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "expected `infigraph index --full` to refuse under INFIGRAPH_BACKEND=daemon, but it succeeded:\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        stderr.contains("not yet supported under the daemon backend"),
        "expected the daemon-mode refusal message, got:\n{stderr}"
    );
    assert!(
        stderr.contains("issues/50"),
        "expected the refusal to point at the tracked follow-up issue, got:\n{stderr}"
    );

    assert!(
        marker.exists(),
        "the pre-existing graph marker file was deleted -- the wipe ran despite the daemon-mode refusal"
    );
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "pre-existing graph data",
        "the marker file's content changed -- something touched the graph dir"
    );
}
