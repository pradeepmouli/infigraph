use std::collections::HashMap;
use std::path::PathBuf;

use infigraph_core::doctor::{
    check_disk, check_locks, check_registry, check_scip_staleness, check_sidecars, check_toolchain,
    check_wal_integrity, check_watchers, check_worktrees, find_repo_entry, format_report,
    projects_in_scope, run_doctor, CheckResult, CheckStatus, DoctorContext, DoctorReport,
    DoctorScope,
};
use infigraph_core::graph::GraphStore;
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
    // A `.infigraph` directory with no matching registry entry represents a
    // genuinely orphaned/indexed-but-unregistered project. A bare dir with no
    // `.infigraph` at all was never indexed and must PASS instead -- see
    // run_doctor_passes_registration_for_never_indexed_dir.
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();
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
fn check_registry_warns_when_a_scan_root_is_unreadable() {
    // An unreadable scan root (typo'd path, unmounted, permission-denied)
    // must not be silently skipped and reported as a clean scan -- it must
    // downgrade to WARN and name the root(s) it could not read.
    let dir = tempfile::TempDir::new().unwrap();
    let good_root = dir.path().join("good-root");
    std::fs::create_dir_all(&good_root).unwrap();
    let bad_root = dir.path().join("does-not-exist");

    let registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };
    let mut ctx = ctx_for(DoctorScope::Global, registry);
    ctx.scan_roots = vec![good_root, bad_root.clone()];

    let results = check_registry(&ctx);
    let discovery = results
        .iter()
        .find(|r| r.name.contains("unregistered-project discovery"))
        .expect("must produce an unregistered-project discovery result");
    assert_eq!(discovery.status, CheckStatus::Warn);
    assert!(
        discovery.message.contains(bad_root.to_str().unwrap()),
        "message should name the unreadable root: {}",
        discovery.message
    );
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
            holder_started_at: 0,
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
            holder_started_at: 0,
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

