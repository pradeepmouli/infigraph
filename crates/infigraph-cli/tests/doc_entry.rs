//! `infigraph doc-entry`, end to end against a real `infigraph daemon`: the
//! review and the document index both reach `docs.kuzu` through the daemon,
//! never from the CLI process.

mod support;

use std::path::Path;

use support::{assert_ok, run, start_daemon, stdout};

const DAEMON: &str = "daemon";

const DOC: &str = "# G\n\n### `app.py::f`\n\nWhy f.\n";

fn git(root: &Path, date: &str, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
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

/// `app.py` and its glossary committed on 2024-01-01, then `f` changed on
/// 2024-06-01: the entry is possibly stale.
fn stale_project() -> (tempfile::TempDir, tempfile::TempDir) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let root = project.path();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("app.py"), "def f():\n    return 1\n").unwrap();
    std::fs::write(root.join("docs/g.md"), DOC).unwrap();
    git(root, "2024-01-01T00:00:00Z", &["init", "-q"]);
    git(root, "2024-01-01T00:00:00Z", &["add", "app.py", "docs"]);
    git(
        root,
        "2024-01-01T00:00:00Z",
        &["commit", "-q", "-m", "initial"],
    );
    std::fs::write(root.join("app.py"), "def f():\n    return 2\n").unwrap();
    git(
        root,
        "2024-06-01T00:00:00Z",
        &["commit", "-q", "-am", "change f"],
    );
    (project, home)
}

#[test]
fn a_review_and_an_edit_go_through_the_daemon() {
    let (project, home) = stale_project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());
    assert_ok(
        &run(root, home.path(), DAEMON, &["index-docs"]),
        "index-docs",
    );

    let before = stdout(&run(root, home.path(), DAEMON, &["stale-docs"]));
    assert!(before.contains("1 of 1 entries possibly stale"), "{before}");

    let dry = run(
        root,
        home.path(),
        DAEMON,
        &["doc-entry", "ack", "app.py::f"],
    );
    assert_ok(&dry, "doc-entry ack (dry run)");
    assert!(stdout(&dry).contains("dry run"), "{}", stdout(&dry));
    let still = stdout(&run(root, home.path(), DAEMON, &["stale-docs"]));
    assert!(still.contains("1 of 1 entries possibly stale"), "{still}");

    let done = run(
        root,
        home.path(),
        DAEMON,
        &["doc-entry", "ack", "app.py::f", "--apply"],
    );
    assert_ok(&done, "doc-entry ack --apply");
    assert!(stdout(&done).contains("recorded"), "{}", stdout(&done));
    let after = stdout(&run(root, home.path(), DAEMON, &["stale-docs"]));
    assert!(after.contains("0 of 1 entries possibly stale"), "{after}");
    assert!(after.contains("1 acknowledged"), "{after}");

    let edited = run(
        root,
        home.path(),
        DAEMON,
        &[
            "doc-entry",
            "edit",
            "app.py::f",
            "--old",
            "Why f.",
            "--new",
            "Why f returns two.",
            "--apply",
        ],
    );
    assert_ok(&edited, "doc-entry edit --apply");
    assert_eq!(
        std::fs::read_to_string(root.join("docs/g.md")).unwrap(),
        "# G\n\n### `app.py::f`\n\nWhy f returns two.\n"
    );
    assert!(
        stdout(&edited).contains("document index refreshed"),
        "{}",
        stdout(&edited)
    );
}

#[test]
fn a_refusal_is_a_failed_command_that_says_why() {
    let (project, home) = stale_project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());
    assert_ok(
        &run(root, home.path(), DAEMON, &["index-docs"]),
        "index-docs",
    );

    let out = run(
        root,
        home.path(),
        DAEMON,
        &[
            "doc-entry",
            "edit",
            "app.py::f",
            "--old",
            "no such words",
            "--new",
            "plugh",
        ],
    );

    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not found"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An entry inside a code fence is not an entry. The refusal must say the
/// heading is there and which fence holds it, not that no entry exists --
/// whether the index was behind (the file was edited after it) or never saw it.
#[test]
fn an_entry_swallowed_by_a_fence_is_refused_with_the_fence_named() {
    let (project, home) = stale_project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());
    assert_ok(
        &run(root, home.path(), DAEMON, &["index-docs"]),
        "index-docs",
    );
    let words = |root: &Path| {
        run(
            root,
            home.path(),
            DAEMON,
            &[
                "doc-entry",
                "edit",
                "app.py::f",
                "--old",
                "Why f.",
                "--new",
                "x",
            ],
        )
    };

    // The index lists the entry; the file now holds it inside a fence.
    std::fs::write(
        root.join("docs/g.md"),
        "# G\n\n```text\n<sketch>\n\n### `app.py::f`\n\nWhy f.\n\n```\n",
    )
    .unwrap();
    let behind = words(root);
    assert!(!behind.status.success());
    let err = String::from_utf8_lossy(&behind.stderr).into_owned();
    assert!(
        err.contains(
            "docs/g.md:6 has that heading, but it is inside a code fence opened at line 3 and closed at line 10"
        ),
        "{err}"
    );

    // Indexed that way from the start: the index never listed it.
    assert_ok(
        &run(root, home.path(), DAEMON, &["index-docs"]),
        "index-docs again",
    );
    let never = words(root);
    let err = String::from_utf8_lossy(&never.stderr).into_owned();
    assert!(
        err.contains(
            "docs/g.md:6 has that heading, but it is inside a code fence opened at line 3 and closed at line 10"
        ),
        "{err}"
    );
}

