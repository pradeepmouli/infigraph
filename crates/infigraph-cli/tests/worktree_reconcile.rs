use std::process::Command;

/// Runs a git command, and on failure reports what git actually said.
///
/// This used to call `.status()`, which sends git's output to the test
/// harness's own stdio and keeps none of it, so a failure could only ever
/// panic with "git [...] failed" -- no exit code, no message. When
/// `worktree remove --force` failed once on a macOS runner that was the
/// entire evidence, and the reason (a still-open file? a locked worktree?
/// a path git recorded differently under /private/var?) was unrecoverable
/// after the fact. Capturing it costs nothing and makes the next
/// occurrence diagnosable from the CI log alone.
fn git(args: &[&str], cwd: &std::path::Path) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed ({})\nstdout: {}\nstderr: {}",
        args,
        out.status,
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim(),
    );
}

fn run_index(root: &std::path::Path, fake_home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["--root", root.to_str().unwrap(), "index"])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .unwrap()
}

fn run_worktree_reconcile(
    cwd: &std::path::Path,
    fake_home: &std::path::Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["worktree", "reconcile"])
        .current_dir(cwd)
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .unwrap()
}

/// Removes a worktree that has been indexed, without racing infigraph's own
/// writer.
///
/// `git worktree remove --force` failed intermittently on macOS runners with
/// `failed to delete '...': Directory not empty`. Nothing here is ignored by
/// git -- the fixture repo has no `.gitignore` -- so `--force` really does
/// delete the untracked `.infigraph/` scratch. "Not empty" means a file
/// reappeared between git's delete pass and its final `rmdir`: indexing a
/// root auto-starts a daemon for it, and that daemon goes on writing into
/// `.infigraph/` after `infigraph index` has already exited.
///
/// So stop the writer rather than retry around it. The stop is asynchronous
/// (the CLI's own wording is that the process exits "within ~1 second"),
/// which is the only thing the bounded retry below is for -- and if it never
/// succeeds, it reports git's actual stderr rather than a bare "failed".
fn remove_worktree(
    main: &std::path::Path,
    worktree: &std::path::Path,
    fake_home: &std::path::Path,
) {
    let _ = Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["--root", worktree.to_str().unwrap(), "daemon-stop"])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .output();

    let args = ["worktree", "remove", "--force", worktree.to_str().unwrap()];
    let mut last = String::new();
    for attempt in 0..10 {
        let out = Command::new("git")
            .args(args)
            .current_dir(main)
            .output()
            .unwrap();
        if out.status.success() {
            return;
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        std::thread::sleep(std::time::Duration::from_millis(200 * (attempt + 1)));
    }
    panic!("git {args:?} never succeeded after stopping the daemon; last stderr: {last}");
}

#[test]
fn reconcile_acts_on_teardown_but_only_reports_bootstrap() {
    let fake_home = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    git(&["init"], main.path());
    git(&["config", "user.email", "t@t.com"], main.path());
    git(&["config", "user.name", "t"], main.path());
    std::fs::write(main.path().join("a.py"), "def foo():\n    pass\n").unwrap();
    git(&["add", "a.py"], main.path());
    git(&["commit", "-m", "init"], main.path());
    let out = run_index(main.path(), fake_home.path());
    assert!(out.status.success());

    let parent = tempfile::tempdir().unwrap();

    // Teardown candidate: register it, then remove the worktree without telling infigraph.
    let removed_wt = parent.path().join("removed-wt");
    git(
        &[
            "worktree",
            "add",
            "-b",
            "gone",
            removed_wt.to_str().unwrap(),
        ],
        main.path(),
    );
    let out = run_index(&removed_wt, fake_home.path());
    assert!(out.status.success());
    remove_worktree(main.path(), &removed_wt, fake_home.path());

    // Bootstrap candidate: create a worktree but never index it.
    let new_wt = parent.path().join("new-wt");
    git(
        &["worktree", "add", "-b", "fresh", new_wt.to_str().unwrap()],
        main.path(),
    );

    let out = run_worktree_reconcile(main.path(), fake_home.path());
    assert!(
        out.status.success(),
        "reconcile failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Teardown candidate: actually evicted.
    let registry_content =
        std::fs::read_to_string(fake_home.path().join(".infigraph/registry.json")).unwrap();
    assert!(!registry_content.contains(&removed_wt.to_string_lossy().to_string()));

    // Bootstrap candidate: reported, but not auto-indexed. git reports worktree
    // paths in canonicalized form (e.g. macOS's /var -> /private/var symlink),
    // which can differ textually from the raw path this test constructed even
    // though they're the same directory -- compare canonicalized forms.
    let new_wt_canon = new_wt.canonicalize().unwrap();
    assert!(stdout.contains(&format!(
        "infigraph worktree init {}",
        new_wt_canon.display()
    )));
    assert!(
        !new_wt.join(".infigraph").exists(),
        "reconcile must not auto-bootstrap"
    );
}
