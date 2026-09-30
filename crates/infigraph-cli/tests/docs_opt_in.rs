//! Document indexing is opt-in per project
//! (docs/superpowers/specs/2026-09-29-docs-opt-in-design.md), end to end
//! against a real `infigraph daemon`.

mod support;

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use infigraph_core::docs_switch::{docs_enabled_recorded, docs_store_path};

const DAEMON: &str = "daemon";

/// Kills and reaps the daemon on every exit path, panics included.
struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A project with one source file and one document, and a scratch `HOME`
/// so neither the registry nor a developer's `~/.infigraph/config.toml`
/// leaks in.
fn project() -> (tempfile::TempDir, tempfile::TempDir) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n").unwrap();
    std::fs::write(
        project.path().join("README.md"),
        "# Hello\n\nThe zebra-crossing handbook.\n",
    )
    .unwrap();
    (project, home)
}

/// Run one CLI command. `INFIGRAPH_NO_WATCH` keeps the pre-dispatch
/// auto-watch from starting a daemon the test did not ask for.
fn run(root: &Path, home: &Path, backend: &str, args: &[&str]) -> Output {
    support::infigraph()
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env(infigraph_core::BACKEND_ENV, backend)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_DOCS_ENABLED")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn assert_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed:\nstdout={}\nstderr={}",
        stdout(out),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The number after `label` on its line of a report (`Files indexed: 1`).
fn count(text: &str, label: &str) -> usize {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(label))
        .and_then(|rest| rest.trim().parse().ok())
        .unwrap_or_else(|| panic!("no `{label}` line in:\n{text}"))
}

/// Index the code locally, then start a real daemon with a fast doc poll
/// and wait until it holds `watch.lock`.
fn start_daemon(root: &Path, home: &Path) -> Daemon {
    let bootstrap = run(
        root,
        home,
        infigraph_core::LOCAL_BACKEND,
        &["index", "--no-embed"],
    );
    assert_ok(&bootstrap, "bootstrap index");
    let daemon = Daemon(
        support::infigraph()
            .args(["daemon", "--debounce", "50"])
            .current_dir(root)
            .env("HOME", home)
            .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
            .env("INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS", "50")
            .env_remove("INFIGRAPH_DOCS_ENABLED")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(
            &root.join(".infigraph").join("watch.lock"),
            Duration::from_secs(30)
        ),
        "the daemon never took watch.lock"
    );
    daemon
}

/// Poll `check` for up to `budget`.
fn eventually(budget: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check()
}

/// Spec decision 2: a project that already has a document index keeps it
/// on. The daemon records the switch at start.
#[test]
fn an_existing_index_is_kept_on_when_the_daemon_starts() {
    let (project, home) = project();
    let root = project.path();
    // The code index first: `index` on a project whose `.infigraph/` has no
    // graph yet auto-promotes to a full rebuild, which wipes `.infigraph/`
    // -- docs store included -- so a store made before it would be gone by
    // the time the daemon starts. After this, `start_daemon`'s bootstrap
    // index is incremental and leaves the store alone.
    assert_ok(
        &run(
            root,
            home.path(),
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "code index",
    );
    // A real store, created the way every pre-opt-in project got one. The
    // test process runs with INFIGRAPH_BACKEND=kuzu pinned by the command.
    infigraph_docs::DocIndex::open(root)
        .unwrap()
        .init()
        .unwrap();
    assert_eq!(docs_enabled_recorded(root), None);

    let _daemon = start_daemon(root, home.path());
    assert!(
        eventually(Duration::from_secs(10), || docs_enabled_recorded(root)
            == Some(true)),
        "the daemon must record [docs] enabled = true for an existing index"
    );
}

/// The side effect the spec exists for: a daemon on a fresh project creates
/// no document index, across many doc-thread polls.
#[test]
fn a_fresh_daemon_creates_no_document_index() {
    let (project, home) = project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());
    // 20 doc-thread polls at 50ms.
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        !docs_store_path(root).exists(),
        "the daemon created docs.kuzu"
    );
    assert_eq!(docs_enabled_recorded(root), None);
}

#[test]
fn index_docs_under_the_daemon_backend_indexes_opts_in_and_is_searchable() {
    let (project, home) = project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());

    let out = run(root, home.path(), DAEMON, &["index-docs"]);
    assert_ok(&out, "index-docs");
    let text = stdout(&out);
    assert!(count(&text, "Files indexed:") >= 1, "{text}");
    assert!(count(&text, "Total documents in store:") >= 1, "{text}");
    assert_eq!(docs_enabled_recorded(root), Some(true));

    let found = run(root, home.path(), DAEMON, &["search-docs", "zebra"]);
    assert_ok(&found, "search-docs");
    assert!(stdout(&found).contains("README.md"), "{}", stdout(&found));
}

