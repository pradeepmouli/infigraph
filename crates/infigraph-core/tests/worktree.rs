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

/// A project that is not a git repository is in no worktree list, and that
/// says nothing about whether it exists: `worktree clean --apply` from a repo
/// evicted a live one and stopped its daemon.
#[test]
fn a_live_project_outside_git_is_never_a_teardown_candidate() {
    let (main, _parent, _wt) = init_repo_with_worktree();
    let plain = tempfile::tempdir().unwrap();
    let mut registry = empty_registry();
    registry
        .repos
        .insert("main".to_string(), repo_entry("main", main.path()));
    registry
        .repos
        .insert("plain".to_string(), repo_entry("plain", plain.path()));

    for scope in [Some(main.path()), None] {
        let drift = find_worktree_drift(&registry, scope);
        assert!(
            drift.teardown_candidates.is_empty(),
            "scope {scope:?}: {:?}",
            drift.teardown_candidates
        );
    }
}

/// Run from a directory outside any repository, no worktree list is
/// discovered at all. Every live project used to read as removed (18 of them
/// on one machine); only the one that is really gone may be a candidate.
#[test]
fn a_run_outside_any_repository_tears_down_only_what_is_gone() {
    let (main, parent, wt_path) = init_repo_with_worktree();
    let outside = tempfile::tempdir().unwrap();
    let gone = parent.path().join("deleted-project");
    let mut registry = empty_registry();
    registry
        .repos
        .insert("main".to_string(), repo_entry("main", main.path()));
    registry
        .repos
        .insert("wt".to_string(), repo_entry("wt", &wt_path));
    registry
        .repos
        .insert("gone".to_string(), repo_entry("gone", &gone));

    let drift = find_worktree_drift(&registry, Some(outside.path()));
    assert_eq!(drift.teardown_candidates, vec![gone]);
}

/// A project in a subdirectory of a repository is not a worktree root, so it
/// is never listed; it is not gone either.
#[test]
fn a_project_in_a_subdirectory_of_a_repo_is_not_a_teardown_candidate() {
    let (main, _parent, _wt) = init_repo_with_worktree();
    let nested = main.path().join("packages").join("app");
    std::fs::create_dir_all(&nested).unwrap();
    let mut registry = empty_registry();
    registry
        .repos
        .insert("main".to_string(), repo_entry("main", main.path()));
    registry
        .repos
        .insert("nested".to_string(), repo_entry("nested", &nested));

    let drift = find_worktree_drift(&registry, Some(main.path()));
    assert!(
        drift.teardown_candidates.is_empty(),
        "{:?}",
        drift.teardown_candidates
    );
}

/// A directory that cannot be examined (no permission on a parent) is not
/// known to be gone, and eviction stops its daemon, so it is kept.
#[cfg(unix)]
#[test]
fn a_directory_that_cannot_be_examined_is_not_a_teardown_candidate() {
    use std::os::unix::fs::PermissionsExt;
    let (main, _parent, _wt) = init_repo_with_worktree();
    let locked = tempfile::tempdir().unwrap();
    let project = locked.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let mut registry = empty_registry();
    registry
        .repos
        .insert("main".to_string(), repo_entry("main", main.path()));
    registry
        .repos
        .insert("locked".to_string(), repo_entry("locked", &project));

    std::fs::set_permissions(locked.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    let drift = find_worktree_drift(&registry, Some(main.path()));
    std::fs::set_permissions(locked.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    assert!(
        drift.teardown_candidates.is_empty(),
        "{:?}",
        drift.teardown_candidates
    );
}
