//! `infigraph worktree clean` (#210): reclaim what finished worktrees carry.
//!
//! A worktree outlives its work and keeps its graph (`.infigraph/`), its
//! `target/` and, on request, its `node_modules/`. This plans which
//! worktrees are finished and what can go, and carries the plan out. Planning
//! is read-only and is what the default (dry-run) prints; applying re-plans
//! each worktree immediately before deleting from it, because a session can
//! start between the table and the delete.
//!
//! A worktree is eligible only if git says it is finished -- clean, on a
//! branch fully contained in another ref (or detached at a reachable commit),
//! not `git worktree lock`ed, not the main worktree, not the one the process
//! runs in -- and nothing holds it: no infigraph lock or process, and no
//! cargo build for its `target/`. Every worktree of every repo in scope is
//! reported, with the reason when it is skipped.
//!
//! What is removed from `.infigraph/` is an allow-list
//! ([`crate::ops::classify_entry`]); anything unlisted survives.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::doctor::{disk_status, path_size, DiskStatus};
use crate::multi::Registry;
use crate::ops::{classify_entry, EntryClass};
use crate::project::canonicalize_lenient;
use crate::worktree::{find_worktree_drift, git_common_dir, list_worktrees, GitWorktree};

/// What to remove beyond the always-derived artifacts.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Also `node_modules/` (git-ignored ones only).
    pub deps: bool,
    /// Also the document index and its sidecars.
    pub docs: bool,
    /// Also snapshots and quarantined or retired graphs.
    pub restore_points: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Derived,
    DocsStore,
    RestorePoint,
    Target,
    NodeModules,
}

/// One path that would be (or was) removed.
#[derive(Debug, Clone)]
pub struct Item {
    pub path: PathBuf,
    pub kind: ItemKind,
    pub bytes: u64,
}

/// Why a worktree is left alone.
#[derive(Debug, Clone, PartialEq)]
pub enum Skip {
    Main,
    CurrentDirectory,
    GitLocked(String),
    PathGone,
    Dirty(usize),
    NotContained,
    Held(String),
    /// Git could not answer a question the verdict depends on.
    Unreadable(String),
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Skip::Main => write!(f, "the main worktree"),
            Skip::CurrentDirectory => write!(f, "this process is running in it"),
            Skip::GitLocked(reason) if reason.is_empty() => write!(f, "locked (git worktree lock)"),
            Skip::GitLocked(reason) => write!(f, "locked (git worktree lock): {reason}"),
            Skip::PathGone => write!(f, "its directory is gone (`git worktree prune`)"),
            Skip::Dirty(1) => write!(f, "1 path has uncommitted or untracked changes"),
            Skip::Dirty(n) => write!(f, "{n} paths have uncommitted or untracked changes"),
            Skip::NotContained => write!(
                f,
                "not contained in any other ref (squash-merged branches look like this)"
            ),
            Skip::Held(why) => write!(f, "in use: {why}"),
            Skip::Unreadable(why) => write!(f, "could not be judged: {why}"),
        }
    }
}

/// Something deliberately left in place, with its size.
#[derive(Debug, Clone)]
pub struct Kept {
    pub what: &'static str,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct WorktreePlan {
    pub path: PathBuf,
    /// The branch's short name; `None` when detached.
    pub branch: Option<String>,
    pub skip: Option<Skip>,
    /// Empty when the worktree is skipped.
    pub items: Vec<Item>,
    pub kept: Vec<Kept>,
    /// Parts of an eligible worktree left alone, and why.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RepoPlan {
    pub main: PathBuf,
    pub worktrees: Vec<WorktreePlan>,
}

