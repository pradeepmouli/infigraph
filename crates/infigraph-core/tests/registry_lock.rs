use infigraph_core::multi::{Registry, RepoEntry};
use std::path::PathBuf;
use std::sync::Mutex;

// Registry::load/save read HOME at call time; HOME is process-global, so
// tests that override it must be serialized against each other (same
// pattern as SLOW_LOCK_ENV in crates/infigraph-core/tests/lockfile.rs).
static HOME_ENV: Mutex<()> = Mutex::new(());

fn entry(name: &str) -> RepoEntry {
    RepoEntry {
        name: name.to_string(),
        path: PathBuf::from(format!("/tmp/{name}")),
        languages: vec!["rust".to_string()],
        symbol_count: 42,
        module_count: 3,
        last_indexed_commit: None,
    }
}

/// Six writers each add ten distinct repos with `add`; returns the names
/// missing from the registry afterwards.
fn additions_lost(add: fn(&str)) -> Vec<String> {
    let threads = 6;
    let per = 10;
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..per {
                    add(&format!("repo-{t}-{i}"));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let stored = Registry::load().unwrap();
    (0..threads)
        .flat_map(|t| (0..per).map(move |i| format!("repo-{t}-{i}")))
        .filter(|name| !stored.repos.contains_key(name))
        .collect()
}

#[test]
fn concurrent_updates_each_land() {
    let _guard = HOME_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", dir.path());
    let lost = additions_lost(|name| {
        Registry::update(|r| {
            r.repos.insert(name.to_string(), entry(name));
            Ok(())
        })
        .unwrap();
    });
    std::env::remove_var("HOME");
    assert!(lost.is_empty(), "updates lost: {lost:?}");
}

#[test]
fn a_failed_update_saves_nothing() {
    let _guard = HOME_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", dir.path());
    let result: anyhow::Result<()> = Registry::update(|r| {
        r.repos.insert("half".to_string(), entry("half"));
        anyhow::bail!("refused")
    });
    let stored = Registry::load().unwrap();
    std::env::remove_var("HOME");
    assert!(result.is_err());
    assert!(stored.repos.is_empty());
}

#[test]
fn an_update_that_cannot_get_the_lock_says_busy() {
    let _guard = HOME_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", dir.path());
    std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
    let held = infigraph_core::lockfile::acquire(
        &dir.path().join(".infigraph").join("registry.lock"),
        "test-holder",
        std::time::Duration::from_secs(1),
    )
    .unwrap();
    let err = Registry::update(|_| Ok(())).unwrap_err();
    drop(held);
    std::env::remove_var("HOME");
    assert!(
        err.downcast_ref::<infigraph_core::lockfile::Busy>()
            .is_some(),
        "{err:#}"
    );
}

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
