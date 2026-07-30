use std::collections::HashMap;
use std::path::PathBuf;

use infigraph_core::doctor::{
    check_registry, find_repo_entry, projects_in_scope, CheckStatus, DoctorContext, DoctorScope,
};
use infigraph_core::multi::{Registry, RepoEntry};

fn repo_entry(name: &str, path: &str) -> RepoEntry {
    RepoEntry {
        name: name.to_string(),
        path: PathBuf::from(path),
        languages: vec!["rust".to_string()],
        symbol_count: 100,
        module_count: 10,
        last_indexed_commit: None,
    }
}

fn ctx_for(scope: DoctorScope, registry: Registry) -> DoctorContext {
    DoctorContext {
        registry,
        scope,
        installed_build_hash: "testhash".to_string(),
        disk_free_bytes: Some(50 * 1024 * 1024 * 1024),
        scan_roots: Vec::new(),
    }
}

#[test]
fn find_repo_entry_matches_canonicalized_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();

    let mut repos = HashMap::new();
    repos.insert(
        "myproj".to_string(),
        repo_entry("myproj", project.to_str().unwrap()),
    );
    let registry = Registry {
        repos,
        groups: HashMap::new(),
    };

    let found = find_repo_entry(&registry, &project);
    assert!(
        found.is_some(),
        "must find the entry by canonicalized path match"
    );
    assert_eq!(found.unwrap().name, "myproj");
}

#[test]
fn find_repo_entry_returns_none_for_unregistered_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    let registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };

    assert!(find_repo_entry(&registry, &project).is_none());
}

#[test]
fn check_registry_project_scope_passes_when_registered() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();

    let mut repos = HashMap::new();
    repos.insert(
        "myproj".to_string(),
        repo_entry("myproj", project.to_str().unwrap()),
    );
    let registry = Registry {
        repos,
        groups: HashMap::new(),
    };
    let ctx = ctx_for(DoctorScope::Project(project.clone()), registry);

    let results = check_registry(&ctx);
    let registration = results
        .iter()
        .find(|r| r.name.contains("registration"))
        .expect("must produce a registration check result");
    assert_eq!(registration.status, CheckStatus::Pass);
}

#[test]
fn check_registry_project_scope_fails_when_unregistered() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    let registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };
    let ctx = ctx_for(DoctorScope::Project(project.clone()), registry);

    let results = check_registry(&ctx);
    let registration = results
        .iter()
        .find(|r| r.name.contains("registration"))
        .expect("must produce a registration check result");
    assert_eq!(registration.status, CheckStatus::Fail);
    assert!(
        registration.remediation.is_some(),
        "a FAIL must carry remediation text"
    );
}

#[test]
fn check_registry_global_scope_without_scan_roots_reports_skipped() {
    let registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };
    let mut ctx = ctx_for(DoctorScope::Global, registry);
    ctx.scan_roots = Vec::new();

    let results = check_registry(&ctx);
    let discovery = results
        .iter()
        .find(|r| r.name.contains("unregistered-project discovery"))
        .expect("must produce a discovery check result even with no scan roots");
    assert_eq!(discovery.status, CheckStatus::Warn);
    assert!(
        discovery.message.to_lowercase().contains("skipped"),
        "must say explicitly it was skipped, not imply a clean scan: {}",
        discovery.message
    );
}

#[test]
fn check_registry_global_scope_with_scan_roots_finds_unregistered_project() {
    let dir = tempfile::TempDir::new().unwrap();
    let scan_root = dir.path().join("projects");
    let unregistered = scan_root.join("orphan-proj");
    std::fs::create_dir_all(unregistered.join(".infigraph")).unwrap();

    let registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };
    let mut ctx = ctx_for(DoctorScope::Global, registry);
    ctx.scan_roots = vec![scan_root];

    let results = check_registry(&ctx);
    let discovery = results
        .iter()
        .find(|r| r.name.contains("orphan-proj"))
        .expect("must find the unregistered project under the scan root");
    assert_eq!(discovery.status, CheckStatus::Fail);
}

#[test]
fn projects_in_scope_project_returns_single_path() {
    let project_path = PathBuf::from("/path/to/project");
    let registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };
    let ctx = DoctorContext {
        registry,
        scope: DoctorScope::Project(project_path.clone()),
        installed_build_hash: "testhash".to_string(),
        disk_free_bytes: Some(50 * 1024 * 1024 * 1024),
        scan_roots: Vec::new(),
    };

    let paths = projects_in_scope(&ctx);
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0], project_path);
}

#[test]
fn projects_in_scope_global_returns_all_registered_repos() {
    let dir = tempfile::TempDir::new().unwrap();
    let proj1 = dir.path().join("proj1");
    let proj2 = dir.path().join("proj2");
    std::fs::create_dir_all(&proj1).unwrap();
    std::fs::create_dir_all(&proj2).unwrap();

    let mut repos = HashMap::new();
    repos.insert(
        "proj1".to_string(),
        repo_entry("proj1", proj1.to_str().unwrap()),
    );
    repos.insert(
        "proj2".to_string(),
        repo_entry("proj2", proj2.to_str().unwrap()),
    );

    let registry = Registry {
        repos,
        groups: HashMap::new(),
    };
    let ctx = DoctorContext {
        registry,
        scope: DoctorScope::Global,
        installed_build_hash: "testhash".to_string(),
        disk_free_bytes: Some(50 * 1024 * 1024 * 1024),
        scan_roots: Vec::new(),
    };

    let paths = projects_in_scope(&ctx);
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&proj1));
    assert!(paths.contains(&proj2));
}
