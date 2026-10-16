//! `semantic_diff` judges both sides by the project's ignore rules: a ref is
//! extracted with `git archive` into a temp directory elsewhere, where the
//! project's untracked `.infigraph/config.toml` does not exist, so the rules
//! have to come from the project root.

use std::path::Path;
use std::process::Command;

use infigraph_languages::bundled_registry;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn a_diff_does_not_see_files_the_project_ignores_for_code() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());

    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, &["init", "-q"]);
    for rel in ["svc/a.py", "skipme/a.py"] {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "def work(x):\n    return x\n").unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "one"]);
    for rel in ["svc/a.py", "skipme/a.py"] {
        std::fs::write(root.join(rel), "def work(x):\n    return x + 1\n").unwrap();
    }
    git(root, &["commit", "-q", "-am", "two"]);

    // The project's own config is untracked, as `.infigraph/` always is.
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::fs::write(
        root.join(".infigraph/config.toml"),
        "[ignore]\ncode = [\"skipme/\"]\n",
    )
    .unwrap();

    let registry = bundled_registry().unwrap();
    let diff = infigraph_core::diff::semantic_diff(root, "HEAD~1", "HEAD", &registry).unwrap();
    let files: Vec<String> = diff
        .changes
        .iter()
        .map(|c| c.file.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(
        files,
        ["svc/a.py"],
        "`[ignore] code = [\"skipme/\"]` holds for a semantic diff: {:?}",
        diff.changes
    );
}