/// A stale-build `watch.lock` holder must point at the exact command that
/// clears it, not just prose telling the reader to "restart it" -- doctor's
/// standing convention is every warning/failure names the tool to run.
#[test]
fn check_locks_stale_watch_lock_remediation_names_watch_stop() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    let lock_path = project.join(".infigraph").join("watch.lock");
    write_lock_file(
        &lock_path,
        &LockInfo {
            pid: std::process::id(),
            role: "cli-watch".to_string(),
            build_hash: "old-hash".to_string(),
            acquired_at: 1000,
            last_heartbeat: 1000,
            holder_started_at: 0,
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
    let watch_lock = results
        .iter()
        .find(|r| r.name.contains("watch.lock"))
        .expect("must check watch.lock");
    let remediation = watch_lock
        .remediation
        .as_deref()
        .expect("a Warn result must carry remediation text");
    assert!(
        remediation.contains("infigraph watch-stop"),
        "remediation should name the exact command to run: {remediation}"
    );
}

/// `graph.lock` held by the watch daemon's own connection (role contains
/// "watch") gets the same exact-command treatment as `watch.lock` itself.
#[test]
fn check_locks_stale_graph_lock_held_by_watch_role_names_watch_stop() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    let lock_path = project.join(".infigraph").join("graph.lock");
    write_lock_file(
        &lock_path,
        &LockInfo {
            pid: std::process::id(),
            role: "watch-daemon-probe".to_string(),
            build_hash: "old-hash".to_string(),
            acquired_at: 1000,
            last_heartbeat: 1000,
            holder_started_at: 0,
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
    let remediation = graph_lock
        .remediation
        .as_deref()
        .expect("a Warn result must carry remediation text");
    assert!(
        remediation.contains("infigraph watch-stop"),
        "remediation should name the exact command to run: {remediation}"
    );
}

/// `graph.lock` held by a non-watch, non-persistent operation (e.g. a plain
/// write) gets a different exact command -- `infigraph kill`, not
/// `watch-stop`, since there's no watcher to stop.
#[test]
fn check_locks_stale_graph_lock_non_watch_role_names_kill() {
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
            holder_started_at: 0,
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
    let remediation = graph_lock
        .remediation
        .as_deref()
        .expect("a Warn result must carry remediation text");
    assert!(
        remediation.contains("infigraph kill"),
        "remediation should name the exact command to run: {remediation}"
    );
}

#[test]
fn check_locks_passes_on_cleanly_released_zero_byte_lock() {
    // `LockFile::Drop` truncates the payload to zero bytes on a clean
    // release specifically so readers never see a stale identity -- a
    // zero-byte lock is the normal healthy post-release state, not a
    // problem, so this must PASS rather than WARN.
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
    assert_eq!(watch_lock.status, CheckStatus::Pass);
}

#[test]
fn check_locks_warns_on_genuinely_unparseable_lock() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();
    let lock_path = project.join(".infigraph").join("watch.lock");
    // Non-empty but not valid LockInfo JSON -- genuinely corrupt, not a
    // clean release.
    std::fs::write(&lock_path, b"not valid json garbage").unwrap();

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
        .expect("must check watch.lock even when unparseable");
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
            holder_started_at: 0,
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
fn check_watchers_passes_on_cleanly_released_zero_byte_lock() {
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

    let results = check_watchers(&ctx);
    let watcher = results
        .iter()
        .find(|r| r.name.contains("watcher liveness"))
        .expect("must check watch.lock even when zero-byte");
    assert_eq!(watcher.status, CheckStatus::Pass);
}

#[test]
fn check_watchers_warns_on_genuinely_unparseable_lock() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();
    let lock_path = project.join(".infigraph").join("watch.lock");
    // Non-empty but not valid LockInfo JSON -- genuinely corrupt, not a
    // clean release.
    std::fs::write(&lock_path, b"not valid json garbage").unwrap();

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
        .expect("must check watch.lock even when unparseable");
    assert_eq!(watcher.status, CheckStatus::Warn);
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

#[test]
fn check_disk_graph_size_uses_canonicalized_path_lookup() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    let infigraph_dir = project.join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();

    // The graph store is a *file* in the current layout, not a directory --
    // it must be a file here so the fix (path_size, which handles files) is
    // actually exercised; a directory would silently mask the bug it fixes.
    let graph_path = infigraph_dir.join("graph");
    let graph_bytes = 3 * 1024 * 1024; // exact, non-zero expected size: 3 MB
    std::fs::write(&graph_path, vec![b'x'; graph_bytes]).unwrap();

    // Register the project under a path representation that differs
    // textually from the scope's path but resolves to the same location on
    // disk, so the lookup genuinely has to canonicalize rather than
    // succeeding on a trivial string equality match.
    let registry_path = project.join(".");
    let mut repos = HashMap::new();
    repos.insert(
        "myproj".to_string(),
        repo_entry("myproj", registry_path.to_str().unwrap()),
    );
    let registry = Registry {
        repos,
        groups: HashMap::new(),
    };

    let ctx = DoctorContext {
        registry,
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "h".to_string(),
        disk_free_bytes: Some(50 * 1024 * 1024 * 1024),
        scan_roots: Vec::new(),
    };

    let results = check_disk(&ctx);
    let graph_size = results
        .iter()
        .find(|r| r.name.contains("graph size"))
        .expect("must find graph size result for matched project");
    assert_eq!(graph_size.status, CheckStatus::Pass);
    assert!(
        graph_size.message.contains("3 MB"),
        "expected exact 3 MB size from the file-based graph store, got: {}",
        graph_size.message
    );
}

#[test]
fn run_doctor_aggregates_every_check_category() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    // A `.infigraph` directory with no matching registry entry represents a
    // genuinely orphaned/indexed-but-unregistered project (not a
    // never-indexed one -- see run_doctor_passes_registration_for_never_indexed_dir
    // for that case), so the registration check should still FAIL here.
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project),
        installed_build_hash: "h".to_string(),
        disk_free_bytes: Some(50 * 1024 * 1024 * 1024),
        scan_roots: Vec::new(),
    };

    let report = run_doctor(ctx);
    let categories: std::collections::HashSet<&str> =
        report.checks.iter().map(|c| c.category).collect();
    assert!(categories.contains("registry"));
    // registration check on an orphaned .infigraph dir must FAIL, making the
    // aggregate worst status FAIL -- proves run_doctor doesn't silently drop it
    assert_eq!(report.worst_status(), CheckStatus::Fail);
}

#[test]
fn run_doctor_passes_registration_for_never_indexed_dir() {
    // A bare directory with no `.infigraph` state at all was never indexed --
    // it's not "orphaned," so the registration check must PASS, not FAIL.
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project),
        installed_build_hash: "h".to_string(),
        disk_free_bytes: Some(50 * 1024 * 1024 * 1024),
        scan_roots: Vec::new(),
    };

    let report = run_doctor(ctx);
    let registration = report
        .checks
        .iter()
        .find(|c| c.name.contains("registration"))
        .expect("must produce a registration result");
    assert_eq!(registration.status, CheckStatus::Pass);
}

