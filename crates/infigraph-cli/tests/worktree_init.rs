mod support;

use std::process::Command;

fn git(args: &[&str], cwd: &std::path::Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn run_index(root: &std::path::Path, fake_home: &std::path::Path) -> std::process::Output {
    support::infigraph()
        .args(["--root", root.to_str().unwrap(), "index"])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .output()
        .unwrap()
}

fn run_worktree(
    action: &str,
    path: &std::path::Path,
    fake_home: &std::path::Path,
) -> std::process::Output {
    support::infigraph()
        .args(["worktree", action, path.to_str().unwrap()])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .output()
        .unwrap()
}

#[test]
fn worktree_init_clones_and_incrementally_indexes() {
    let fake_home = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    git(&["init"], main.path());
    git(&["config", "user.email", "t@t.com"], main.path());
    git(&["config", "user.name", "t"], main.path());
    std::fs::write(main.path().join("a.py"), "def foo():\n    pass\n").unwrap();
    git(&["add", "a.py"], main.path());
    git(&["commit", "-m", "init"], main.path());

    // Index the main worktree first (full embeddings, not --no-embed, since this
    // test verifies embedding-clone correctness).
    let out = run_index(main.path(), fake_home.path());
    assert!(
        out.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(main.path().join(".infigraph/graph").exists());
    let main_embeddings = std::fs::read(main.path().join(".infigraph/embeddings.bin")).unwrap();

    // Create a linked worktree with identical content (no new commits on it yet).
    let parent = tempfile::tempdir().unwrap();
    let wt_path = parent.path().join("wt1");
    git(
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            wt_path.to_str().unwrap(),
        ],
        main.path(),
    );

    let out = run_worktree("init", &wt_path, fake_home.path());
    assert!(
        out.status.success(),
        "worktree init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(wt_path.join(".infigraph/graph").exists());

    // Content-correctness check: since the new worktree has zero file differences
    // from main at creation time, its cloned-then-incrementally-reindexed
    // embeddings.bin must be byte-for-byte identical to main's -- not just
    // "close," which would be the case if vectors were regenerated instead of
    // reused (float re-embedding of identical text is deterministic here, but
    // exact byte equality is still the strongest available signal that the
    // clone was actually used rather than silently discarded).
    let wt_embeddings = std::fs::read(wt_path.join(".infigraph/embeddings.bin")).unwrap();
    assert_eq!(main_embeddings, wt_embeddings);
}

/// The main graph's restore pools -- snapshots, a quarantined graph, a retired
/// one -- are its history, not the worktree's: a fresh worktree can re-clone
/// from main, and carrying them cost ~2 GiB in every one of 34 worktrees. The
/// graph itself is still cloned and answers queries.
#[test]
fn worktree_init_leaves_the_mains_restore_pools_behind() {
    let fake_home = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    git(&["init"], main.path());
    git(&["config", "user.email", "t@t.com"], main.path());
    git(&["config", "user.name", "t"], main.path());
    std::fs::write(main.path().join(".gitignore"), ".infigraph/\n.claude/\n").unwrap();
    std::fs::write(main.path().join("a.py"), "def foo_marker():\n    pass\n").unwrap();
    git(&["add", "."], main.path());
    git(&["commit", "-m", "init"], main.path());
    let out = run_index(main.path(), fake_home.path());
    support::assert_ok(&out, "index");

    let ig = main.path().join(".infigraph");
    let pools = [
        "snapshots/1/graph",
        "graph.previous.1791397452",
        "graph.previous.1791397452.wal",
        "graph.corrupt.1791226834",
        "graph.corrupt.1791226834.wal",
    ];
    for pool in pools {
        let path = ig.join(pool);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; 4096]).unwrap();
    }

    let parent = tempfile::tempdir().unwrap();
    let wt_path = parent.path().join("wt1");
    git(
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            wt_path.to_str().unwrap(),
        ],
        main.path(),
    );
    let out = run_worktree("init", &wt_path, fake_home.path());
    support::assert_ok(&out, "worktree init");

    let wt_ig = wt_path.join(".infigraph");
    assert!(wt_ig.join("graph").exists(), "the graph was not cloned");
    for pool in pools {
        assert!(!wt_ig.join(pool).exists(), "{pool} was cloned");
    }
    assert!(
        !wt_ig.join("snapshots").exists(),
        "an empty snapshots/ was left behind"
    );
    let found = support::run(
        &wt_path,
        fake_home.path(),
        infigraph_core::LOCAL_BACKEND,
        &["search", "foo_marker"],
    );
    support::assert_ok(&found, "search in the new worktree");
    assert!(
        support::stdout(&found).contains("foo_marker"),
        "the cloned graph does not answer: {}",
        support::stdout(&found)
    );
    // The source keeps its pools.
    for pool in pools {
        assert!(ig.join(pool).exists(), "{pool} vanished from main");
    }
}
