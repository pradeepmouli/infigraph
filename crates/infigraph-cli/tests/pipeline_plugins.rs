//! Pipeline plugins during `infigraph index-docs`, end to end through the real
//! binary: a user-level plugin (an `sh` script speaking the line protocol)
//! under a pinned `HOME`, documents in a temp project, and the `pipeline`
//! subcommands reading back what the plugin extracted.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{assert_ok, run, start_daemon, stdout};

const DAEMON: &str = "daemon";

/// A project (with a code file, so the daemon's bootstrap index has
/// something to index) and a scratch `HOME`.
fn project() -> (tempfile::TempDir, tempfile::TempDir) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n").unwrap();
    (project, home)
}

fn write(root: &Path, rel: &str, text: &str) {
    let full = root.join(rel);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

/// A user-level plugin `id`. Its process appends a line to `starts` each
/// time it starts, and answers every document with a pipeline named by the
/// document's `name=`, reading `in=` and `out=`.
fn install_plugin(home: &Path, id: &str, patterns: &[&str], starts: &Path) {
    let dir = home.join(".infigraph/pipelines").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("extract.sh"),
        format!(
            r#"#!/bin/sh
echo started >> '{}'
echo '{{"ready":true}}'
while IFS= read -r line; do
  name=$(printf '%s' "$line" | sed -n 's/.*name=\([a-z0-9_]*\).*/\1/p')
  in=$(printf '%s' "$line" | sed -n 's/.*in=\([a-z0-9_]*\).*/\1/p')
  out=$(printf '%s' "$line" | sed -n 's/.*out=\([a-z0-9_]*\).*/\1/p')
  echo "{{\"status\":\"ok\",\"data\":{{\"core\":{{\"name\":\"$name\",\"inputs\":[\"$in\"],\"outputs\":[\"$out\"]}},\"properties\":{{\"owner\":\"o_$name\"}}}}}}"
done
"#,
            starts.display()
        ),
    )
    .unwrap();
    let patterns: Vec<String> = patterns.iter().map(|p| format!("{p:?}")).collect();
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "[plugin]\nname = \"{id}\"\nplugin_id = \"{id}\"\ncommand = [\"sh\", \"extract.sh\"]\n\
             detect_patterns = [{}]\n\n[[plugin.schema]]\nname = \"owner\"\ncol_type = \"STRING\"\n",
            patterns.join(", ")
        ),
    )
    .unwrap();
}

fn starts(file: &Path) -> usize {
    std::fs::read_to_string(file).map_or(0, |t| t.lines().count())
}

fn query(root: &Path, home: &Path, backend: &str, owner: &str) -> String {
    let out = run(
        root,
        home,
        backend,
        &["pipeline", "query", "fake", "owner", owner],
    );
    assert_ok(&out, "pipeline query");
    stdout(&out)
}

fn deps(root: &Path, home: &Path, backend: &str) -> String {
    let out = run(root, home, backend, &["pipeline", "deps"]);
    assert_ok(&out, "pipeline deps");
    stdout(&out)
}

fn index_docs(root: &Path, home: &Path, backend: &str) -> String {
    let out = run(root, home, backend, &["index-docs"]);
    assert_ok(&out, "index-docs");
    stdout(&out)
}

/// The whole life of a document's pipeline, under `backend`: extracted,
/// linked into the dependency graph, not re-extracted when nothing changed,
/// replaced when the document renames its pipeline, gone when the document is.
fn pipeline_lifecycle(backend: &str) {
    let (project, home) = project();
    let (root, home) = (project.path(), home.path());
    let counter = tempfile::tempdir().unwrap();
    let starts_file = counter.path().join("starts");
    install_plugin(home, "fake", &["name="], &starts_file);
    write(root, "docs/alpha.md", "# A\nname=alpha in=raw out=clean\n");
    write(root, "docs/beta.md", "# B\nname=beta in=clean out=final\n");
    write(root, "README.md", "# Readme\nplain prose\n");
    let _daemon = (backend == DAEMON).then(|| start_daemon(root, home));

    index_docs(root, home, backend);
    assert!(
        query(root, home, backend, "o_alpha").contains("1 results"),
        "alpha was not stored"
    );
    assert!(query(root, home, backend, "o_beta").contains("1 results"));
    let graph = deps(root, home, backend);
    assert!(graph.contains("beta → alpha"), "{graph}");
    let started = starts(&starts_file);
    assert!(started >= 1, "the plugin never started");

    // Nothing changed: no document is offered, so no plugin starts.
    index_docs(root, home, backend);
    assert_eq!(starts(&starts_file), started, "an unchanged run spawned");

    // The document renames its pipeline: the old rows are gone.
    write(root, "docs/alpha.md", "# A\nname=gamma in=raw out=clean\n");
    index_docs(root, home, backend);
    assert!(query(root, home, backend, "o_alpha").contains("No results"));
    assert!(query(root, home, backend, "o_gamma").contains("1 results"));

    // The document is deleted: its pipeline goes with it.
    std::fs::remove_file(root.join("docs/beta.md")).unwrap();
    index_docs(root, home, backend);
    assert!(query(root, home, backend, "o_beta").contains("No results"));
    assert!(query(root, home, backend, "o_gamma").contains("1 results"));
}

