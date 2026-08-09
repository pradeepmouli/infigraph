use std::collections::HashMap;

use infigraph_core::multi::{Registry, RepoEntry};

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

#[test]
fn deregister_by_path_removes_matching_entry_and_returns_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = empty_registry();
    registry
        .repos
        .insert("myrepo".to_string(), repo_entry("myrepo", dir.path()));

    let removed = registry.deregister_by_path(dir.path());

    assert_eq!(removed, vec!["myrepo".to_string()]);
    assert!(registry.repos.is_empty());
}

#[test]
fn deregister_by_path_returns_empty_when_nothing_matches() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut registry = empty_registry();
    registry
        .repos
        .insert("myrepo".to_string(), repo_entry("myrepo", dir.path()));

    let removed = registry.deregister_by_path(other.path());

    assert!(removed.is_empty());
    assert_eq!(registry.repos.len(), 1);
}
