//! `scripts/prune-build-cache.sh` refuses only while a running cargo or rustc
//! builds into the target directory it would prune. The process list is the
//! `PRUNE_BUILD_CACHE_PROCESSES` seam: one line per process, `name<TAB>cwd<TAB>args`,
//! with `?` for a working directory that cannot be read.
//!
//! The checkout that owns a target directory is the directory holding it, so
//! these tests build a fake checkout with its own `.shared-target` in a tempdir.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fake checkout with its shared target directory in it.
fn checkout() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let owner = dir.path().join("checkout");
    let target = owner.join(".shared-target");
    std::fs::create_dir_all(target.join("debug").join("deps")).unwrap();
    let owner = owner.canonicalize().unwrap();
    let target = target.canonicalize().unwrap();
    (dir, owner, target)
}

fn prune(target: &Path, listing: &str) -> Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/prune-build-cache.sh");
    Command::new("bash")
        .arg(script)
        .arg("--target-dir")
        .arg(target)
        .env("PRUNE_BUILD_CACHE_PROCESSES", listing)
        .output()
        .unwrap()
}

fn assert_proceeds(out: &Output, case: &str) {
    assert!(
        out.status.success(),
        "{case}: should proceed, got {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("dry run"),
        "{case}: no dry-run report: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

fn assert_refuses(out: &Output, case: &str) {
    assert_eq!(out.status.code(), Some(1), "{case}: should refuse");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("refusing to prune"),
        "{case}: no refusal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn no_build_running_proceeds() {
    let (_dir, _owner, target) = checkout();
    assert_proceeds(&prune(&target, ""), "no process");
}

#[test]
fn a_build_in_another_checkout_does_not_block() {
    let (dir, _owner, target) = checkout();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let listing = format!(
        "cargo\t{}\tcargo check --workspace\nrustc\t{}\trustc --crate-name sittir_node_macros --out-dir {}/elsewhere/target/debug/deps\n",
        elsewhere.display(),
        elsewhere.display(),
        dir.path().display()
    );
    assert_proceeds(&prune(&target, &listing), "another checkout");
}

#[test]
fn a_cargo_inside_the_owning_checkout_blocks() {
    let (_dir, owner, target) = checkout();
    let worktree = owner.join("scratchpad").join("wt-x");
    let listing = format!(
        "cargo\t{}\tcargo test -p infigraph-core\n",
        worktree.display()
    );
    assert_refuses(&prune(&target, &listing), "cargo in the owning checkout");
}

#[test]
fn a_rustc_naming_the_target_blocks() {
    let (_dir, _owner, target) = checkout();
    let listing = format!(
        "rustc\t/elsewhere\trustc --crate-name x -L dependency={}/debug/deps\n",
        target.display()
    );
    assert_refuses(&prune(&target, &listing), "rustc naming the target");
}

#[test]
fn a_rustc_naming_another_target_does_not_block() {
    let (_dir, _owner, target) = checkout();
    let listing =
        "rustc\t/elsewhere\trustc --crate-name x -L dependency=/other/target/debug/deps\n";
    assert_proceeds(&prune(&target, listing), "rustc naming another target");
}

#[test]
fn an_unreadable_working_directory_blocks() {
    let (_dir, _owner, target) = checkout();
    let listing = "rustc\t?\trustc --crate-name x\n";
    assert_refuses(&prune(&target, listing), "unreadable cwd");
}

#[test]
fn a_bare_name_with_no_working_directory_blocks() {
    let (_dir, _owner, target) = checkout();
    assert_refuses(&prune(&target, "cargo\n"), "bare name, old-style list");
}