#[test]
fn format_report_uses_plain_glyphs_by_color_flag() {
    let report = DoctorReport {
        scope: DoctorScope::Project(PathBuf::from("/tmp/x")),
        checks: vec![
            CheckResult {
                category: "disk",
                name: "disk: free space".to_string(),
                status: CheckStatus::Pass,
                message: "10 GB free".to_string(),
                remediation: None,
            },
            CheckResult {
                category: "locks",
                name: "graph.lock".to_string(),
                status: CheckStatus::Warn,
                message: "stale".to_string(),
                remediation: Some("delete it".to_string()),
            },
            CheckResult {
                category: "registry",
                name: "registration".to_string(),
                status: CheckStatus::Fail,
                message: "not registered".to_string(),
                remediation: Some("run infigraph index".to_string()),
            },
        ],
    };

    let plain = format_report(&report, false);
    assert!(plain.contains("[✓]"), "plain output:\n{plain}");
    assert!(plain.contains("[!]"), "plain output:\n{plain}");
    assert!(plain.contains("[✗]"), "plain output:\n{plain}");
    assert!(
        !plain.contains('\x1b'),
        "color=false must never emit ANSI escapes:\n{plain}"
    );

    let colored = format_report(&report, true);
    assert!(
        colored.contains("\x1b[32m✓\x1b[0m"),
        "colored output:\n{colored}"
    );
    assert!(
        colored.contains("\x1b[33m!\x1b[0m"),
        "colored output:\n{colored}"
    );
    assert!(
        colored.contains("\x1b[31m✗\x1b[0m"),
        "colored output:\n{colored}"
    );

    // Summary line stays plain text regardless of color, so existing
    // substring-based consumers (e.g. the MCP tool_dispatch test) keep working.
    assert!(plain.contains("1 PASS, 1 WARN, 1 FAIL"));
    assert!(colored.contains("1 PASS, 1 WARN, 1 FAIL"));
}

/// Serializes tests that mutate `INFIGRAPH_INSTANCES_DIR` (process-global),
/// mirroring the `ENV_LOCK` convention used by other integration test
/// binaries in this workspace (e.g. `startup_watch.rs`) -- not reusable
/// across files since each `tests/*.rs` compiles to its own crate.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn write_alive_watch_lock(lock_path: &std::path::Path) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_lock_file(
        lock_path,
        &LockInfo {
            pid: std::process::id(),
            role: "cli-watch".to_string(),
            build_hash: "any-hash".to_string(),
            acquired_at: now - 10,
            last_heartbeat: now, // fresh, and distinct from acquired_at
            holder_started_at: 0,
        },
    );
}

/// A watch daemon that's alive and healthy by every other measure, but for
/// a project with no live MCP server instance registered, must now WARN --
/// this is the orphan-daemon-detection half of the true-up work: a daemon
/// left running from a closed MCP session is otherwise invisible to doctor
/// since it looks identical to one still being used.
#[test]
fn check_watchers_warns_when_alive_watcher_has_no_live_mcp_instance() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();
    let lock_path = project.join(".infigraph").join("watch.lock");
    write_alive_watch_lock(&lock_path);

    let instances_dir = dir.path().join("instances");
    std::fs::create_dir_all(&instances_dir).unwrap();
    std::env::set_var("INFIGRAPH_INSTANCES_DIR", &instances_dir);

    let ctx = ctx_for(DoctorScope::Project(project.clone()), Registry::default());
    let results = check_watchers(&ctx);

    std::env::remove_var("INFIGRAPH_INSTANCES_DIR");

    let watcher = results
        .iter()
        .find(|r| r.name.contains("watcher liveness"))
        .expect("must produce a watcher liveness result");
    assert_eq!(watcher.status, CheckStatus::Warn);
    assert!(
        watcher.message.contains("no MCP server instance"),
        "message should explain no live MCP instance is serving this project: {}",
        watcher.message
    );
}