#[derive(Debug, Clone, Default)]
pub struct CleanPlan {
    pub repos: Vec<RepoPlan>,
    /// Registry entries whose directory is gone: evicted by the same
    /// teardown `worktree reconcile` uses.
    pub dead_registry: Vec<PathBuf>,
    pub free_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Applied {
    /// Worktrees cleaned, with the bytes removed from each.
    pub cleaned: Vec<(PathBuf, u64)>,
    /// Worktrees that were eligible in the plan and no longer were.
    pub changed: Vec<(PathBuf, String)>,
    pub failed: Vec<(PathBuf, String)>,
    pub evicted: Vec<PathBuf>,
}

/// Plan a clean. `scope` is the repo to look at (any path in it), or `None`
/// for every repo in the registry; `cwd` is the directory the process runs in.
pub fn plan(registry: &Registry, scope: Option<&Path>, cwd: &Path, options: &Options) -> CleanPlan {
    let anchors: Vec<PathBuf> = match scope {
        Some(path) => vec![path.to_path_buf()],
        None => registry.repos.values().map(|e| e.path.clone()).collect(),
    };
    let mut plan = CleanPlan::default();
    let mut seen = HashSet::new();
    for anchor in anchors {
        // A registry path that is gone has no repo to look at; it shows up
        // in `dead_registry` below instead.
        let Ok(common) = git_common_dir(&anchor) else {
            continue;
        };
        if !seen.insert(common) {
            continue;
        }
        if let Some(repo) = plan_repo(&anchor, cwd, options) {
            plan.repos.push(repo);
        }
    }
    plan.repos.sort_by(|a, b| a.main.cmp(&b.main));

    // The same pure function `worktree reconcile` acts on.
    let mut dead = find_worktree_drift(registry, scope).teardown_candidates;
    dead.sort();
    plan.dead_registry = dead;
    plan.free_bytes = fs2::available_space(scope.unwrap_or(cwd)).ok();
    plan
}

fn plan_repo(anchor: &Path, cwd: &Path, options: &Options) -> Option<RepoPlan> {
    let listed = list_worktrees(anchor).ok()?;
    let main = listed.first()?.path.clone();
    let paths: Vec<&Path> = listed.iter().map(|w| w.path.as_path()).collect();
    let procs = crate::ps::list_infigraph_processes(&paths);
    let worktrees = listed
        .iter()
        .enumerate()
        .map(|(i, wt)| plan_worktree(anchor, wt, i == 0, cwd, options, &procs))
        .collect();
    Some(RepoPlan { main, worktrees })
}

fn short_branch(wt: &GitWorktree) -> Option<String> {
    wt.branch_ref
        .as_deref()
        .map(|r| r.strip_prefix("refs/heads/").unwrap_or(r).to_string())
}

/// Plan one worktree. `repo_dir` is any existing directory of its repo, for
/// the git queries that are about the repo rather than the worktree.
fn plan_worktree(
    repo_dir: &Path,
    wt: &GitWorktree,
    is_main: bool,
    cwd: &Path,
    options: &Options,
    procs: &[crate::ps::ProcessRow],
) -> WorktreePlan {
    let mut plan = WorktreePlan {
        path: wt.path.clone(),
        branch: short_branch(wt),
        skip: skip_reason(repo_dir, wt, is_main, cwd, procs),
        items: Vec::new(),
        kept: Vec::new(),
        notes: Vec::new(),
    };
    if plan.skip.is_none() {
        collect_items(&mut plan, options);
    }
    plan
}

fn skip_reason(
    repo_dir: &Path,
    wt: &GitWorktree,
    is_main: bool,
    cwd: &Path,
    procs: &[crate::ps::ProcessRow],
) -> Option<Skip> {
    if is_main {
        return Some(Skip::Main);
    }
    if canonicalize_lenient(cwd).starts_with(canonicalize_lenient(&wt.path)) {
        return Some(Skip::CurrentDirectory);
    }
    if let Some(reason) = &wt.locked {
        return Some(Skip::GitLocked(reason.clone()));
    }
    if wt.prunable || !wt.path.is_dir() {
        return Some(Skip::PathGone);
    }
    match dirty_paths(&wt.path) {
        Ok(0) => {}
        Ok(n) => return Some(Skip::Dirty(n)),
        Err(e) => return Some(Skip::Unreadable(format!("{e:#}"))),
    }
    match is_contained(repo_dir, wt) {
        Ok(true) => {}
        Ok(false) => return Some(Skip::NotContained),
        Err(e) => return Some(Skip::Unreadable(format!("{e:#}"))),
    }
    holder(wt, procs).map(Skip::Held)
}

fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("run git {args:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Paths git reports as changed or untracked. Ignored paths are not counted.
fn dirty_paths(worktree: &Path) -> Result<usize> {
    let status = git_output(worktree, &["status", "--porcelain"])?;
    Ok(status.lines().filter(|l| !l.is_empty()).count())
}

/// Whether the worktree's commit is reachable from some ref that is not its
/// own branch or that branch's upstream: both contain it by construction and
/// prove nothing about the work being merged anywhere.
fn is_contained(repo_dir: &Path, wt: &GitWorktree) -> Result<bool> {
    let head = wt.head.as_deref().context("the worktree has no commit")?;
    let refs = git_output(
        repo_dir,
        &[
            "for-each-ref",
            "--contains",
            head,
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    let own = wt.branch_ref.as_deref();
    let upstream = match own {
        Some(branch) => git_output(repo_dir, &["for-each-ref", "--format=%(upstream)", branch])?
            .trim()
            .to_string(),
        None => String::new(),
    };
    Ok(refs
        .lines()
        .any(|r| Some(r) != own && (upstream.is_empty() || r != upstream)))
}

/// Whether something has the lock file open exclusively. A file that cannot be
/// opened is treated as held: skipping is the safe answer.
fn lock_is_held(path: &Path) -> bool {
    use fs2::FileExt;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(_) => true,
    }
}

/// What, if anything, is using the worktree's infigraph state: a lock some
/// process holds, or a live process the durable state ties to this path.
fn holder(wt: &GitWorktree, procs: &[crate::ps::ProcessRow]) -> Option<String> {
    let infigraph_dir = wt.path.join(".infigraph");
    if let Some(lock) = crate::ps::PROJECT_LOCKS
        .iter()
        .find(|lock| lock_is_held(&infigraph_dir.join(lock)))
    {
        return Some(format!("{lock} is held by a running process"));
    }
    let here = canonicalize_lenient(&wt.path);
    procs
        .iter()
        .find(|row| {
            row.alive
                && row
                    .projects
                    .iter()
                    .any(|p| canonicalize_lenient(Path::new(p)) == here)
        })
        .map(|row| format!("pid {} ({})", row.pid, row.roles.join(", ")))
}

/// Whether a cargo build holds `<target>/<profile>/.cargo-lock`.
fn cargo_build_running(target: &Path) -> bool {
    let Ok(profiles) = std::fs::read_dir(target) else {
        return false;
    };
    profiles
        .flatten()
        .any(|profile| lock_is_held_if_present(&profile.path().join(".cargo-lock")))
}

fn lock_is_held_if_present(path: &Path) -> bool {
    path.is_file() && lock_is_held(path)
}

fn is_git_ignored(worktree: &Path, relative: &str) -> bool {
    Command::new("git")
        .args(["check-ignore", "-q", "--", relative])
        .current_dir(worktree)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn collect_items(plan: &mut WorktreePlan, options: &Options) {
    let infigraph_dir = plan.path.join(".infigraph");
    let mut restore_bytes = 0;
    let mut docs_bytes = 0;
    if let Ok(entries) = std::fs::read_dir(&infigraph_dir) {
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let class = classify_entry(&name);
            let bytes = || path_size(&entry.path());
            match class {
                EntryClass::Kept => {}
                EntryClass::Derived => plan.items.push(Item {
                    path: entry.path(),
                    kind: ItemKind::Derived,
                    bytes: bytes(),
                }),
                EntryClass::DocsStore if options.docs => plan.items.push(Item {
                    path: entry.path(),
                    kind: ItemKind::DocsStore,
                    bytes: bytes(),
                }),
                EntryClass::DocsStore => docs_bytes += bytes(),
                EntryClass::RestorePoint if options.restore_points => plan.items.push(Item {
                    path: entry.path(),
                    kind: ItemKind::RestorePoint,
                    bytes: bytes(),
                }),
                EntryClass::RestorePoint => restore_bytes += bytes(),
            }
        }
    }
    if restore_bytes > 0 {
        plan.kept.push(Kept {
            what: "restore points",
            bytes: restore_bytes,
        });
    }
    if docs_bytes > 0 {
        plan.kept.push(Kept {
            what: "docs store",
            bytes: docs_bytes,
        });
    }
    collect_build_dir(plan, "target", ItemKind::Target, true);
    if options.deps {
        collect_build_dir(plan, "node_modules", ItemKind::NodeModules, false);
    }
}

/// A build directory of the worktree, if it may go: a real directory (never a
/// symlink, which is never followed) that git ignores, and, for `target/`,
/// that no cargo build is using.
fn collect_build_dir(plan: &mut WorktreePlan, name: &str, kind: ItemKind, cargo: bool) {
    let path = plan.path.join(name);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        plan.notes
            .push(format!("{name}/ is a symlink; left alone and not followed"));
    } else if !meta.is_dir() {
    } else if !is_git_ignored(&plan.path, name) {
        plan.notes
            .push(format!("{name}/ is not git-ignored; left alone"));
    } else if cargo && cargo_build_running(&path) {
        plan.notes.push(format!(
            "{name}/ left: a cargo build holds {name}/<profile>/.cargo-lock"
        ));
    } else {
        plan.items.push(Item {
            bytes: path_size(&path),
            path,
            kind,
        });
    }
}

/// Carry out `plan`. Each eligible worktree is re-planned first; one that is
/// no longer eligible is reported in [`Applied::changed`] and left alone.
/// `teardown` is the registry eviction `worktree reconcile` uses.
pub fn apply(
    plan: &CleanPlan,
    cwd: &Path,
    options: &Options,
    teardown: &mut dyn FnMut(&Path) -> Result<()>,
) -> Applied {
    let mut applied = Applied::default();
    for repo in &plan.repos {
        let listed = list_worktrees(&repo.main).unwrap_or_default();
        for planned in repo.worktrees.iter().filter(|w| w.skip.is_none()) {
            let Some(current) = listed.iter().find(|w| w.path == planned.path) else {
                applied.changed.push((
                    planned.path.clone(),
                    "changed since the plan: it is gone".into(),
                ));
                continue;
            };
            // A session can start between the table and the delete.
            let procs = crate::ps::list_infigraph_processes(&[planned.path.as_path()]);
            let fresh = plan_worktree(&repo.main, current, false, cwd, options, &procs);
            if let Some(why) = &fresh.skip {
                applied.changed.push((
                    planned.path.clone(),
                    format!("changed since the plan: {why}"),
                ));
                continue;
            }
            if let Err(e) = teardown(&planned.path) {
                applied
                    .failed
                    .push((planned.path.clone(), format!("teardown: {e:#}")));
                continue;
            }
            let mut bytes = 0;
            let mut removed = 0;
            for item in &fresh.items {
                match remove_item(&item.path) {
                    Ok(()) => {
                        bytes += item.bytes;
                        removed += 1;
                    }
                    Err(e) => applied.failed.push((item.path.clone(), format!("{e:#}"))),
                }
            }
            crate::audit::audit_log(
                "worktree-clean",
                "clean-worktree",
                &format!("{removed} paths, {bytes} bytes"),
                &planned.path.display().to_string(),
            );
            applied.cleaned.push((planned.path.clone(), bytes));
        }
    }
    for dead in &plan.dead_registry {
        match teardown(dead) {
            Ok(()) => applied.evicted.push(dead.clone()),
            Err(e) => applied
                .failed
                .push((dead.clone(), format!("teardown: {e:#}"))),
        }
    }
    applied
}

/// Remove one path without following symlinks.
fn remove_item(path: &Path) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

fn human(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (MB * 1024.0))
    } else {
        format!("{:.1} MB", bytes as f64 / MB)
    }
}

