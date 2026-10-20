//! A project indexed by the previous build keeps its code graph under the old
//! name. Commands that decide from "does the graph exist" must see it, and
//! only a command that may write renames it.

mod support;

use std::path::Path;
use std::process::{Command, Output};

fn infigraph_in(root: &Path, home: &Path, args: &[&str]) -> Output {
    support::infigraph()
        .arg("--root")
        .arg(root)
        .args(args)
        .env("HOME", home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .output()
        .unwrap()
}

/// An indexed project whose `.infigraph/` is then put back the way the
/// previous build would have left it: every `code.kuzu*` file as `graph*`.
fn project_indexed_by_the_previous_build(home: &Path) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(root.path())
            .status()
            .unwrap()
            .success());
    };
    git(&["init"]);
    std::fs::write(root.path().join("a.py"), "def legacy_marker():\n    pass\n").unwrap();
    let out = infigraph_in(root.path(), home, &["index", "--no-embed"]);
    support::assert_ok(&out, "index");
    let ig = root.path().join(".infigraph");
    let stem = infigraph_core::graph::names::STEM;
    for e in std::fs::read_dir(&ig).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(rest) = name.strip_prefix(stem) {
            if rest.ends_with(".lock") {
                std::fs::remove_file(e.path()).unwrap();
            } else {
                std::fs::rename(e.path(), ig.join(format!("graph{rest}"))).unwrap();
            }
        }
    }
    assert!(ig.join("graph").exists() && !ig.join(stem).exists());
    root
}

fn old_names_left(ig: &Path) -> Vec<String> {
    std::fs::read_dir(ig)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| infigraph_core::graph::names::legacy::is_entry(n))
        .collect()
}

#[test]
fn index_renames_a_legacy_store_instead_of_forking_it() {
    let home = tempfile::tempdir().unwrap();
    let root = project_indexed_by_the_previous_build(home.path());
    let ig = root.path().join(".infigraph");

    let snapshots = |ig: &Path| {
        std::fs::read_dir(ig.join("snapshots"))
            .map(|d| d.count())
            .unwrap_or(0)
    };
    let before = snapshots(&ig);
    let out = infigraph_in(root.path(), home.path(), &["index", "--no-embed"]);
    support::assert_ok(&out, "index over a legacy store");
    // A full rebuild files a snapshot of what it replaces; an incremental
    // index does not. Judging "no graph" before the rename would promote this
    // run to a full rebuild.
    assert_eq!(
        snapshots(&ig),
        before,
        "the legacy store was rebuilt instead of renamed"
    );

    assert!(
        infigraph_core::graph::names::live(&ig).exists(),
        "the store is under the new name"
    );
    assert_eq!(old_names_left(&ig), Vec::<String>::new());
    let found = infigraph_in(root.path(), home.path(), &["search", "legacy_marker"]);
    support::assert_ok(&found, "search");
    assert!(
        String::from_utf8_lossy(&found.stdout).contains("legacy_marker"),
        "the indexed symbol came across the rename: {}",
        String::from_utf8_lossy(&found.stdout)
    );
}

#[test]
fn doctor_reports_a_legacy_store_as_pending_and_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    let root = project_indexed_by_the_previous_build(home.path());
    let ig = root.path().join(".infigraph");

    let out = infigraph_in(root.path(), home.path(), &["doctor"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("store-migration-pending"), "{text}");
    assert!(
        !infigraph_core::graph::names::live(&ig).exists(),
        "doctor must not create or rename a store"
    );
    assert!(ig.join("graph").exists());
}