#[test]
fn one_of_two_entries_with_a_key_is_chosen_with_nth() {
    let (project, home) = stale_project();
    let root = project.path();
    std::fs::write(
        root.join("docs/g.md"),
        "# G\n\n### `app.py::f`\n\nWhy f.\n\n### `app.py::f`\n\nWhy f again.\n",
    )
    .unwrap();
    let _daemon = start_daemon(root, home.path());
    assert_ok(
        &run(root, home.path(), DAEMON, &["index-docs"]),
        "index-docs",
    );

    let both = run(
        root,
        home.path(),
        DAEMON,
        &["doc-entry", "delete", "app.py::f", "--force"],
    );
    assert!(!both.status.success());
    let why = String::from_utf8_lossy(&both.stderr).into_owned();
    assert!(
        why.contains("docs/g.md:3") && why.contains("docs/g.md:7") && why.contains("`nth`"),
        "{why}"
    );

    let done = run(
        root,
        home.path(),
        DAEMON,
        &[
            "doc-entry",
            "delete",
            "app.py::f",
            "--force",
            "--nth",
            "2",
            "--apply",
        ],
    );
    assert_ok(&done, "doc-entry delete --nth 2 --apply");
    assert_eq!(
        std::fs::read_to_string(root.join("docs/g.md")).unwrap(),
        "# G\n\n### `app.py::f`\n\nWhy f.\n"
    );
}

#[test]
fn a_linked_worktree_without_an_index_is_refused_not_resolved_to_the_main_checkout() {
    let (project, home) = stale_project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());
    assert_ok(
        &run(root, home.path(), DAEMON, &["index-docs"]),
        "index-docs",
    );
    git(
        root,
        "2024-06-02T00:00:00Z",
        &["worktree", "add", "-q", "-b", "topic", "scratchpad/wt-x"],
    );
    let wt = root.join("scratchpad/wt-x");
    let before = std::fs::read_to_string(root.join("docs/g.md")).unwrap();

    let out = run(
        &wt,
        home.path(),
        DAEMON,
        &[
            "doc-entry",
            "edit",
            "app.py::f",
            "--old",
            "Why f.",
            "--new",
            "Changed.",
            "--apply",
        ],
    );

    assert!(!out.status.success());
    let why = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(why.contains("no index of its own"), "{why}");
    assert_eq!(
        std::fs::read_to_string(root.join("docs/g.md")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("docs/g.md")).unwrap(),
        before
    );
}

/// The hook script as installed: Markdown edits reach `infigraph doc-guard`,
/// everything else passes without running it.
#[cfg(unix)]
mod hook_script {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn jq_available() -> bool {
        Command::new("jq")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// The script's stdout for `input`, with the freshly built `infigraph` first
    /// on PATH.
    fn hook(input: &serde_json::Value) -> String {
        let script = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/resources/integrations/claude-code/.claude/hooks/infigraph-doc-guard.sh"
        );
        let bin_dir = std::path::Path::new(env!("CARGO_BIN_EXE_infigraph"))
            .parent()
            .unwrap()
            .to_path_buf();
        let path = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new("bash")
            .arg(script)
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "the hook must never fail");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    #[test]
    fn the_script_denies_an_entry_edit_and_passes_everything_else() {
        if !jq_available() {
            eprintln!("jq is not installed: the hook script needs it, skipping");
            return;
        }
        let (project, home) = stale_project();
        let root = project.path();
        assert_ok(
            &run(
                root,
                home.path(),
                infigraph_core::LOCAL_BACKEND,
                &["index-docs"],
            ),
            "index-docs",
        );
        let edit = |file: &str, old: &str| {
            serde_json::json!({
                "tool_name": "Edit", "cwd": root,
                "tool_input": {"file_path": root.join(file), "old_string": old, "new_string": "x"},
            })
        };

        let denied = hook(&edit("docs/g.md", "Why f."));
        let decision: serde_json::Value = serde_json::from_str(denied.trim())
            .unwrap_or_else(|e| panic!("not a decision ({e}): {denied:?}"));
        let out = &decision["hookSpecificOutput"];
        assert_eq!(out["hookEventName"], "PreToolUse");
        assert_eq!(out["permissionDecision"], "deny");
        let why = out["permissionDecisionReason"].as_str().unwrap();
        assert!(
            why.contains("app.py::f") && why.contains("doc_entry"),
            "{why}"
        );

        assert_eq!(hook(&edit("docs/g.md", "# G")), "", "outside every entry");
        assert_eq!(hook(&edit("app.py", "return 2")), "", "not Markdown");
        assert_eq!(hook(&serde_json::json!({"tool_name": "Bash"})), "");
    }
}
