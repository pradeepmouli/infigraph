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

/// Stops the daemon `infigraph index --full` auto-started under
/// `INFIGRAPH_BACKEND=daemon`, whether the test passes or panics.
struct StopDaemonOnDrop {
    root: std::path::PathBuf,
    home: std::path::PathBuf,
}

impl Drop for StopDaemonOnDrop {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_infigraph"))
            .arg("--root")
            .arg(&self.root)
            .arg("daemon-stop")
            .env("HOME", &self.home)
            .output();
    }
}

/// #100 follow-up: a daemon-routed `index --full` (the only kind under
/// `INFIGRAPH_BACKEND=daemon`, and what every auto-recovery rebuild is)
/// returned before the registration step, so a project rebuilt that way
/// never came back into `registry.json` -- `doctor` then FAILed its
/// registration check forever (sittir, 2026-09-02, after seven rebuilds).
#[test]
fn daemon_routed_full_index_registers_repo_in_local_registry() {
    let project = tempfile::tempdir().expect("failed to create project temp dir");
    let fake_home = tempfile::tempdir().expect("failed to create fake home temp dir");
    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n")
        .expect("failed to write fixture file");
    let _stop = StopDaemonOnDrop {
        root: project.path().to_path_buf(),
        home: fake_home.path().to_path_buf(),
    };

    // The real scenario: an already-indexed project whose registry entry
    // is gone (a pruned registry.json) and whose next rebuild goes through
    // the daemon. A plain local index first so `.infigraph/` exists, then
    // drop the registry it just wrote.
    let first = Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .arg("--root")
        .arg(project.path())
        .arg("index")
        .arg("--no-embed")
        .env("HOME", fake_home.path())
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .expect("failed to run the initial infigraph index");
    if !first.status.success() && first.stdout.is_empty() && first.stderr.is_empty() {
        eprintln!("Skipping: infigraph binary could not execute in this environment");
        return;
    }
    assert!(first.status.success(), "initial index failed");
    let registry_path = fake_home.path().join(".infigraph").join("registry.json");
    let _ = std::fs::remove_file(&registry_path);

    let output = Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .arg("--root")
        .arg(project.path())
        .arg("index")
        .arg("--full")
        .arg("--no-embed")
        .env("HOME", fake_home.path())
        .env("INFIGRAPH_BACKEND", "daemon")
        .env_remove("INFIGRAPH_NO_WATCH")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "infigraph index --full (daemon) failed: stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("full reindex"),
        "expected the daemon-routed full-reindex branch to run: stdout={stdout}\nstderr={stderr}"
    );

    let registry_content = std::fs::read_to_string(&registry_path).unwrap_or_default();
    let canonical =
        std::fs::canonicalize(project.path()).unwrap_or_else(|_| project.path().to_path_buf());
    let repo_name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    assert!(
        registry_content.contains(&format!("\"{repo_name}\"")),
        "registry.json has no entry for '{repo_name}' after a daemon-routed `index --full`:\n{registry_content}\nstdout={stdout}\nstderr={stderr}"
    );
}
