use std::path::Path;

use anyhow::Result;
use infigraph_core::clone::clone_infigraph_dir;
use infigraph_core::multi::Registry;
use infigraph_core::worktree::{find_worktree_drift, main_worktree_path};

use crate::index::cmd_index;

pub(crate) fn cmd_worktree_init(path: &Path) -> Result<()> {
    let main = main_worktree_path(path)?;

    if main != path && main.join(".infigraph").is_dir() {
        clone_infigraph_dir(&main, path)?;
        println!(
            "Cloned .infigraph/ from main worktree {} into {}.",
            main.display(),
            path.display()
        );
    }

    // Incremental index: content-hash comparison against the (possibly just-cloned)
    // graph means unchanged files are skipped automatically -- no separate "seeded"
    // code path needed here.
    cmd_index(path, false, false)?;
    println!("Indexed {}.", path.display());
    Ok(())
}

pub(crate) fn cmd_worktree_teardown(path: &Path) -> Result<()> {
    // Stop the watcher, if any, before touching the registry -- mirrors the same
    // sentinel-based stop cmd_delete_project already uses in info_commands.rs.
    let lock_path = path.join(".infigraph").join("watch.lock");
    if infigraph_core::watch::daemon::daemon_is_alive(&lock_path) {
        let sentinel = path.join(".infigraph").join("watch.stop");
        let _ = std::fs::write(&sentinel, b"");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    let mut registry = Registry::load()?;
    let removed = registry.deregister_by_path(path);
    registry.save()?;

    if removed.is_empty() {
        println!(
            "{} was not in the registry (nothing to evict).",
            path.display()
        );
    } else {
        println!(
            "Evicted '{}' from the registry. .infigraph/ left on disk.",
            removed.join(", ")
        );
    }
    Ok(())
}

pub(crate) fn cmd_worktree_reconcile(global: bool) -> Result<()> {
    let registry = Registry::load()?;
    let scope = if global {
        None
    } else {
        Some(std::env::current_dir()?)
    };
    let drift = find_worktree_drift(&registry, scope.as_deref());

    for path in &drift.teardown_candidates {
        cmd_worktree_teardown(path)?;
    }

    if drift.bootstrap_candidates.is_empty() {
        println!("No unindexed worktrees found.");
    } else {
        println!(
            "{} unindexed worktree(s) found:",
            drift.bootstrap_candidates.len()
        );
        for path in &drift.bootstrap_candidates {
            println!(
                "  run `infigraph worktree init {}` to bootstrap it",
                path.display()
            );
        }
    }
    Ok(())
}
