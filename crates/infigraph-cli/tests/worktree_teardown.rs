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
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["--root", root.to_str().unwrap(), "index"])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .unwrap()
}

fn run_worktree(
    action: &str,
    path: &std::path::Path,
    fake_home: &std::path::Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_infigraph"))
        .args(["worktree", action, path.to_str().unwrap()])
        .env("HOME", fake_home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_BACKEND")
        .output()
        .unwrap()
}

#[test]
fn worktree_teardown_evicts_registry_entry_but_keeps_infigraph_dir() {
    let fake_home = tempfile::tempdir().unwrap();
    let main = tempfile::tempdir().unwrap();
    git(&["init"], main.path());
    git(&["config", "user.email", "t@t.com"], main.path());
    git(&["config", "user.name", "t"], main.path());
    std::fs::write(main.path().join("a.py"), "def foo():\n    pass\n").unwrap();
    git(&["add", "a.py"], main.path());
    git(&["commit", "-m", "init"], main.path());

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

    // Register the worktree the honest way: index it for real.
    let out = run_index(&wt_path, fake_home.path());
    assert!(out.status.success());
    let registry_path = fake_home.path().join(".infigraph/registry.json");
    let before = std::fs::read_to_string(&registry_path).unwrap();
    assert!(before.contains(wt_path.file_name().unwrap().to_str().unwrap()));

    let out = run_worktree("teardown", &wt_path, fake_home.path());
    assert!(
        out.status.success(),
        "teardown failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let after = std::fs::read_to_string(&registry_path).unwrap();
    assert!(
        !after.contains(&wt_path.to_string_lossy().to_string()),
        "registry.json should no longer reference {}:\n{after}",
        wt_path.display()
    );
    assert!(
        wt_path.join(".infigraph/embeddings.bin").exists(),
        ".infigraph/ must survive teardown"
    );
}
