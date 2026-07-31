use std::collections::HashMap;
use std::path::PathBuf;

use infigraph_core::doctor::{
    check_disk, check_locks, check_registry, check_sidecars, check_toolchain, check_watchers,
    find_repo_entry, projects_in_scope, CheckStatus, DoctorContext, DoctorScope,
};
use infigraph_core::lockfile::LockInfo;
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

fn write_lock_file(path: &std::path::Path, info: &LockInfo) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string(info).unwrap()).unwrap();
}

#[test]
fn check_locks_passes_for_live_holder_matching_build_hash() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    let lock_path = project.join(".infigraph").join("graph.lock");
    write_lock_file(
        &lock_path,
        &LockInfo {
            pid: std::process::id(), // our own PID -- guaranteed alive
            role: "graph-write".to_string(),
            build_hash: "matching-hash".to_string(),
            acquired_at: 1000,
            last_heartbeat: 1000,
        },
    );
    let registry = infigraph_core::multi::Registry::default();
    let ctx = DoctorContext {
        registry,
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "matching-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_locks(&ctx);
    let graph_lock = results
        .iter()
        .find(|r| r.name.contains("graph.lock"))
        .expect("must check graph.lock");
    assert_eq!(graph_lock.status, CheckStatus::Pass);
}

#[test]
fn check_locks_warns_on_build_hash_mismatch() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    let lock_path = project.join(".infigraph").join("graph.lock");
    write_lock_file(
        &lock_path,
        &LockInfo {
            pid: std::process::id(),
            role: "graph-write".to_string(),
            build_hash: "old-hash".to_string(),
            acquired_at: 1000,
            last_heartbeat: 1000,
        },
    );
    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "new-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_locks(&ctx);
    let graph_lock = results
        .iter()
        .find(|r| r.name.contains("graph.lock"))
        .expect("must check graph.lock");
    assert_eq!(graph_lock.status, CheckStatus::Warn);
    assert!(
        graph_lock.message.contains("build"),
        "message should mention build hash mismatch: {}",
        graph_lock.message
    );
}

#[test]
fn check_locks_warns_on_stale_zero_byte_lock() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();
    let lock_path = project.join(".infigraph").join("watch.lock");
    std::fs::write(&lock_path, b"").unwrap(); // zero-byte, no holder payload

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "any-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_locks(&ctx);
    let watch_lock = results
        .iter()
        .find(|r| r.name.contains("watch.lock"))
        .expect("must check watch.lock even when zero-byte");
    assert_eq!(watch_lock.status, CheckStatus::Warn);
}

#[test]
fn check_locks_passes_when_lock_file_absent() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    // no .infigraph dir at all -- absent lock is not itself a problem

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "any-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_locks(&ctx);
    let graph_lock = results
        .iter()
        .find(|r| r.name.contains("graph.lock"))
        .expect("must still report a result for an absent lock file");
    assert_eq!(graph_lock.status, CheckStatus::Pass);
}

#[test]
fn check_watchers_warns_when_heartbeat_stale_but_pid_alive() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();
    let lock_path = project.join(".infigraph").join("watch.lock");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_lock_file(
        &lock_path,
        &LockInfo {
            pid: std::process::id(),
            role: "cli-watch".to_string(),
            build_hash: "any-hash".to_string(),
            acquired_at: now - 500,
            last_heartbeat: now - 400, // >300s stale, and distinct from acquired_at
        },
    );

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "any-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_watchers(&ctx);
    let watcher = results
        .iter()
        .find(|r| r.name.contains("watcher liveness"))
        .expect("must produce a watcher liveness result");
    assert_eq!(watcher.status, CheckStatus::Warn);
    assert!(
        watcher.message.contains("stale"),
        "message should mention stale heartbeat: {}",
        watcher.message
    );
}

#[test]
fn check_watchers_passes_when_no_watch_lock_present() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "any-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_watchers(&ctx);
    let watcher = results
        .iter()
        .find(|r| r.name.contains("watcher liveness"))
        .expect("must still produce a result when no watcher is running");
    assert_eq!(watcher.status, CheckStatus::Pass);
}

#[test]
fn check_disk_fails_below_2gb() {
    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Global,
        installed_build_hash: "h".to_string(),
        disk_free_bytes: Some(1024 * 1024 * 1024), // 1GB
        scan_roots: Vec::new(),
    };
    let results = check_disk(&ctx);
    let free_space = results
        .iter()
        .find(|r| r.name.contains("free space"))
        .unwrap();
    assert_eq!(free_space.status, CheckStatus::Fail);
}

#[test]
fn check_disk_warns_below_10gb() {
    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Global,
        installed_build_hash: "h".to_string(),
        disk_free_bytes: Some(5 * 1024 * 1024 * 1024), // 5GB
        scan_roots: Vec::new(),
    };
    let results = check_disk(&ctx);
    let free_space = results
        .iter()
        .find(|r| r.name.contains("free space"))
        .unwrap();
    assert_eq!(free_space.status, CheckStatus::Warn);
}

#[test]
fn check_disk_passes_above_10gb() {
    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Global,
        installed_build_hash: "h".to_string(),
        disk_free_bytes: Some(50 * 1024 * 1024 * 1024), // 50GB
        scan_roots: Vec::new(),
    };
    let results = check_disk(&ctx);
    let free_space = results
        .iter()
        .find(|r| r.name.contains("free space"))
        .unwrap();
    assert_eq!(free_space.status, CheckStatus::Pass);
}

#[test]
fn check_sidecars_warns_when_embeddings_older_than_graph_by_over_an_hour() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    let infigraph_dir = project.join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();

    let graph_path = infigraph_dir.join("graph");
    std::fs::write(&graph_path, b"graph-data").unwrap();
    let embeddings_path = infigraph_dir.join("embeddings.bin");
    std::fs::write(&embeddings_path, b"stale-embeddings").unwrap();

    // Back-date the embeddings file by 2 hours relative to the graph file.
    let graph_mtime = std::fs::metadata(&graph_path).unwrap().modified().unwrap();
    let stale_mtime = graph_mtime - std::time::Duration::from_secs(2 * 60 * 60);
    let stale_file = std::fs::File::open(&embeddings_path).unwrap();
    stale_file.set_modified(stale_mtime).unwrap();

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "h".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };
    let results = check_sidecars(&ctx);
    let sidecar = results
        .iter()
        .find(|r| r.name.contains("embeddings.bin"))
        .unwrap();
    assert_eq!(sidecar.status, CheckStatus::Warn);
}

#[test]
fn check_toolchain_passes_and_reports_installed_version() {
    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Global,
        installed_build_hash: "abc123".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };
    let results = check_toolchain(&ctx);
    let version = results.iter().find(|r| r.name.contains("version")).unwrap();
    assert_eq!(version.status, CheckStatus::Pass);
    assert!(version.message.contains("abc123"));
}
