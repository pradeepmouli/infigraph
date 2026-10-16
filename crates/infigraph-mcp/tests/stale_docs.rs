//! `stale_docs`: which glossary entries describe code that changed after they
//! did, and what changed, so an agent knows what to edit in the markdown.
//!
//! A project here is a tiny Python tree in a temp git repository, its graph,
//! and its document index, built with the local backend.

use std::path::Path;

use serde_json::{json, Value};

use infigraph_mcp::tools::doc_sync::tool_stale_docs;

mod support;

use support::docs::{
    code_project, commit_as, git, index_documents, project, reindex_code, write, APP,
};

const GLOSSARY: &str = "# Glossary\n\n\
### `src/app.py::compute_total`\n\nSums the items in a loop, not with sum(): items may be generators.\n\n\
### `src/app.py::Cart::add`\n\nAppends to the cart; never merges duplicates.\n";

const FIRST: &str = "2024-01-01T00:00:00Z";

fn stale_docs(dir: &Path, args: Value) -> String {
    let mut args = args;
    args["path"] = json!(dir.to_string_lossy());
    tool_stale_docs(&args).unwrap()
}

/// `APP` and `GLOSSARY` committed together on `FIRST`, documents indexed.
fn committed_project() -> tempfile::TempDir {
    let dir = code_project(&[], &[("docs/glossary/src.md", GLOSSARY)]);
    git(dir.path(), &["init", "-q"], FIRST);
    commit_as(dir.path(), FIRST, Some("initial import"));
    index_documents(dir.path());
    dir
}

/// Edits `compute_total` and commits it as `message` on `date`.
fn change_total(dir: &Path, from: &str, to: &str, date: &str, message: &str) {
    let now = std::fs::read_to_string(dir.join("src/app.py")).unwrap();
    write(dir, "src/app.py", &now.replace(from, to));
    commit_as(dir, date, Some(message));
    reindex_code(dir);
}

#[test]
fn a_symbol_changed_after_its_entry_is_listed_with_the_commit_and_the_lines_that_changed() {
    let dir = committed_project();
    change_total(
        dir.path(),
        "total += item\n",
        "total += item * 2\n",
        "2024-06-01T00:00:00Z",
        "double every item",
    );

    let out = stale_docs(dir.path(), json!({}));

    assert!(out.contains("1 of 2 entries possibly stale"), "{out}");
    assert!(
        out.contains("src/app.py::compute_total") && out.contains("docs/glossary/src.md:3"),
        "the entry and its document are not named:\n{out}"
    );
    assert!(
        out.contains("double every item") && out.contains("2024-06-01"),
        "no commit subject or date:\n{out}"
    );
    assert!(
        out.contains("entry last touched 2024-01-01"),
        "no entry date:\n{out}"
    );
    assert!(
        out.contains("-        total += item\n") && out.contains("+        total += item * 2\n"),
        "no patch lines:\n{out}"
    );
    assert!(
        !out.contains("Cart::add"),
        "an entry whose code did not change is listed:\n{out}"
    );
}

#[test]
fn code_and_entry_changed_in_one_commit_is_in_sync_and_not_listed() {
    let dir = committed_project();
    write(
        dir.path(),
        "src/app.py",
        &APP.replace("total += item", "total += item * 3"),
    );
    write(
        dir.path(),
        "docs/glossary/src.md",
        &GLOSSARY.replace("generators.", "generators; each item counts triple."),
    );
    commit_as(
        dir.path(),
        "2024-06-01T00:00:00Z",
        Some("triple, documented"),
    );
    reindex_code(dir.path());
    index_documents(dir.path());

    let out = stale_docs(dir.path(), json!({}));

    assert!(out.contains("0 of 2 entries possibly stale"), "{out}");
    assert!(!out.contains("compute_total"), "{out}");
}

#[test]
fn an_uncommitted_edit_counts_and_its_lines_are_shown() {
    let dir = committed_project();
    write(
        dir.path(),
        "src/app.py",
        &APP.replace("return total", "return total + 1"),
    );
    reindex_code(dir.path());

    let out = stale_docs(dir.path(), json!({}));

    assert!(out.contains("1 of 2 entries possibly stale"), "{out}");
    assert!(
        out.contains("uncommitted") && out.contains("+    return total + 1"),
        "the working-tree change is not shown:\n{out}"
    );
}