#[test]
fn index_docs_with_the_daemon_opted_out_indexes_in_process() {
    let (project, home) = project();
    let root = project.path();
    let local = infigraph_core::LOCAL_BACKEND;

    let out = run(root, home.path(), local, &["index-docs"]);
    assert_ok(&out, "index-docs");
    assert!(
        count(&stdout(&out), "Files indexed:") >= 1,
        "{}",
        stdout(&out)
    );
    assert_eq!(docs_enabled_recorded(root), Some(true));
    assert!(docs_store_path(root).exists());

    let found = run(root, home.path(), local, &["search-docs", "zebra"]);
    assert_ok(&found, "search-docs");
    assert!(stdout(&found).contains("README.md"), "{}", stdout(&found));
}

#[test]
fn reindex_docs_rebuilds_through_the_daemon() {
    let (project, home) = project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());
    assert_ok(
        &run(root, home.path(), DAEMON, &["index-docs"]),
        "index-docs",
    );

    let again = run(root, home.path(), DAEMON, &["index-docs"]);
    assert_ok(&again, "second index-docs");
    assert_eq!(
        count(&stdout(&again), "Files indexed:"),
        0,
        "an incremental run skips unchanged files"
    );

    let full = run(root, home.path(), DAEMON, &["reindex-docs"]);
    assert_ok(&full, "reindex-docs");
    assert!(stdout(&full).contains("full reindex"), "{}", stdout(&full));
    assert!(
        count(&stdout(&full), "Files indexed:") >= 1,
        "a full rebuild re-indexes every file: {}",
        stdout(&full)
    );
}

/// A code index refreshes documents only for a project that has opted in:
/// `infigraph index` on a fresh project must not opt it in on its own.
#[test]
fn a_code_index_does_not_opt_a_project_in_to_documents() {
    let (project, home) = project();
    let root = project.path();

    // Not `--no-embed`: that returns before `index`'s document step.
    let out = run(root, home.path(), infigraph_core::LOCAL_BACKEND, &["index"]);
    assert_ok(&out, "index");

    assert_eq!(docs_enabled_recorded(root), None);
    assert!(
        !docs_store_path(root).exists(),
        "a code index created docs.kuzu"
    );
}

#[test]
fn clean_docs_turns_docs_off_and_the_daemon_does_not_bring_them_back() {
    let (project, home) = project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());
    assert_ok(
        &run(root, home.path(), DAEMON, &["index-docs"]),
        "index-docs",
    );
    assert!(docs_store_path(root).exists());

    let out = run(root, home.path(), DAEMON, &["clean-docs"]);
    assert_ok(&out, "clean-docs");
    assert_eq!(docs_enabled_recorded(root), Some(false));
    assert!(!docs_store_path(root).exists());

    // A new document, then many doc-thread polls: nothing comes back.
    std::fs::write(root.join("NEW.md"), "# New\n\nanother page\n").unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !docs_store_path(root).exists(),
        "the daemon re-created docs.kuzu after clean-docs"
    );
}

/// Spec: `search_docs` on a project that is not enabled answers with the
/// message, and the search itself creates nothing.
#[test]
fn search_docs_on_a_fresh_project_says_how_to_opt_in_and_creates_nothing() {
    let (project, home) = project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());

    let out = run(root, home.path(), DAEMON, &["search-docs", "zebra"]);
    assert_ok(&out, "search-docs");
    assert!(
        stdout(&out).contains(infigraph_core::docs_switch::DOCS_NOT_INDEXED),
        "{}",
        stdout(&out)
    );
    assert!(!docs_store_path(root).exists());
    assert_eq!(docs_enabled_recorded(root), None);
}

/// The group build's document step (step 5), through the real CLI: a repo that
/// opted in has its documents refreshed, and a repo that did not is left
/// with no store and no switch.
#[test]
fn group_build_refreshes_only_the_repos_that_opted_in() {
    let (opted_in, home) = project();
    let (other, _other_home) = project();
    let (a, b) = (opted_in.path(), other.path());

    assert_ok(&run(a, home.path(), "kuzu", &["index-docs"]), "index-docs");
    std::fs::write(a.join("second.md"), "# Second\n\nThe quokka appendix.\n").unwrap();

    assert_ok(
        &run(a, home.path(), "kuzu", &["group", "create", "g"]),
        "group create",
    );
    for repo in [a, b] {
        assert_ok(
            &run(
                a,
                home.path(),
                "kuzu",
                &["group", "add", "g", repo.to_str().unwrap()],
            ),
            "group add",
        );
    }
    assert_ok(
        &run(a, home.path(), "kuzu", &["group", "build", "g"]),
        "group build",
    );

    let found = run(a, home.path(), "kuzu", &["search-docs", "quokka"]);
    assert_ok(&found, "search-docs");
    assert!(stdout(&found).contains("quokka"), "{}", stdout(&found));
    assert!(
        !b.join(".infigraph/docs.kuzu").exists(),
        "a repo that never opted in got a document index from the group build"
    );
    assert!(
        !b.join(".infigraph/config.toml").exists()
            || !{
                std::fs::read_to_string(b.join(".infigraph/config.toml"))
                    .unwrap()
                    .contains("[docs]")
            }
    );
}
