use std::path::Path;

use anyhow::Result;
use infigraph_core::clone::clone_infigraph_dir;
use infigraph_core::worktree::main_worktree_path;

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
