use std::path::Path;

use anyhow::Result;
use infigraph_core::clone::clone_infigraph_dir;
use infigraph_core::multi::Registry;
use infigraph_core::project::canonicalize_lenient;
use infigraph_core::worktree::{find_worktree_drift, main_worktree_path};

use crate::index::cmd_index;
use crate::info_commands::{request_daemon_stop, stop_scip_enrich_for, DaemonStop};

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
    // `git worktree remove` has usually deleted the directory already, so
    // work from the path it had -- see `canonicalize_lenient`.
    let path = &canonicalize_lenient(path);

    // The worktree's detached scip-enrich first (its imports are routed to
    // the daemon): found by argv and cwd, so it works after the directory
    // is gone.
    stop_scip_enrich_for(path);

    // Stop the daemon, if any, before touching the registry. Over its socket,
    // which lives outside the worktree, so this reaches it after the
    // directory is gone; the `watch.stop` fallback needs the directory.
    match request_daemon_stop(path) {
        Ok(DaemonStop::Stopped) => println!("Stopped the daemon for {}.", path.display()),
        Ok(DaemonStop::NotRunning) => {}
        Ok(DaemonStop::ViaSentinel(why)) => println!(
            "The daemon for {} did not take the stop request ({why}); wrote the stop sentinel instead.",
            path.display()
        ),
        // Only the sentinel write fails, on a directory that is gone; a
        // daemon whose root is gone exits on its own (#136).
        Err(e) => eprintln!(
            "warning: could not stop the daemon for {}: {e:#}",
            path.display()
        ),
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