/// The plan as a table, and what `applied` did if anything.
pub fn render(plan: &CleanPlan, applied: Option<&Applied>) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    if applied.is_some() {
        let _ = writeln!(out, "worktree clean: applied");
    } else {
        let _ = writeln!(
            out,
            "worktree clean: dry run -- nothing was deleted; re-run with --apply to delete"
        );
    }
    if let Some(free) = plan.free_bytes {
        let verdict = match disk_status(free) {
            DiskStatus::Ok => "ok",
            DiskStatus::Low => "WARN: below 10 GiB, worth freeing",
            DiskStatus::Critical => "FAIL: below 2 GiB, free space now",
        };
        let _ = writeln!(out, "free space: {} ({verdict})", human(free));
    }
    let mut reclaimable = 0;
    for repo in &plan.repos {
        let _ = writeln!(out, "\nrepo {}", repo.main.display());
        for w in &repo.worktrees {
            let branch = w.branch.as_deref().unwrap_or("(detached)");
            match &w.skip {
                Some(skip) => {
                    let _ = writeln!(out, "  {} [{branch}]  skipped: {skip}", w.path.display());
                }
                None => {
                    let bytes: u64 = w.items.iter().map(|i| i.bytes).sum();
                    reclaimable += bytes;
                    let _ = writeln!(
                        out,
                        "  {} [{branch}]  eligible: reclaim {}",
                        w.path.display(),
                        human(bytes)
                    );
                    for item in &w.items {
                        let shown = item.path.strip_prefix(&w.path).unwrap_or(&item.path);
                        let _ = writeln!(
                            out,
                            "      remove {} ({})",
                            shown.display(),
                            human(item.bytes)
                        );
                    }
                    for kept in &w.kept {
                        let _ = writeln!(out, "      kept ({}) {}", kept.what, human(kept.bytes));
                    }
                    for note in &w.notes {
                        let _ = writeln!(out, "      note: {note}");
                    }
                }
            }
        }
    }
    if !plan.dead_registry.is_empty() {
        let _ = writeln!(
            out,
            "\nregistry: {} entr{} point at a deleted directory (evicted by --apply):",
            plan.dead_registry.len(),
            if plan.dead_registry.len() == 1 {
                "y"
            } else {
                "ies"
            }
        );
        for dead in &plan.dead_registry {
            let _ = writeln!(out, "  {}", dead.display());
        }
    }
    let _ = writeln!(out, "\ntotal reclaimable: {}", human(reclaimable));
    if let Some(applied) = applied {
        let _ = writeln!(out, "\napplied:");
        for (path, bytes) in &applied.cleaned {
            let _ = writeln!(out, "  cleaned {} ({})", path.display(), human(*bytes));
        }
        for (path, why) in &applied.changed {
            let _ = writeln!(out, "  skipped {}: {why}", path.display());
        }
        for path in &applied.evicted {
            let _ = writeln!(out, "  evicted registry entry {}", path.display());
        }
        for (path, why) in &applied.failed {
            let _ = writeln!(out, "  FAILED {}: {why}", path.display());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    /// A repo on `main` with one commit; `.infigraph/`, `target/` and
    /// `node_modules/` are git-ignored, as in a real checkout.
    struct Repo {
        _tmp: tempfile::TempDir,
        main: PathBuf,
    }

    impl Repo {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let main = std::fs::canonicalize(tmp.path()).unwrap().join("main");
            std::fs::create_dir_all(&main).unwrap();
            git(&main, &["init", "-q", "-b", "main"]);
            write(&main.join(".gitignore"), 0);
            std::fs::write(
                main.join(".gitignore"),
                ".infigraph/\ntarget/\nnode_modules/\n.DS_Store\n",
            )
            .unwrap();
            write(&main.join("a.txt"), 1);
            git(&main, &["add", "."]);
            git(&main, &["commit", "-qm", "init"]);
            Repo { _tmp: tmp, main }
        }

        fn dir(&self, name: &str) -> PathBuf {
            self.main.parent().unwrap().join(name)
        }

        /// A linked worktree on a new branch, with one commit on it.
        fn branch(&self, name: &str) -> PathBuf {
            let path = self.dir(name);
            git(
                &self.main,
                &["worktree", "add", "-q", "-b", name, path.to_str().unwrap()],
            );
            write(&path.join(format!("{name}.txt")), 1);
            git(&path, &["add", "."]);
            git(&path, &["commit", "-qm", name]);
            path
        }

        /// Merge `name` into main, so the branch is contained in it.
        fn merge(&self, name: &str) {
            git(&self.main, &["merge", "-q", "--no-ff", "-m", name, name]);
        }

        fn registry(&self) -> Registry {
            Registry::default()
        }
    }

    /// A project's `.infigraph/` as a real one looks, with derived files, the
    /// user's own, a restore point and a docs store.
    fn seed(worktree: &Path) {
        let ig = worktree.join(".infigraph");
        write(&ig.join("graph"), 4000);
        write(&ig.join("graph.wal"), 100);
        write(&ig.join("embeddings.bin"), 2000);
        write(&ig.join("bm25_cache.bin"), 500);
        write(&ig.join("daemon.log"), 300);
        write(&ig.join("last-run.index.json"), 50);
        write(&ig.join("write-tmp").join("payload"), 200);
        write(&ig.join("config.toml"), 20);
        write(&ig.join("sessions").join("s.md"), 60);
        write(&ig.join("structured-schemas").join("x.toml"), 30);
        write(&ig.join("learned").join("l.json"), 30);
        write(&ig.join("my-notes.txt"), 10);
        write(&ig.join("docs.kuzu"), 900);
        write(&ig.join("docs_embeddings.bin"), 100);
        write(&ig.join("snapshots").join("1").join("graph"), 700);
        write(&ig.join("graph.corrupt.1790000000"), 80);
    }

    fn options() -> Options {
        Options::default()
    }

    fn plan_for(repo: &Repo, options: &Options) -> CleanPlan {
        plan(&repo.registry(), Some(&repo.main), &repo.main, options)
    }

    fn find<'a>(plan: &'a CleanPlan, path: &Path) -> &'a WorktreePlan {
        plan.repos
            .iter()
            .flat_map(|repo| repo.worktrees.iter())
            .find(|w| w.path == path)
            .unwrap_or_else(|| panic!("{} is not in the plan", path.display()))
    }

    fn names(plan: &WorktreePlan) -> Vec<String> {
        let mut names: Vec<String> = plan
            .items
            .iter()
            .map(|i| i.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn no_teardown() -> impl FnMut(&Path) -> Result<()> {
        |_| Ok(())
    }

    /// The tree under `root`, with sizes, to compare before and after.
    fn tree(root: &Path) -> Vec<(PathBuf, u64)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let meta = std::fs::symlink_metadata(e.path()).unwrap();
                out.push((e.path(), meta.len()));
                if meta.is_dir() {
                    walk(&e.path(), out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out);
        out.sort();
        out
    }

    #[test]
    fn a_merged_clean_worktree_is_eligible_and_lists_only_derived_files() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        seed(&wt);

        let plan = plan_for(&repo, &options());
        let w = find(&plan, &wt);

        assert_eq!(w.skip, None, "{w:?}");
        assert_eq!(
            names(w),
            vec![
                "bm25_cache.bin",
                "daemon.log",
                "embeddings.bin",
                "graph",
                "graph.wal",
                "last-run.index.json",
                "write-tmp"
            ]
        );
        let kept: Vec<&str> = w.kept.iter().map(|k| k.what).collect();
        assert!(kept.contains(&"restore points"), "{kept:?}");
        assert!(kept.contains(&"docs store"), "{kept:?}");
    }

    #[test]
    fn what_nobody_listed_is_never_a_candidate() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        seed(&wt);

        let plan = plan_for(
            &repo,
            &Options {
                deps: true,
                docs: false,
                restore_points: false,
            },
        );
        let listed = names(find(&plan, &wt));
        for survivor in [
            "config.toml",
            "sessions",
            "structured-schemas",
            "learned",
            "my-notes.txt",
        ] {
            assert!(
                !listed.contains(&survivor.to_string()),
                "{survivor} listed: {listed:?}"
            );
        }
    }

    #[test]
    fn docs_and_restore_points_are_listed_only_when_asked() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        seed(&wt);

        let plan = plan_for(
            &repo,
            &Options {
                deps: false,
                docs: true,
                restore_points: true,
            },
        );
        let listed = names(find(&plan, &wt));
        for asked in [
            "docs.kuzu",
            "docs_embeddings.bin",
            "snapshots",
            "graph.corrupt.1790000000",
        ] {
            assert!(
                listed.contains(&asked.to_string()),
                "{asked} missing: {listed:?}"
            );
        }
    }

    #[test]
    fn every_worktree_appears_with_its_reason() {
        let repo = Repo::new();
        let merged = repo.branch("merged");
        repo.merge("merged");
        let unmerged = repo.branch("unmerged");
        let dirty = repo.branch("dirty");
        repo.merge("dirty");
        write(&dirty.join("scratch.txt"), 3); // untracked, not ignored
        let locked = repo.branch("locked");
        repo.merge("locked");
        git(&repo.main, &["worktree", "lock", locked.to_str().unwrap()]);

        let plan = plan_for(&repo, &options());

        assert_eq!(find(&plan, &repo.main).skip, Some(Skip::Main));
        assert_eq!(find(&plan, &merged).skip, None);
        assert_eq!(find(&plan, &unmerged).skip, Some(Skip::NotContained));
        assert_eq!(find(&plan, &dirty).skip, Some(Skip::Dirty(1)));
        assert!(matches!(
            find(&plan, &locked).skip,
            Some(Skip::GitLocked(_))
        ));
        let total: usize = plan.repos.iter().map(|r| r.worktrees.len()).sum();
        assert_eq!(total, 5, "a worktree was silently left out of the plan");
    }

    #[test]
    fn ignored_files_do_not_make_a_worktree_dirty() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        seed(&wt); // .infigraph/ is git-ignored
        write(&wt.join("target").join("debug").join("x"), 5);

        assert_eq!(find(&plan_for(&repo, &options()), &wt).skip, None);
    }

    #[test]
    fn a_squash_merged_branch_is_not_contained_and_says_so() {
        let repo = Repo::new();
        let wt = repo.branch("squashed");
        git(&repo.main, &["merge", "-q", "--squash", "squashed"]);
        git(&repo.main, &["commit", "-qm", "squash"]);

        let plan = plan_for(&repo, &options());
        let w = find(&plan, &wt);
        assert_eq!(w.skip, Some(Skip::NotContained));
        assert!(
            Skip::NotContained.to_string().contains("squash-merged"),
            "{}",
            Skip::NotContained
        );
    }

    #[test]
    fn a_pushed_branch_is_not_contained_in_its_own_upstream() {
        let repo = Repo::new();
        let bare = repo.dir("remote.git");
        git(
            repo.dir("").as_path(),
            &["init", "-q", "--bare", bare.to_str().unwrap()],
        );
        git(
            &repo.main,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        let wt = repo.branch("pushed");
        git(&wt, &["push", "-q", "-u", "origin", "pushed"]);

        let plan = plan_for(&repo, &options());
        assert_eq!(
            find(&plan, &wt).skip,
            Some(Skip::NotContained),
            "the branch's own upstream contains it trivially and proves nothing"
        );
    }

    #[test]
    fn a_detached_worktree_is_eligible_only_at_a_reachable_commit() {
        let repo = Repo::new();
        let reachable = repo.dir("detached-ok");
        git(
            &repo.main,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                reachable.to_str().unwrap(),
                "main",
            ],
        );
        let stray = repo.branch("stray");
        let orphan = repo.dir("detached-lost");
        git(
            &repo.main,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                orphan.to_str().unwrap(),
                "stray",
            ],
        );
        git(
            &repo.main,
            &["worktree", "remove", "--force", stray.to_str().unwrap()],
        );
        git(&repo.main, &["branch", "-D", "stray"]);

        let plan = plan_for(&repo, &options());
        assert_eq!(find(&plan, &reachable).skip, None);
        assert_eq!(find(&plan, &orphan).skip, Some(Skip::NotContained));
    }

    #[test]
    fn the_worktree_the_process_runs_in_is_never_touched() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");

        let plan = plan(&repo.registry(), Some(&repo.main), &wt, &options());
        assert_eq!(find(&plan, &wt).skip, Some(Skip::CurrentDirectory));
    }

    #[test]
    fn a_held_infigraph_lock_skips_the_worktree() {
        use fs2::FileExt;
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        seed(&wt);
        let lock_path = wt.join(".infigraph").join("watch.lock");
        write(&lock_path, 0);
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        held.try_lock_exclusive().unwrap();

        let plan = plan_for(&repo, &options());
        assert!(
            matches!(find(&plan, &wt).skip, Some(Skip::Held(ref why)) if why.contains("watch.lock")),
            "{:?}",
            find(&plan, &wt).skip
        );
        drop(held);
        assert_eq!(find(&plan_for(&repo, &options()), &wt).skip, None);
    }

    #[test]
    fn a_running_cargo_build_protects_target_but_not_the_graph() {
        use fs2::FileExt;
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        seed(&wt);
        write(&wt.join("target").join("debug").join("deps").join("x"), 50);
        let cargo_lock = wt.join("target").join("debug").join(".cargo-lock");
        write(&cargo_lock, 0);
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&cargo_lock)
            .unwrap();
        held.try_lock_exclusive().unwrap();

        let plan = plan_for(&repo, &options());
        let w = find(&plan, &wt);
        assert_eq!(w.skip, None);
        assert!(!names(w).contains(&"target".to_string()), "{:?}", names(w));
        assert!(names(w).contains(&"graph".to_string()));
        assert!(w.notes.iter().any(|n| n.contains("cargo")), "{:?}", w.notes);
        drop(held);
        let plan = plan_for(&repo, &options());
        assert!(names(find(&plan, &wt)).contains(&"target".to_string()));
    }

    #[test]
    fn target_and_node_modules_need_to_be_git_ignored_and_node_modules_needs_deps() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        write(&wt.join("target").join("debug").join("x"), 5);
        write(&wt.join("node_modules").join("pkg").join("index.js"), 5);

        let without = plan_for(&repo, &options());
        let listed = names(find(&without, &wt));
        assert!(listed.contains(&"target".to_string()), "{listed:?}");
        assert!(!listed.contains(&"node_modules".to_string()), "{listed:?}");

        let with = plan_for(
            &repo,
            &Options {
                deps: true,
                ..Options::default()
            },
        );
        assert!(names(find(&with, &wt)).contains(&"node_modules".to_string()));
    }

    #[test]
    fn a_tracked_directory_called_target_is_never_removed() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        // `target/` is ignored in this repo, so track a differently named
        // build directory the way a project that commits one would.
        write(&wt.join("build").join("tracked.txt"), 5);
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "track"]);
        repo.merge("fix-a");

        let listed = names(find(
            &plan_for(
                &repo,
                &Options {
                    deps: true,
                    ..Options::default()
                },
            ),
            &wt,
        ));
        assert!(!listed.contains(&"build".to_string()), "{listed:?}");
        // and a tracked `target/` (force-added past the ignore) is not ignored:
        write(&wt.join("target").join("t.txt"), 5);
        git(&wt, &["add", "-f", "target/t.txt"]);
        git(&wt, &["commit", "-qm", "track target"]);
        repo.merge("fix-a");
        let listed = names(find(&plan_for(&repo, &options()), &wt));
        assert!(!listed.contains(&"target".to_string()), "{listed:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_target_is_not_followed() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        // `target/` in .gitignore matches directories only; a symlink named
        // target needs the plain pattern to be ignored rather than untracked.
        let exclude = repo.main.join(".git").join("info").join("exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        std::fs::write(&exclude, "target\n").unwrap();
        let outside = repo.dir("elsewhere");
        write(&outside.join("precious"), 5);
        std::os::unix::fs::symlink(&outside, wt.join("target")).unwrap();

        let plan = plan_for(&repo, &options());
        let w = find(&plan, &wt);
        assert!(!names(w).contains(&"target".to_string()));
        assert!(
            w.notes.iter().any(|n| n.contains("symlink")),
            "{:?}",
            w.notes
        );
        apply(&plan, &repo.main, &options(), &mut no_teardown());
        assert!(outside.join("precious").exists());
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        seed(&wt);
        let before = tree(repo.main.parent().unwrap());

        let plan = plan_for(
            &repo,
            &Options {
                deps: true,
                docs: true,
                restore_points: true,
            },
        );
        let _ = render(&plan, None);

        assert_eq!(tree(repo.main.parent().unwrap()), before);
    }

    #[test]
    fn apply_removes_exactly_the_listed_paths_and_tears_the_worktree_down() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        seed(&wt);
        write(&wt.join("target").join("debug").join("x"), 5);
        let ig = wt.join(".infigraph");
        let plan = plan_for(&repo, &options());
        let expected: u64 = find(&plan, &wt).items.iter().map(|i| i.bytes).sum();

        let mut torn_down = Vec::new();
        let applied = apply(&plan, &repo.main, &options(), &mut |p: &Path| {
            torn_down.push(p.to_path_buf());
            Ok(())
        });

        assert_eq!(applied.cleaned, vec![(wt.clone(), expected)]);
        assert_eq!(torn_down, vec![wt.clone()]);
        for gone in [
            "graph",
            "graph.wal",
            "embeddings.bin",
            "bm25_cache.bin",
            "daemon.log",
            "last-run.index.json",
            "write-tmp",
        ] {
            assert!(!ig.join(gone).exists(), "{gone} survived");
        }
        assert!(!wt.join("target").exists());
        for survivor in [
            "config.toml",
            "sessions/s.md",
            "structured-schemas/x.toml",
            "learned/l.json",
            "my-notes.txt",
            "docs.kuzu",
            "docs_embeddings.bin",
            "snapshots/1/graph",
            "graph.corrupt.1790000000",
        ] {
            assert!(ig.join(survivor).exists(), "{survivor} was removed");
        }
        assert!(wt.join("a.txt").exists(), "tracked files are never touched");
    }

    #[test]
    fn the_infigraph_dir_stays_even_when_nothing_in_it_is_left() {
        // A worktree with no config.toml, sessions or restore points: the
        // empty directory is what tells `worktree reconcile` and `doctor` the
        // worktree was cleaned on purpose rather than never indexed.
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        write(&wt.join(".infigraph").join("graph"), 100);
        write(&wt.join(".infigraph").join("embeddings.bin"), 100);

        let plan = plan_for(&repo, &options());
        apply(&plan, &repo.main, &options(), &mut no_teardown());

        assert!(wt.join(".infigraph").is_dir());
        assert_eq!(std::fs::read_dir(wt.join(".infigraph")).unwrap().count(), 0);
    }

    #[test]
    fn a_worktree_that_stopped_being_eligible_since_the_plan_is_left_alone() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        seed(&wt);
        let plan = plan_for(&repo, &options());
        assert_eq!(find(&plan, &wt).skip, None);

        write(&wt.join("started-editing.txt"), 3); // dirty after the plan

        let mut torn_down = Vec::new();
        let applied = apply(&plan, &repo.main, &options(), &mut |p: &Path| {
            torn_down.push(p.to_path_buf());
            Ok(())
        });

        assert!(applied.cleaned.is_empty(), "{applied:?}");
        assert!(torn_down.is_empty());
        assert_eq!(applied.changed.len(), 1);
        assert!(
            applied.changed[0].1.contains("changed since the plan"),
            "{applied:?}"
        );
        assert!(wt.join(".infigraph").join("graph").exists());
    }

    #[test]
    fn a_dead_registry_entry_is_listed_and_evicted_through_teardown() {
        let repo = Repo::new();
        let gone = repo.dir("deleted-worktree");
        let mut registry = repo.registry();
        registry.repos.insert(
            "deleted".into(),
            crate::multi::RepoEntry {
                name: "deleted".into(),
                path: gone.clone(),
                languages: vec![],
                symbol_count: 0,
                module_count: 0,
                last_indexed_commit: None,
            },
        );

        let plan = plan(&registry, Some(&repo.main), &repo.main, &options());
        assert_eq!(plan.dead_registry, vec![gone.clone()]);

        let mut torn_down = Vec::new();
        let applied = apply(&plan, &repo.main, &options(), &mut |p: &Path| {
            torn_down.push(p.to_path_buf());
            Ok(())
        });
        assert_eq!(torn_down, vec![gone.clone()]);
        assert_eq!(applied.evicted, vec![gone]);
    }

    #[test]
    fn the_table_names_each_worktree_its_state_and_the_free_space() {
        let repo = Repo::new();
        let wt = repo.branch("fix-a");
        repo.merge("fix-a");
        seed(&wt);
        let other = repo.branch("not-merged");
        let plan = plan_for(&repo, &options());

        let table = render(&plan, None);

        assert!(table.contains(wt.to_str().unwrap()), "{table}");
        assert!(table.contains(other.to_str().unwrap()), "{table}");
        assert!(table.contains("eligible"), "{table}");
        assert!(table.contains("not contained in any other ref"), "{table}");
        assert!(table.contains("squash-merged"), "{table}");
        assert!(table.contains("kept (restore points)"), "{table}");
        assert!(table.contains("kept (docs store)"), "{table}");
        assert!(table.contains("dry run"), "{table}");
    }
}
