use infigraph_core::doctor::{
    check_locks, check_registry, check_watchers, CheckStatus, DoctorContext, DoctorScope,
};
use infigraph_core::lockfile::LockInfo;

fn write_lock_file(path: &std::path::Path, info: &LockInfo) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string(info).unwrap()).unwrap();
}

#[test]
fn check_registry_passes_for_registered_project() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();

    let mut registry = infigraph_core::multi::Registry::default();
    registry.repos.insert(
        "test".to_string(),
        infigraph_core::multi::RepoEntry {
            name: "test".to_string(),
            path: project.clone(),
            symbol_count: 100,
            module_count: 10,
            languages: vec![],
            last_indexed_commit: None,
        },
    );

    let ctx = DoctorContext {
        registry,
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "any-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_registry(&ctx);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, CheckStatus::Pass);
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
            role: "index".to_string(),
            build_hash: "test-hash".to_string(),
            acquired_at: 100,
            last_heartbeat: 100,
        },
    );

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "test-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_locks(&ctx);
    let graph_lock = results
        .iter()
        .find(|r| r.name.contains("graph.lock"))
        .expect("must produce a graph.lock result");
    assert_eq!(graph_lock.status, CheckStatus::Pass);
}

#[test]
fn check_locks_warns_on_stale_zero_byte_lock() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();
    let lock_path = project.join(".infigraph").join("graph.lock");
    std::fs::write(&lock_path, "").unwrap(); // empty/unreadable lock

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
        .expect("must produce a graph.lock result");
    assert_eq!(graph_lock.status, CheckStatus::Warn);
}

#[test]
fn check_locks_passes_when_lock_file_absent() {
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

    let results = check_locks(&ctx);
    let graph_lock = results
        .iter()
        .find(|r| r.name.contains("graph.lock"))
        .expect("must produce a graph.lock result");
    assert_eq!(graph_lock.status, CheckStatus::Pass);
}

#[test]
fn check_watchers_warns_when_heartbeat_stale_but_pid_alive() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();
    let lock_path = project.join(".infigraph").join("watch.lock");
    write_lock_file(
        &lock_path,
        &LockInfo {
            pid: std::process::id(),
            role: "cli-watch".to_string(),
            build_hash: "any-hash".to_string(),
            acquired_at: 0,
            last_heartbeat: 0, // 0 => "never heartbeat since epoch", definitely stale by any real clock
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
