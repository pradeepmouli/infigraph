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

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq)]
pub struct GitWorktree {
    pub path: PathBuf,
    /// The checked-out commit; `None` for a bare repository.
    pub head: Option<String>,
    /// `refs/heads/<name>`; `None` when detached.
    pub branch_ref: Option<String>,
    /// `git worktree lock`ed, with the reason if one was given.
    pub locked: Option<String>,
    /// Git reports its directory as gone (`git worktree prune` would drop it).
    pub prunable: bool,
}

/// Every worktree of the repo containing `path`, in the order `git worktree
/// list --porcelain` reports them. The first is always the main worktree (git's
/// documented, unconditional ordering).
pub fn list_worktrees(path: &Path) -> Result<Vec<GitWorktree>> {
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
    let mut worktrees: Vec<GitWorktree> = Vec::new();
    for line in stdout.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            worktrees.push(GitWorktree {
                path: PathBuf::from(p),
                head: None,
                branch_ref: None,
                locked: None,
                prunable: false,
            });
            continue;
        }
        let Some(current) = worktrees.last_mut() else {
            continue;
        };
        if let Some(head) = line.strip_prefix("HEAD ") {
            current.head = Some(head.to_string());
        } else if let Some(branch) = line.strip_prefix("branch ") {
            current.branch_ref = Some(branch.to_string());
        } else if line == "locked" {
            current.locked = Some(String::new());
        } else if let Some(reason) = line.strip_prefix("locked ") {
            current.locked = Some(reason.to_string());
        } else if line == "prunable" || line.starts_with("prunable ") {
            current.prunable = true;
        }
    }
    Ok(worktrees)
}

/// Live worktree paths for the repo containing `path`, in the order `git worktree
/// list --porcelain` reports them. The first element is always the main worktree
/// (git's documented, unconditional ordering).
pub fn list_worktree_paths(path: &Path) -> Result<Vec<PathBuf>> {
    Ok(list_worktrees(path)?
        .into_iter()
        .map(|worktree| worktree.path)
        .collect())
}

/// How long [`linked_worktrees`] keeps an answer for a project.
pub const LINKED_WORKTREES_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// The linked worktrees of the project at `root`, besides `root` itself, kept
/// for [`LINKED_WORKTREES_TTL`] so a caller on every tool call does not run git
/// on every tool call. `None` is "unknown" (git could not say), kept for the
/// TTL as well: a failure is not retried at once, and no caller may treat it as
/// "there are none".
///
/// The answer for a checkout with no `.git/worktrees` entries is read from the
/// file system alone: git is asked only when something is there to list.
#[derive(Default)]
pub struct WorktreeProbe {
    seen: std::sync::Mutex<std::collections::HashMap<PathBuf, Answer>>,
}

/// When a project's worktrees were last asked about, and what the answer was.
type Answer = (std::time::Instant, Option<Vec<PathBuf>>);

impl WorktreeProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// `root`'s linked worktrees at `now`, from the cache while it is fresh,
    /// else from `list` (every worktree of the repository, as `git worktree
    /// list` gives them, `root`'s own among them).
    pub fn linked(
        &self,
        root: &Path,
        now: std::time::Instant,
        list: &dyn Fn(&Path) -> Result<Vec<PathBuf>>,
    ) -> Option<Vec<PathBuf>> {
        let key = canonicalize_lenient(root);
        if let Some((at, answer)) = self
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            if now.saturating_duration_since(*at) < LINKED_WORKTREES_TTL {
                return answer.clone();
            }
        }
        let answer = if has_linked_worktree_entries(&key) {
            list(&key).ok().map(|all| {
                all.into_iter()
                    .filter(|p| canonicalize_lenient(p) != key)
                    .collect()
            })
        } else {
            Some(Vec::new())
        };
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (now, answer.clone()));
        answer
    }
}

/// Whether `<root>/.git/worktrees` holds an entry: git records a linked
/// worktree there, so with none (or a `.git` that is a file, a linked worktree's
/// own) there is nothing to list.
fn has_linked_worktree_entries(root: &Path) -> bool {
    std::fs::read_dir(root.join(".git").join("worktrees"))
        .is_ok_and(|mut entries| entries.next().is_some())
}

