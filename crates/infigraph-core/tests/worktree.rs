use std::collections::HashMap;
use std::process::Command;

use infigraph_core::multi::{Registry, RepoEntry};
use infigraph_core::worktree::{
    find_worktree_drift, git_common_dir, list_worktree_paths, main_worktree_path,
};

fn repo_entry(name: &str, path: &std::path::Path) -> RepoEntry {
    RepoEntry {
        name: name.to_string(),
        path: path.to_path_buf(),
        languages: vec!["python".to_string()],
        symbol_count: 1,
        module_count: 1,
        last_indexed_commit: None,
    }
}

fn empty_registry() -> Registry {
    Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    }
}

fn git(args: &[&str], cwd: &std::path::Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "git {:?} failed in {}",
        args,
        cwd.display()
    );
}

fn init_repo_with_worktree() -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
    let main = tempfile::tempdir().unwrap();
    git(&["init"], main.path());
    git(&["config", "user.email", "t@t.com"], main.path());
    git(&["config", "user.name", "t"], main.path());
    std::fs::write(main.path().join("a.txt"), "hello").unwrap();
    git(&["add", "a.txt"], main.path());
    git(&["commit", "-m", "init"], main.path());

    let parent = tempfile::tempdir().unwrap();
    let wt_path = parent.path().join("wt1");
    git(
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            wt_path.to_str().unwrap(),
        ],
        main.path(),
    );
    (main, parent, wt_path)
}

#[test]
fn list_worktree_paths_puts_main_worktree_first() {
    let (main, _parent, wt_path) = init_repo_with_worktree();
    let paths = list_worktree_paths(main.path()).unwrap();
    assert_eq!(paths.len(), 2);
    assert_eq!(
        paths[0].canonicalize().unwrap(),
        main.path().canonicalize().unwrap()
    );
    assert!(paths
        .iter()
        .any(|p| p.canonicalize().unwrap() == wt_path.canonicalize().unwrap()));
}

#[test]
fn main_worktree_path_resolves_from_the_linked_worktree_too() {
    let (main, _parent, wt_path) = init_repo_with_worktree();
    let resolved = main_worktree_path(&wt_path).unwrap();
    assert_eq!(
        resolved.canonicalize().unwrap(),
        main.path().canonicalize().unwrap()
    );
}

#[test]
fn git_common_dir_matches_between_main_and_linked_worktree() {
    let (main, _parent, wt_path) = init_repo_with_worktree();
    assert_eq!(
        git_common_dir(main.path()).unwrap(),
        git_common_dir(&wt_path).unwrap()
    );
}

#[test]
fn find_worktree_drift_flags_unindexed_worktree_as_bootstrap_candidate() {
    let (main, _parent, wt_path) = init_repo_with_worktree();
    // Registry knows about the main worktree only.
    let mut registry = empty_registry();
    registry.repos.insert(
        "main-repo".to_string(),
        repo_entry("main-repo", main.path()),
    );

    let drift = find_worktree_drift(&registry, Some(main.path()));
    assert_eq!(drift.bootstrap_candidates.len(), 1);
    assert_eq!(
        drift.bootstrap_candidates[0].canonicalize().unwrap(),
        wt_path.canonicalize().unwrap()
    );
    assert!(drift.teardown_candidates.is_empty());
}

#[test]
fn find_worktree_drift_flags_removed_worktree_as_teardown_candidate() {
    let (main, _parent, wt_path) = init_repo_with_worktree();
    git(
        &["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        main.path(),
    );

    let mut registry = empty_registry();
    registry.repos.insert(
        "main-repo".to_string(),
        repo_entry("main-repo", main.path()),
    );
    registry
        .repos
        .insert("wt1".to_string(), repo_entry("wt1", &wt_path));

    let drift = find_worktree_drift(&registry, Some(main.path()));
    assert_eq!(drift.teardown_candidates.len(), 1);
    assert_eq!(drift.teardown_candidates[0], wt_path);
}
