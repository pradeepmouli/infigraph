//! Regression test for task-3-review.md Finding 1: `infigraph index` on a
//! brand-new, never-before-indexed project must not print a spurious
//! `[auto-watch] Failed to start watcher: ...` line to stderr.
//!
//! `ensure_watcher_running` (crates/infigraph-cli/src/index.rs) is called
//! from `main.rs` *before* `cmd_index` creates `.infigraph`, so on the very
//! first `infigraph index` run, `.infigraph` doesn't exist yet at the point
//! the auto-watch check runs. That's an expected precondition-not-met
//! state, not an actionable failure — it must stay silent (matching the
//! pre-daemon-primitive behavior), not surface as a "Failed" message.

use std::process::Command;

#[test]
fn first_ever_index_on_fresh_project_does_not_print_watcher_failure() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    assert!(
        !tmp.path().join(".infigraph").exists(),
        "precondition: project must be genuinely unindexed"
    );

    // Minimal source file so indexing has something to do.
    std::fs::write(tmp.path().join("hello.py"), "def hello():\n    pass\n")
        .expect("failed to write fixture file");

    let output = Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .arg("--root")
        .arg(tmp.path())
        .arg("index")
        .arg("--no-embed")
        // Explicitly clear CI/opt-out env vars so this test exercises the
        // real "not yet indexed" path regardless of the ambient environment
        // (e.g. real CI runners set CI=true, which would otherwise make
        // ensure_daemon_running short-circuit before ever reaching the
        // `.infigraph`-missing check this test targets).
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("JENKINS_URL")
        .env_remove("BUILDKITE")
        .env_remove("GITLAB_CI")
        .env_remove("INFIGRAPH_NO_WATCH")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .expect("failed to run infigraph index");

    if !output.status.success() && output.stdout.is_empty() && output.stderr.is_empty() {
        // Binary failed to run at all (e.g. missing DLLs on some CI images)
        // — not what this test is checking, skip rather than false-fail.
        eprintln!("Skipping: infigraph binary could not execute in this environment");
        return;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Failed to start watcher"),
        "first-ever `infigraph index` on a fresh project must not print an \
         auto-watch failure — .infigraph not existing yet is expected, not \
         a failure. stderr was:\n{stderr}\nstdout was:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