#[test]
fn a_symbol_or_a_path_narrows_the_list() {
    let dir = committed_project();
    change_total(
        dir.path(),
        "total += item\n",
        "total += item * 2\n",
        "2024-06-01T00:00:00Z",
        "double every item",
    );

    let by_symbol = stale_docs(dir.path(), json!({"symbol": "src/app.py::compute_total"}));
    assert!(by_symbol.contains("double every item"), "{by_symbol}");

    let unchanged = stale_docs(dir.path(), json!({"symbol": "src/app.py::Cart::add"}));
    assert!(
        unchanged.contains("none is possibly stale") && !unchanged.contains("double every item"),
        "{unchanged}"
    );

    let under = stale_docs(dir.path(), json!({"path_prefix": "src"}));
    assert!(under.contains("compute_total"), "{under}");
    let elsewhere = stale_docs(dir.path(), json!({"path_prefix": "lib"}));
    assert!(
        !elsewhere.contains("compute_total") && elsewhere.contains("0 of 0 entries"),
        "{elsewhere}"
    );
}

#[test]
fn the_history_shown_per_entry_is_capped_and_says_so() {
    let dir = committed_project();
    for (n, date) in [
        "2024-02-01",
        "2024-03-01",
        "2024-04-01",
        "2024-05-01",
        "2024-06-01",
    ]
    .iter()
    .enumerate()
    {
        let now = std::fs::read_to_string(dir.path().join("src/app.py")).unwrap();
        let before = if n == 0 {
            "total = 0".to_string()
        } else {
            format!("total = {n}")
        };
        change_total(
            dir.path(),
            &before,
            &format!("total = {}", n + 1),
            &format!("{date}T00:00:00Z"),
            &format!("change number {}", n + 1),
        );
        assert_ne!(
            now,
            std::fs::read_to_string(dir.path().join("src/app.py")).unwrap()
        );
    }

    let out = stale_docs(dir.path(), json!({}));

    assert!(
        out.contains("change number 5"),
        "newest commit missing:\n{out}"
    );
    assert!(
        !out.contains("change number 1\n"),
        "the cap did not drop the oldest commit:\n{out}"
    );
    assert!(out.contains("(truncated"), "no truncation marker:\n{out}");
}

#[test]
fn outside_a_git_checkout_nothing_is_judged_and_the_mode_says_why() {
    let dir = project(&[("docs/glossary/src.md", GLOSSARY)]);

    let out = stale_docs(dir.path(), json!({}));

    assert!(
        out.contains("could not be judged") && out.contains("git"),
        "{out}"
    );
    let live = infigraph_core::degraded::live::for_root(dir.path());
    assert!(live.iter().any(|n| n.key == "doc-sync-unknown"), "{live:?}");
}

#[test]
fn a_project_without_a_document_index_says_so() {
    let dir = code_project(&[], &[]);
    git(dir.path(), &["init", "-q"], FIRST);
    git(dir.path(), &["add", "src"], FIRST);
    git(dir.path(), &["commit", "-q", "-m", "code only"], FIRST);

    let out = stale_docs(dir.path(), json!({}));

    assert!(out.contains("no document index"), "{out}");
}

#[test]
fn doctor_and_the_tool_judge_the_same_entries() {
    let dir = committed_project();
    change_total(
        dir.path(),
        "total += item\n",
        "total += item * 2\n",
        "2024-06-01T00:00:00Z",
        "double every item",
    );

    let report = infigraph_docs::link_check::check_project(dir.path(), true);
    let sync = report
        .iter()
        .find(|c| c.name.ends_with("doc/code sync"))
        .unwrap();
    let out = stale_docs(dir.path(), json!({}));

    assert!(
        sync.message.contains("1 of 2 entries possibly stale"),
        "{}",
        sync.message
    );
    assert!(out.contains("1 of 2 entries possibly stale"), "{out}");
    assert!(
        sync.message.contains("src/app.py::compute_total")
            && out.contains("src/app.py::compute_total"),
        "{}\n{out}",
        sync.message
    );
}
