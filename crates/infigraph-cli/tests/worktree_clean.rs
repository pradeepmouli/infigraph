//! `infigraph worktree clean` (#210) through the real binary: a repo with an
//! indexed, merged worktree. The in-process behaviour (every skip reason, the
//! allow-list, the re-check) is covered where the logic lives, in
//! `infigraph-core/src/worktree_clean.rs`; this is what only the binary shows:
//! the default is a dry run, `--apply` evicts and deletes, `doctor` stays quiet
//! afterwards, a cleaned worktree can be re-bootstrapped, and a worktree with a
//! live daemon is left alone.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use infigraph_core::last_run::{self, Kind, RunRecord, Tally};

fn git(dir: &Path, args: &[&str]) {
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
}

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    main: PathBuf,
    wt: PathBuf,
}

/// A repo whose `.gitignore` covers what infigraph writes into a project (so a
/// finished worktree is not made "dirty" by its own index) and the `.DS_Store`
/// macOS drops into temp directories, with a merged
/// worktree that has been indexed and registered.
fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let home = base.join("home");
    let main = base.join("main");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(
        main.join(".gitignore"),
        ".infigraph/\n.claude/\ntarget/\nindex.scip*\n.DS_Store\n",
    )
    .unwrap();
    std::fs::write(main.join("a.py"), "def alpha():\n    return 1\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-qm", "init"]);
    let wt = base.join("wt-a");
    git(
        &main,
        &["worktree", "add", "-q", "-b", "fix-a", wt.to_str().unwrap()],
    );
    std::fs::write(wt.join("b.py"), "def beta():\n    return 2\n").unwrap();
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-qm", "b"]);
    git(
        &main,
        &["merge", "-q", "--no-ff", "-m", "merge fix-a", "fix-a"],
    );

    for root in [&main, &wt] {
        let indexed = run(root, &home, &["index", "--no-embed"]);
        support::assert_ok(&indexed, "index");
    }
    Fixture {
        _tmp: tmp,
        home,
        main,
        wt,
    }
}

fn run(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    support::run(cwd, home, infigraph_core::LOCAL_BACKEND, args)
}

#[test]
fn the_default_is_a_dry_run_that_changes_nothing() {
    let f = fixture();

    let out = run(&f.main, &f.home, &["worktree", "clean"]);

    support::assert_ok(&out, "worktree clean");
    let text = support::stdout(&out);
    assert!(text.contains("dry run"), "{text}");
    assert!(text.contains(f.wt.to_str().unwrap()), "{text}");
    assert!(text.contains("eligible"), "{text}");
    assert!(text.contains("skipped: the main worktree"), "{text}");
    assert!(
        f.wt.join(".infigraph").join("graph").exists(),
        "a dry run deleted the graph"
    );
}

#[test]
fn apply_cleans_the_worktree_and_doctor_stays_quiet_about_it() {
    let f = fixture();
    let ig = f.wt.join(".infigraph");
    // Something worth reporting about the run that built the graph, and the
    // user's own files.
    last_run::record(
        &ig,
        Kind::Index,
        RunRecord::new(false, "drain failed", Tally::default()),
    );
    assert!(last_run::read(&ig, Kind::Index)
        .unwrap()
        .last_problem
        .is_some());
    std::fs::write(ig.join("config.toml"), "# mine\n").unwrap();
    std::fs::create_dir_all(ig.join("sessions")).unwrap();
    std::fs::write(ig.join("sessions").join("s.md"), "notes").unwrap();

    let out = run(&f.main, &f.home, &["worktree", "clean", "--apply"]);

    support::assert_ok(&out, "worktree clean --apply");
    assert!(
        !ig.join("graph").exists(),
        "the graph survived:\n{}\ngit status:\n{}",
        support::stdout(&out),
        String::from_utf8_lossy(
            &Command::new("git")
                .args(["status", "--porcelain", "--ignored=no"])
                .current_dir(&f.wt)
                .output()
                .unwrap()
                .stdout
        )
    );
    assert!(
        last_run::read(&ig, Kind::Index).is_none(),
        "the record of the run that built the removed graph survived"
    );
    assert!(ig.join("config.toml").exists());
    assert!(ig.join("sessions").join("s.md").exists());
    let registry = std::fs::read_to_string(f.home.join(".infigraph/registry.json")).unwrap();
    assert!(
        !registry.contains("wt-a"),
        "a cleaned worktree stays registered:\n{registry}"
    );

    // Neither scope of doctor has anything to say about the cleaned worktree.
    for (cwd, args) in [
        (&f.wt, vec!["doctor"]),
        (&f.main, vec!["doctor", "--global"]),
    ] {
        let report = run(cwd, &f.home, &args);
        let text = format!(
            "{}{}",
            support::stdout(&report),
            String::from_utf8_lossy(&report.stderr)
        );
        let complaints: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("wt-a") && (l.starts_with("[!]") || l.starts_with("[✗]")))
            .collect();
        assert!(
            complaints.is_empty(),
            "doctor {args:?} complains about the cleaned worktree:\n{}",
            complaints.join("\n")
        );
    }
}

#[test]
fn a_cleaned_worktree_can_be_bootstrapped_again() {
    let f = fixture();
    support::assert_ok(
        &run(&f.main, &f.home, &["worktree", "clean", "--apply"]),
        "clean",
    );
    assert!(!f.wt.join(".infigraph").join("graph").exists());

    let out = run(
        &f.main,
        &f.home,
        &["worktree", "init", f.wt.to_str().unwrap()],
    );

    support::assert_ok(&out, "worktree init on a cleaned worktree");
    assert!(
        f.wt.join(".infigraph").join("graph").exists(),
        "init did not rebuild the graph"
    );
}

#[test]
fn a_worktree_with_a_live_daemon_is_left_alone() {
    let f = fixture();
    let _daemon = support::start_daemon(&f.wt, &f.home);

    let out = run(&f.main, &f.home, &["worktree", "clean", "--apply"]);

    support::assert_ok(&out, "worktree clean --apply");
    let text = support::stdout(&out);
    assert!(text.contains("in use"), "{text}");
    assert!(
        f.wt.join(".infigraph").join("graph").exists(),
        "the graph a live daemon holds was deleted"
    );
}