/// The counterpart: a live MCP instance genuinely registered for this exact
/// project must keep the watcher check passing -- the new check must not
/// warn on the ordinary, common case (MCP server actively using its own
/// watcher).
#[test]
fn check_watchers_passes_when_alive_watcher_has_a_live_mcp_instance() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();
    let lock_path = project.join(".infigraph").join("watch.lock");
    write_alive_watch_lock(&lock_path);

    let instances_dir = dir.path().join("instances");
    std::fs::create_dir_all(&instances_dir).unwrap();
    std::env::set_var("INFIGRAPH_INSTANCES_DIR", &instances_dir);

    // A live "MCP instance" for this project -- this test process itself
    // stands in for one, since the check only needs a PID/start-time that's
    // genuinely alive right now (same technique `prune_stale_holder`'s own
    // tests use for a live-holder case).
    let canonical_project = project.canonicalize().unwrap();
    let info = infigraph_core::instances::InstanceInfo::current(
        &canonical_project.to_string_lossy(),
        "stdio",
    );
    let _instance_guard = infigraph_core::instances::register_instance(&info).unwrap();

    let ctx = ctx_for(DoctorScope::Project(project.clone()), Registry::default());
    let results = check_watchers(&ctx);

    std::env::remove_var("INFIGRAPH_INSTANCES_DIR");

    let watcher = results
        .iter()
        .find(|r| r.name.contains("watcher liveness"))
        .expect("must produce a watcher liveness result");
    assert_eq!(
        watcher.status,
        CheckStatus::Pass,
        "a live MCP instance registered for this exact project must not warn: {}",
        watcher.message
    );
}

#[test]
fn check_worktrees_warns_on_teardown_candidate() {
    let main = tempfile::TempDir::new().unwrap();
    let git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(main.path())
            .status()
            .unwrap()
            .success());
    };
    git(&["init"]);
    git(&["config", "user.email", "t@t.com"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(main.path().join("a.py"), "x = 1\n").unwrap();
    git(&["add", "a.py"]);
    git(&["commit", "-m", "init"]);

    let parent = tempfile::TempDir::new().unwrap();
    let wt_path = parent.path().join("wt1");
    assert!(std::process::Command::new("git")
        .args(["worktree", "add", "-b", "gone", wt_path.to_str().unwrap()])
        .current_dir(main.path())
        .status()
        .unwrap()
        .success());
    assert!(std::process::Command::new("git")
        .args(["worktree", "remove", "--force", wt_path.to_str().unwrap()])
        .current_dir(main.path())
        .status()
        .unwrap()
        .success());

    let mut registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };
    registry.repos.insert(
        "main-repo".to_string(),
        repo_entry("main-repo", main.path().to_str().unwrap()),
    );
    registry.repos.insert(
        "wt1".to_string(),
        repo_entry("wt1", wt_path.to_str().unwrap()),
    );

    let ctx = ctx_for(DoctorScope::Project(main.path().to_path_buf()), registry);
    let results = check_worktrees(&ctx);

    let teardown_warning = results
        .iter()
        .find(|r| r.name.contains("wt1"))
        .expect("must flag the removed-but-registered worktree");
    assert_eq!(teardown_warning.status, CheckStatus::Warn);
    assert!(teardown_warning
        .remediation
        .as_ref()
        .unwrap()
        .contains("infigraph worktree teardown"));
}

fn project_with_graph(dir: &std::path::Path) -> (PathBuf, PathBuf) {
    let project = dir.join("myproj");
    let infigraph_dir = project.join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();
    let graph_path = infigraph_dir.join("graph");
    (project, graph_path)
}

#[test]
fn check_scip_staleness_warns_when_scip_generation_behind_ast_generation() {
    let dir = tempfile::TempDir::new().unwrap();
    let (project, graph_path) = project_with_graph(dir.path());

    {
        let store = GraphStore::open(&graph_path).unwrap();
        let lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
        store.bump_scip_generation_conn(&conn, &lock).unwrap();
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
        // ast_generation=3, scip_generation=1 -- 2 generations behind.
    }

    let ctx = ctx_for(DoctorScope::Project(project), Registry::default());
    let results = check_scip_staleness(&ctx);
    let check = results
        .iter()
        .find(|r| r.name.contains("SCIP enrichment"))
        .expect("a check must be reported once SCIP has run at least once");
    assert_eq!(check.status, CheckStatus::Warn);
    assert!(
        check.message.contains("2 AST generations behind"),
        "message: {}",
        check.message
    );
}