#[test]
fn a_pipeline_is_extracted_replaced_and_deleted_under_the_daemon_backend() {
    pipeline_lifecycle(DAEMON);
}

#[test]
fn a_pipeline_is_extracted_replaced_and_deleted_in_process() {
    pipeline_lifecycle(infigraph_core::LOCAL_BACKEND);
}

/// `index_docs` returns early when no document file is left, so the stale
/// documents, and with them their pipelines, must still be pruned.
#[test]
fn deleting_the_last_document_removes_its_pipeline() {
    let (project, home) = project();
    let (root, home) = (project.path(), home.path());
    let counter = tempfile::tempdir().unwrap();
    install_plugin(home, "fake", &["name="], &counter.path().join("starts"));
    write(root, "only.md", "# Only\nname=alpha in=raw out=clean\n");
    let local = infigraph_core::LOCAL_BACKEND;

    index_docs(root, home, local);
    assert!(query(root, home, local, "o_alpha").contains("1 results"));

    std::fs::remove_file(root.join("only.md")).unwrap();
    index_docs(root, home, local);
    assert!(query(root, home, local, "o_alpha").contains("No results"));
}

/// The person who ran `index-docs` sees why a plugin did nothing, under the
/// daemon backend too, where the work happens in another process.
#[test]
fn a_failing_plugin_is_reported_to_the_person_who_ran_index_docs() {
    let (project, home) = project();
    let (root, home) = (project.path(), home.path());
    let dir = home.join(".infigraph/pipelines/broken");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "[plugin]\nname = \"broken\"\nplugin_id = \"broken\"\n\
         command = [\"/nonexistent/infigraph-test-plugin\"]\ndetect_patterns = [\"name=\"]\n",
    )
    .unwrap();
    write(root, "a.md", "# A\nname=alpha\n");
    write(root, "b.md", "# B\nname=beta\n");
    let _daemon = start_daemon(root, home);

    let report = index_docs(root, home, DAEMON);
    assert!(report.contains("Pipeline warnings (1)"), "{report}");
    assert!(report.contains("broken"), "{report}");
    assert!(report.contains("Files indexed: 2"), "{report}");
}

/// A project's own plugins arrive with the repository, so they run only for
/// a project the user listed in their own config.
#[test]
fn a_project_level_plugin_runs_only_once_the_user_trusts_the_project() {
    let (project, home) = project();
    let (root, home) = (project.path(), home.path());
    let root = &root.canonicalize().unwrap();
    let marker = root.join("plugin-ran");
    let dir = root.join("pipelines/mark");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "[plugin]\nname = \"mark\"\nplugin_id = \"mark\"\n\
             command = [\"sh\", \"-c\", \"touch '{}'; exit 1\"]\n\
             detect_patterns = [\"MARKME\"]\n",
            marker.display()
        ),
    )
    .unwrap();
    write(root, "m.md", "# M\nMARKME\n");
    let local = infigraph_core::LOCAL_BACKEND;

    index_docs(root, home, local);
    assert!(!marker.exists(), "an untrusted project's plugin ran");

    write(
        home,
        ".infigraph/config.toml",
        &format!(
            "[pipelines]\ntrusted_projects = [{:?}]\n",
            root.to_string_lossy()
        ),
    );
    write(root, "m.md", "# M\nMARKME again\n");
    index_docs(root, home, local);
    assert!(marker.exists(), "a trusted project's plugin did not run");
}
