use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::multi::Registry;
use crate::project::canonicalize_lenient;

pub fn git_common_dir(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(path)
        .output()
        .context("run git rev-parse --git-common-dir")?;
    anyhow::ensure!(
        output.status.success(),
        "not a git repository (or git rev-parse failed): {}",
        path.display()
    );
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let common_dir = PathBuf::from(raw);
    let resolved = if common_dir.is_absolute() {
        common_dir
    } else {
        path.join(common_dir)
    };
    resolved
        .canonicalize()
        .with_context(|| format!("canonicalize git common dir for {}", path.display()))
}

/// Live worktree paths for the repo containing `path`, in the order `git worktree
/// list --porcelain` reports them. The first element is always the main worktree
/// (git's documented, unconditional ordering).
pub fn list_worktree_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(path)
        .output()
        .context("run git worktree list --porcelain")?;
    anyhow::ensure!(
        output.status.success(),
        "git worktree list failed for {}",
        path.display()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut paths = Vec::new();
    for line in stdout.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            paths.push(PathBuf::from(p));
        }
    }
    Ok(paths)
}

pub fn main_worktree_path(path: &Path) -> Result<PathBuf> {
    list_worktree_paths(path)?
        .into_iter()
        .next()
        .context("git worktree list returned no entries")
}

/// Whether `path` is a linked worktree: inside git, and not the repo's main
/// worktree. Outside git, or when git cannot say, it is not.
pub fn is_linked_worktree(path: &Path) -> bool {
    main_worktree_path(path)
        .is_ok_and(|main| canonicalize_lenient(&main) != canonicalize_lenient(path))
}

#[derive(Debug, Default)]
pub struct WorktreeDrift {
    pub bootstrap_candidates: Vec<PathBuf>,
    pub teardown_candidates: Vec<PathBuf>,
}

/// Diff live git worktrees against the registry. When `repo_scope` is `Some`, only
/// the repo containing that path is considered; `None` sweeps every repo among the
/// registered projects (the `--global` case).
///
/// A removed worktree's registry entry can no longer self-report which repo it
/// belonged to (`git rev-parse` can't run in a directory that no longer exists),
/// so teardown detection can't always be strictly scope-verified for dead entries
/// -- see the comment below for how this is handled.
pub fn find_worktree_drift(registry: &Registry, repo_scope: Option<&Path>) -> WorktreeDrift {
    let mut drift = WorktreeDrift::default();

    // Discover each in-scope repo's current live worktree list. Scoped: probe
    // repo_scope directly (guaranteed to exist -- it's the caller's own cwd or a
    // known project root, never a path we're trying to detect drift *for*).
    // Global: one entry per distinct common-dir among registry paths that still
    // resolve (a dead entry can't self-report its own common-dir, but it's still
    // checked against whatever repos WERE discovered, below).
    let mut live_by_common: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    if let Some(scope) = repo_scope {
        if let (Ok(common), Ok(live)) = (git_common_dir(scope), list_worktree_paths(scope)) {
            live_by_common.insert(common, live);
        }
    } else {
        for entry in registry.repos.values() {
            if let Ok(common) = git_common_dir(&entry.path) {
                live_by_common
                    .entry(common)
                    .or_insert_with(|| list_worktree_paths(&entry.path).unwrap_or_default());
            }
        }
    }

    // Bootstrap candidates: live worktrees with no registry entry and no
    // .infigraph/ yet. Registry paths may differ textually from git's reported
    // paths even when they refer to the same directory (e.g. macOS's /var vs
    // /private/var symlink) -- compare canonicalized forms, not raw PathBufs.
    let registered_canon: std::collections::HashSet<PathBuf> = registry
        .repos
        .values()
        .map(|e| canonicalize_lenient(&e.path))
        .collect();
    for live in live_by_common.values() {
        for live_path in live {
            let registered = registered_canon.contains(&canonicalize_lenient(live_path));
            let has_infigraph = live_path.join(".infigraph").is_dir();
            if !registered && !has_infigraph {
                drift.bootstrap_candidates.push(live_path.clone());
            }
        }
    }

    // Teardown candidates: registered paths absent from every discovered repo's
    // live list. When scoped, entries whose common-dir is still resolvable and
    // does NOT match the scope are excluded; entries that are unresolvable (the
    // common case -- the worktree is gone) can't be verified against the scope
    // and are conservatively included, since the caller (a --path-scoped or
    // hook-triggered call) already knows which repo it just acted on.
    let all_live_canon: std::collections::HashSet<PathBuf> = live_by_common
        .values()
        .flatten()
        .map(|p| canonicalize_lenient(p))
        .collect();
    let scope_common = repo_scope.and_then(|p| git_common_dir(p).ok());
    for entry in registry.repos.values() {
        if let Some(ref scope) = scope_common {
            if let Ok(entry_common) = git_common_dir(&entry.path) {
                if &entry_common != scope {
                    continue;
                }
            }
        }
        if !all_live_canon.contains(&canonicalize_lenient(&entry.path)) {
            drift.teardown_candidates.push(entry.path.clone());
        }
    }

    drift
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// A repo with one commit and a linked worktree beside it.
    pub(crate) fn repo_with_linked_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        std::fs::write(main.join("a.txt"), "a").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-qm", "init"]);
        let linked = tmp.path().join("linked");
        git(&main, &["worktree", "add", "-q", linked.to_str().unwrap()]);
        (tmp, main, linked)
    }

    #[test]
    fn a_linked_worktree_is_told_apart_from_the_main_one() {
        let (_tmp, main, linked) = repo_with_linked_worktree();
        assert!(is_linked_worktree(&linked));
        assert!(!is_linked_worktree(&main));
    }

    #[test]
    fn a_directory_outside_git_is_not_a_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_linked_worktree(tmp.path()));
    }
}