#[test]
fn check_scip_staleness_passes_when_caught_up() {
    let dir = tempfile::TempDir::new().unwrap();
    let (project, graph_path) = project_with_graph(dir.path());

    {
        let store = GraphStore::open(&graph_path).unwrap();
        let lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
        store.bump_scip_generation_conn(&conn, &lock).unwrap();
        // ast_generation=1, scip_generation=1 -- fully caught up.
    }

    let ctx = ctx_for(DoctorScope::Project(project), Registry::default());
    let results = check_scip_staleness(&ctx);
    let check = results
        .iter()
        .find(|r| r.name.contains("SCIP enrichment"))
        .unwrap();
    assert_eq!(check.status, CheckStatus::Pass);
}

#[test]
fn check_scip_staleness_is_silent_when_scip_has_never_run() {
    let dir = tempfile::TempDir::new().unwrap();
    let (project, graph_path) = project_with_graph(dir.path());

    {
        let store = GraphStore::open(&graph_path).unwrap();
        let lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
        // ast_generation=2, scip_generation=0 -- SCIP has never run, which
        // is "not yet judged," not "stale" -- must not warn (e.g. a
        // language with no applicable SCIP indexer would warn on every
        // single doctor run otherwise).
    }

    let ctx = ctx_for(DoctorScope::Project(project), Registry::default());
    let results = check_scip_staleness(&ctx);
    assert!(
        results.is_empty(),
        "a project that has never run SCIP enrichment must not be reported at all: {results:?}"
    );
}

#[test]
fn check_scip_staleness_is_silent_when_no_graph_exists() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();

    let ctx = ctx_for(DoctorScope::Project(project), Registry::default());
    let results = check_scip_staleness(&ctx);
    assert!(results.is_empty());
}

/// Regression test: the exact real-world incident this check exists for --
/// a process holding `graph.lock` gets killed (via `infigraph kill`, `gc`'s
/// orphaned-daemon sweep, or a bare `kill -9` outside infigraph entirely)
/// mid-write, leaving an unreplayed WAL behind. `doctor` must surface this
/// proactively rather than waiting for the next unrelated command to hit
/// `GraphStore::open`'s own refusal (github.com/pradeepmouli/infigraph#92).
#[test]
fn check_wal_integrity_warns_on_a_wal_left_by_a_dead_holder() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    let infigraph_dir = project.join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();
    std::fs::write(infigraph_dir.join("graph.wal"), b"wal").unwrap();

    const DEAD_PID: u32 = 999_999;
    write_lock_file(
        &infigraph_dir.join("graph.lock"),
        &LockInfo {
            pid: DEAD_PID,
            role: "graph-write".to_string(),
            build_hash: "some-build".to_string(),
            acquired_at: 1000,
            last_heartbeat: 1000,
            holder_started_at: 0,
        },
    );

    let ctx = ctx_for(DoctorScope::Project(project), Registry::default());
    let results = check_wal_integrity(&ctx);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, CheckStatus::Warn);
    assert!(
        results[0].message.contains(&DEAD_PID.to_string()),
        "message should name the dead holder: {}",
        results[0].message
    );
    assert!(
        results[0]
            .remediation
            .as_deref()
            .unwrap_or("")
            .contains("index --full"),
        "remediation should say how to recover: {:?}",
        results[0].remediation
    );
}

#[test]
fn check_wal_integrity_passes_with_no_wal() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();

    let ctx = ctx_for(DoctorScope::Project(project), Registry::default());
    let results = check_wal_integrity(&ctx);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, CheckStatus::Pass);
}

/// R3.1.4e (#100 second incident): running doctor must never mutate the
/// state it's inspecting -- no check may spawn a watcher, acquire a lock,
/// or otherwise act as a side effect of diagnosing. A real, fully-indexed
/// project (so every check family -- sidecars, SCIP staleness, WAL
/// integrity, disk, locks -- has real data to look at, not just an empty
/// directory) with no watcher ever started must still show no watch.lock
/// after a full doctor run.
#[test]
fn doctor_never_creates_a_watch_lock_as_a_side_effect() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("a.py"), "def a():\n    pass\n").unwrap();

    let registry = infigraph_languages::bundled_registry().unwrap();
    let mut prism = infigraph_core::Infigraph::open(&project, registry).unwrap();
    prism.init().unwrap();
    prism.index().unwrap();
    drop(prism);

    let watch_lock = project.join(".infigraph").join("watch.lock");
    assert!(!watch_lock.exists(), "must not exist before doctor runs");

    let ctx = ctx_for(DoctorScope::Project(project.clone()), Registry::default());
    let _report = run_doctor(ctx);

    assert!(
        !watch_lock.exists(),
        "running doctor must never spawn a watcher as a side effect"
    );
}
