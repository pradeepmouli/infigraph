//! A linked worktree is its own project. One whose `.infigraph/` has no code
//! store (reclaimed by `worktree clean`, say) used to resolve UP to the
//! checkout that holds it, so every read answered from the parent's graph with
//! nothing saying so. It resolves to itself now, and says it has no index.

use infigraph_core::daemon::lifecycle::{
    ensure_daemon_running_required, no_daemon_came_up, DaemonStartOutcome,
};
use infigraph_core::degraded::{derived_from_disk, DegradedMode};
use infigraph_core::graph::names::STEM;
use infigraph_core::project::{is_linked_worktree_root, resolve_project_root};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.email=a@b", "-c", "user.name=n"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn make_store(dir: &Path) {
    let ig = dir.join(".infigraph");
    std::fs::create_dir_all(&ig).unwrap();
    std::fs::write(ig.join(STEM), b"x").unwrap();
}

/// An indexed checkout with a linked worktree nested inside it, as the repos
/// here keep them (`scratchpad/wt-<name>`).
fn checkout_with_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().canonicalize().unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "i"]);
    make_store(&main);
    let wt = main.join("scratchpad").join("wt-x");
    git(
        &main,
        &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
    );
    (tmp, main, wt)
}

#[test]
fn a_worktree_with_no_code_store_resolves_to_itself_not_to_the_checkout() {
    let (_tmp, main, wt) = checkout_with_worktree();
    assert!(is_linked_worktree_root(&wt));
    assert!(
        !is_linked_worktree_root(&main),
        "the checkout is not linked"
    );
    assert_eq!(resolve_project_root(&wt), wt);
    let deep = wt.join("crates/a/src");
    std::fs::create_dir_all(&deep).unwrap();
    assert_eq!(resolve_project_root(&deep), wt, "nor from a subdirectory");
}

#[test]
fn a_worktree_with_docs_only_is_still_not_the_checkouts_project() {
    // what `worktree clean` leaves: `.infigraph/` without a code store
    let (_tmp, _main, wt) = checkout_with_worktree();
    std::fs::create_dir_all(wt.join(".infigraph").join("docs.kuzu")).unwrap();
    assert_eq!(resolve_project_root(&wt), wt);
}

#[test]
fn a_worktree_with_its_own_store_and_the_checkout_itself_are_unchanged() {
    let (_tmp, main, wt) = checkout_with_worktree();
    make_store(&wt);
    assert_eq!(resolve_project_root(&wt), wt);
    let sub = main.join("crates/core");
    std::fs::create_dir_all(&sub).unwrap();
    assert_eq!(resolve_project_root(&sub), main);
    assert_eq!(resolve_project_root(&main), main);
}

#[test]
fn a_plain_nested_repository_still_resolves_to_the_indexed_project_around_it() {
    let (_tmp, main, _wt) = checkout_with_worktree();
    let nested = main.join("vendor").join("lib");
    std::fs::create_dir_all(nested.join(".git")).unwrap();
    assert_eq!(resolve_project_root(&nested), main);
}

#[test]
fn a_read_of_a_worktree_with_no_code_store_starts_no_daemon() {
    let (_tmp, _main, wt) = checkout_with_worktree();
    std::fs::create_dir_all(wt.join(".infigraph").join("docs.kuzu")).unwrap();
    // a binary that cannot run: a start attempt would be `Failed`
    let outcome = ensure_daemon_running_required(&wt, Path::new("/nonexistent/infigraph"));
    assert!(
        matches!(outcome, DaemonStartOutcome::AlreadyRunning),
        "nothing to start for an unindexed worktree: {outcome:?}"
    );
    assert!(
        !wt.join(".infigraph").join(STEM).exists(),
        "and no store is made by asking"
    );
}

#[test]
fn a_worktree_with_no_code_store_is_a_degraded_mode_that_says_what_to_run() {
    let (_tmp, main, wt) = checkout_with_worktree();
    let modes = derived_from_disk(&wt);
    let mode = modes
        .iter()
        .find(|m| **m == DegradedMode::WorktreeHasNoIndex)
        .unwrap_or_else(|| panic!("{modes:?}"));
    assert_eq!(mode.key(), "worktree-has-no-index");
    assert!(mode.remedy().contains("infigraph worktree init"));

    assert!(!derived_from_disk(&main).contains(&DegradedMode::WorktreeHasNoIndex));
    make_store(&wt);
    assert!(!derived_from_disk(&wt).contains(&DegradedMode::WorktreeHasNoIndex));
}

#[test]
fn the_no_daemon_error_for_such_a_worktree_names_the_cure() {
    let (_tmp, _main, wt) = checkout_with_worktree();
    let message = no_daemon_came_up(&wt, "reads would fail").to_string();
    assert!(message.contains("infigraph worktree init"), "{message}");
    let (_tmp2, main, _) = checkout_with_worktree();
    let message = no_daemon_came_up(&main, "reads would fail").to_string();
    assert!(!message.contains("worktree init"), "{message}");
}
