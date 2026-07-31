//! Regression test: `infigraph index` must register the project in
//! ~/.infigraph/registry.json (via `Registry::register_repo`) on the
//! default local build, not only under the `remote` + Neo4j feature
//! combination. Without this, `infigraph doctor`'s registry check
//! permanently FAILs for any CLI-only-indexed solo project, and its own
//! remediation text ("run `infigraph index <path>` to re-register it")
//! silently does nothing.

use std::process::Command;

#[test]
fn index_registers_repo_in_local_registry() {
    let project = tempfile::tempdir().expect("failed to create project temp dir");
    let fake_home = tempfile::tempdir().expect("failed to create fake home temp dir");

    // Minimal source file so indexing has something to do.
    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n")
        .expect("failed to write fixture file");

    let output = Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .arg("--root")
        .arg(project.path())
        .arg("index")
        .arg("--no-embed")
        .env("HOME", fake_home.path())
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("JENKINS_URL")
        .env_remove("BUILDKITE")
        .env_remove("GITLAB_CI")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .expect("failed to run infigraph index");

    if !output.status.success() && output.stdout.is_empty() && output.stderr.is_empty() {
        // Binary failed to run at all (e.g. missing DLLs on some CI images)
        // — not what this test is checking, skip rather than false-fail.
        eprintln!("Skipping: infigraph binary could not execute in this environment");
        return;
    }

    assert!(
        output.status.success(),
        "infigraph index failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let registry_path = fake_home.path().join(".infigraph").join("registry.json");
    let registry_content = std::fs::read_to_string(&registry_path).unwrap_or_else(|e| {
        panic!("expected registry.json to exist at {registry_path:?} after `infigraph index`: {e}")
    });

    let canonical =
        std::fs::canonicalize(project.path()).unwrap_or_else(|_| project.path().to_path_buf());
    let repo_name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    assert!(
        registry_content.contains(&format!("\"{repo_name}\"")),
        "registry.json does not contain an entry for '{repo_name}' after `infigraph index`:\n{registry_content}"
    );
}
