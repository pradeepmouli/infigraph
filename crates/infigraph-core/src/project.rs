//! Resolving a user-supplied path to the project root that owns it.
//!
//! Every entry point must agree on this. When they disagree the damage is
//! silent and permanent: running a command from a subdirectory creates a
//! second `.infigraph/` there, and because a store with a `graph` in it
//! *is* a project, every later lookup — including ones that would have
//! walked up correctly — stops at the new one instead of the real root.
//! One stray invocation forks a repo into two indexes, two daemons and two
//! sockets, and nothing ever merges them back.

use std::path::{Path, PathBuf};

/// A `.infigraph/` that belongs to a project rather than to the user.
///
/// The user-level `~/.infigraph/` also holds a `graph`, so the discriminator
/// is `registry.json`, which only the global store has. Without this check a
/// repo living under `$HOME` resolves to the global store.
fn is_project_store(dir: &Path) -> bool {
    let ig = dir.join(".infigraph");
    ig.join("graph").exists() && !ig.join("registry.json").exists()
}

/// The repo root for `start`, identified by a `.git` entry.
///
/// Matches a file as well as a directory: a linked worktree's `.git` is a
/// file pointing at the real git dir.
fn git_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Resolve `start` to the project root that should own it.
///
/// In order: `start` itself if it is already a project store; else the
/// nearest ancestor that is; else the git root, so that a *first* index run
/// from a subdirectory still lands at the repo root rather than creating a
/// store wherever the command happened to be typed; else `start` unchanged.
///
/// The git-root step is what makes resolution per-repo instead of per-path.
/// The ancestor walk alone only helps once a repo has been indexed at its
/// root — it cannot fix the very invocation that puts the store in the
/// wrong place to begin with.
pub fn resolve_project_root(start: &Path) -> PathBuf {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());

    if is_project_store(&start) {
        return start;
    }

    let mut current = start.as_path();
    while let Some(parent) = current.parent() {
        if is_project_store(parent) {
            return parent.to_path_buf();
        }
        current = parent;
    }

    if let Some(root) = git_root(&start) {
        return root;
    }

    start
}

#[cfg(test)]
mod tests {
    use super::resolve_project_root;

    /// Compare against the canonical form: `resolve_project_root`
    /// canonicalises, and on macOS a tempdir's `/var/...` is a symlink to
    /// `/private/var/...`, so the raw path never matches.
    fn canon(p: &std::path::Path) -> std::path::PathBuf {
        p.canonicalize().unwrap()
    }

    fn make_store(dir: &std::path::Path) {
        let ig = dir.join(".infigraph");
        std::fs::create_dir_all(&ig).unwrap();
        std::fs::write(ig.join("graph"), b"x").unwrap();
    }

    #[test]
    fn a_path_inside_an_indexed_project_resolves_to_that_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        make_store(root);
        let deep = root.join("packages/codegen/src/emitters");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(
            resolve_project_root(&deep),
            canon(root),
            "a subdirectory of an indexed project must resolve to the project \
             root; resolving to itself is what forks a second store"
        );
    }

    #[test]
    fn a_project_root_resolves_to_itself() {
        let tmp = tempfile::tempdir().unwrap();
        make_store(tmp.path());
        assert_eq!(resolve_project_root(tmp.path()), canon(tmp.path()));
    }

    #[test]
    fn a_user_level_store_is_not_mistaken_for_a_project() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let ig = home.join(".infigraph");
        std::fs::create_dir_all(&ig).unwrap();
        std::fs::write(ig.join("graph"), b"x").unwrap();
        std::fs::write(ig.join("registry.json"), b"{}").unwrap();
        let repo = home.join("code/myrepo");
        std::fs::create_dir_all(&repo).unwrap();

        assert_ne!(
            resolve_project_root(&repo),
            canon(home),
            "the global ~/.infigraph has a graph too; only registry.json \
             distinguishes it, and resolving a repo onto it is catastrophic"
        );
    }

    #[test]
    fn an_unindexed_subdirectory_resolves_to_its_git_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let deep = root.join("packages/tools/src");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(
            resolve_project_root(&deep),
            canon(root),
            "a first index run from a subdirectory must still land at the \
             repo root, or it creates the store in the wrong place"
        );
    }
}
