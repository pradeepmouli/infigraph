use infigraph_core::multi::{Registry, RepoEntry};
use std::path::PathBuf;
use std::sync::Mutex;

// Registry::load/save read HOME at call time; HOME is process-global, so
// tests that override it must be serialized against each other (same
// pattern as SLOW_LOCK_ENV in crates/infigraph-core/tests/lockfile.rs).
static HOME_ENV: Mutex<()> = Mutex::new(());

#[test]
fn concurrent_saves_never_produce_unparseable_registry_json() {
    let _guard = HOME_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", dir.path());

    let mut handles = Vec::new();
    for t in 0..4 {
        handles.push(std::thread::spawn(move || {
            for i in 0..15 {
                let mut registry = Registry::load().unwrap_or_default();
                registry.repos.insert(
                    format!("repo-{t}-{i}"),
                    RepoEntry {
                        name: format!("repo-{t}-{i}"),
                        path: PathBuf::from(format!("/tmp/repo-{t}-{i}")),
                        languages: vec!["rust".to_string()],
                        symbol_count: 42,
                        module_count: 3,
                        last_indexed_commit: None,
                    },
                );
                registry.save().unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // Every save() call went through an atomic temp+rename swap, so the
    // final file must always parse — a torn write would fail here.
    let loaded = Registry::load();
    assert!(
        loaded.is_ok(),
        "registry.json corrupted after concurrent saves: {loaded:?}"
    );

    std::env::remove_var("HOME");
}