/// [`WorktreeProbe::linked`] on the process's own cache, asking git.
pub fn linked_worktrees(root: &Path) -> Option<Vec<PathBuf>> {
    static PROBE: std::sync::OnceLock<WorktreeProbe> = std::sync::OnceLock::new();
    PROBE.get_or_init(WorktreeProbe::new).linked(
        root,
        std::time::Instant::now(),
        &list_worktree_paths,
    )
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
/// A teardown candidate is an entry whose directory is gone
/// ([`crate::multi::RepoEntry::is_gone`]), whichever repo it belonged to: once
/// the directory is gone, `git rev-parse` can no longer say.
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

    // Teardown candidates: registered directories that are gone and that no
    // discovered repo still lists (a worktree deleted by hand stays listed
    // until `git worktree prune`). Being absent from a list proves nothing on
    // its own: a project outside git, one in a subdirectory of a repo, and
    // every project when this runs outside any repo are all absent and alive.
    let all_live_canon: std::collections::HashSet<PathBuf> = live_by_common
        .values()
        .flatten()
        .map(|p| canonicalize_lenient(p))
        .collect();
    for entry in registry.repos.values() {
        if entry.is_gone() && !all_live_canon.contains(&canonicalize_lenient(&entry.path)) {
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

    // ---- WorktreeProbe: one cached answer, git only when there is something to list ----

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// A lister that counts its calls, answering what git would for `all`.
    fn counting(
        all: Vec<PathBuf>,
        calls: &AtomicUsize,
    ) -> impl Fn(&Path) -> Result<Vec<PathBuf>> + '_ {
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(all.clone())
        }
    }

    #[test]
    fn a_project_with_a_linked_worktree_lists_it_and_asks_git_once_per_ttl() {
        let (_tmp, main, linked) = repo_with_linked_worktree();
        let calls = AtomicUsize::new(0);
        let list = counting(vec![main.clone(), linked.clone()], &calls);
        let probe = WorktreeProbe::new();
        let t0 = Instant::now();
        assert_eq!(probe.linked(&main, t0, &list), Some(vec![linked.clone()]));
        assert_eq!(
            probe.linked(&main, t0 + Duration::from_secs(59), &list),
            Some(vec![linked.clone()])
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "within the TTL: no second git call"
        );
        probe.linked(&main, t0 + LINKED_WORKTREES_TTL, &list);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "after the TTL it asks again"
        );
    }

    /// No `.git/worktrees` entry, no git: the common case costs a stat.
    #[test]
    fn a_checkout_with_no_worktrees_never_runs_git() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        let calls = AtomicUsize::new(0);
        let list = counting(vec![tmp.path().to_path_buf()], &calls);
        let probe = WorktreeProbe::new();
        assert_eq!(
            probe.linked(tmp.path(), Instant::now(), &list),
            Some(vec![])
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_linked_worktree_itself_and_a_non_repository_have_none_to_list() {
        let (_tmp, _main, linked) = repo_with_linked_worktree();
        let plain = tempfile::tempdir().unwrap();
        let calls = AtomicUsize::new(0);
        let list = counting(vec![], &calls);
        let probe = WorktreeProbe::new();
        assert_eq!(probe.linked(&linked, Instant::now(), &list), Some(vec![]));
        assert_eq!(
            probe.linked(plain.path(), Instant::now(), &list),
            Some(vec![])
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// A failure to list is "unknown", not "none", and is not retried at once.
    #[test]
    fn a_listing_failure_is_unknown_and_is_kept_for_the_ttl() {
        let (_tmp, main, _linked) = repo_with_linked_worktree();
        let calls = AtomicUsize::new(0);
        let failing = |_: &Path| -> Result<Vec<PathBuf>> {
            calls.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("git is not available")
        };
        let probe = WorktreeProbe::new();
        let t0 = Instant::now();
        assert_eq!(probe.linked(&main, t0, &failing), None);
        assert_eq!(
            probe.linked(&main, t0 + Duration::from_secs(1), &failing),
            None
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
