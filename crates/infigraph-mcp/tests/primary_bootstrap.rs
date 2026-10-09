//! The primary MCP instance must not lease projects nobody asked it about.
//!
//! In daemon mode a daemon outlives its client, and a project's daemon starts
//! (and attaches its document watcher) the first time a tool call names the
//! project. The primary instance used to also walk the whole registry at
//! `initialize` and start a daemon for every project whose documents are on --
//! which every cloned worktree is -- and then held a lease on each for as long
//! as it lived. With one such process per machine, ~33 daemons stayed leased
//! and none ever idled out.

use std::sync::Mutex;
use std::time::{Duration, Instant};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn watch_lock_held(lock_path: &std::path::Path) -> bool {
    infigraph_core::lockfile::try_acquire(lock_path, "test-probe")
        .map(|g| g.is_none())
        .unwrap_or(false)
}

fn wait_for_lock_state(lock_path: &std::path::Path, want_held: bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if watch_lock_held(lock_path) == want_held {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_primary_instance_starts_no_daemon_for_projects_no_tool_call_named() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // A daemon can only be spawned if the CLI binary is there and the CI
    // opt-out is off; without both, "none started" would pass vacuously.
    let Ok(_cli) = infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    ) else {
        eprintln!("skipping: infigraph CLI binary not built in this target dir");
        return;
    };
    let saved: Vec<_> = infigraph_core::daemon::lifecycle::CI_ENV_VARS
        .iter()
        .filter_map(|v| std::env::var_os(v).map(|old| (*v, old)))
        .collect();
    for v in infigraph_core::daemon::lifecycle::CI_ENV_VARS {
        std::env::remove_var(v);
    }

    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let old_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &home_path);
    std::env::set_var(infigraph_core::BACKEND_ENV, "daemon");

    // Several registered projects, each indexed and with documents on -- the
    // shape of a repo's cloned worktrees.
    let mut registry = infigraph_core::multi::Registry::default();
    let mut roots = Vec::new();
    for i in 0..3 {
        let root = home_path.join(format!("proj{i}"));
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        std::fs::write(
            root.join(".infigraph").join("config.toml"),
            "[docs]\nenabled = true\n",
        )
        .unwrap();
        registry.repos.insert(
            format!("proj{i}"),
            infigraph_core::multi::RepoEntry {
                name: format!("proj{i}"),
                path: root.clone(),
                languages: vec![],
                symbol_count: 0,
                module_count: 0,
                last_indexed_commit: None,
            },
        );
        roots.push(root);
    }
    registry.save().unwrap();

    infigraph_mcp::bootstrap_registered_projects();

    let started: Vec<_> = roots
        .iter()
        .filter(|root| {
            wait_for_lock_state(
                &root.join(".infigraph").join("watch.lock"),
                true,
                Duration::from_secs(3),
            )
        })
        .cloned()
        .collect();

    // Clean up whatever was started before asserting.
    for root in &started {
        std::fs::write(root.join(".infigraph").join("watch.stop"), "").unwrap();
        wait_for_lock_state(
            &root.join(".infigraph").join("watch.lock"),
            false,
            Duration::from_secs(15),
        );
    }
    std::env::set_var(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND);
    match old_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
    for (v, old) in saved {
        std::env::set_var(v, old);
    }

    assert!(
        started.is_empty(),
        "the primary instance started a daemon for projects no tool call named: {started:?}"
    );
}
