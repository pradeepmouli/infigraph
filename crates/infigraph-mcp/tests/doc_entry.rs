//! `doc_entry`: the mechanical part of keeping a glossary entry in step with
//! its code. The agent supplies the prose; the tool finds the entry, edits
//! exactly its lines, and refreshes the index. Dry-run unless `apply` is set.
//!
//! A project here is a tiny Python tree with two glossary files, built with
//! the local backend (and, where review is involved, a git history).

use std::path::Path;

use serde_json::{json, Value};

use infigraph_mcp::tools::doc_entry::tool_doc_entry;
use infigraph_mcp::tools::graph::tool_get_doc_context;

mod support;

use support::docs::{code_project, git, index_documents, reindex_code, write};

const SRC_DOC: &str = "# Glossary\n\n\
### `src/app.py::compute_total`\n\nSums the items in a loop, not with sum(): items may be generators.\n\n\
#### body\n\n```text\naccumulate in place\n```\n\n\
### `src/app.py::Cart::add`\n\nAppends to the cart; never merges duplicates.\n";

const LIB_DOC: &str = "# Lib\n\n\
### `lib/util.py::helper`\n\nHelps. Calls `lib/util.py::other`.\n\n\
### `lib/util.py::other`\n\nOthers.\n";

const UTIL: &str =
    "def helper():\n    return 1\n\n\ndef other():\n    return 2\n\n\ndef third():\n    return 4\n";
const MORE: &str = "def fresh():\n    return 3\n";
const ELSEWHERE: &str = "def lonely():\n    return 5\n";

const FIRST: &str = "2024-01-01T00:00:00Z";

fn project() -> tempfile::TempDir {
    let dir = code_project(
        &[
            ("lib/util.py", UTIL),
            ("lib/more.py", MORE),
            ("other/x.py", ELSEWHERE),
        ],
        &[
            ("docs/glossary/src.md", SRC_DOC),
            ("docs/glossary/lib.md", LIB_DOC),
        ],
    );
    index_documents(dir.path());
    dir
}

fn committed() -> tempfile::TempDir {
    let dir = project();
    git(dir.path(), &["init", "-q"], FIRST);
    git(dir.path(), &["add", "src", "lib", "other", "docs"], FIRST);
    git(dir.path(), &["commit", "-q", "-m", "initial import"], FIRST);
    dir
}

/// Commits every source and document (`commit_as` takes `src` and `docs` only).
fn commit_all(dir: &Path, date: &str, message: &str) {
    git(dir, &["add", "src", "lib", "other", "docs"], date);
    git(dir, &["commit", "-q", "-m", message], date);
}

fn run(dir: &Path, args: Value) -> anyhow::Result<String> {
    let mut args = args;
    args["path"] = json!(dir.to_string_lossy());
    tool_doc_entry(&args)
}

fn ok(dir: &Path, args: Value) -> String {
    run(dir, args.clone()).unwrap_or_else(|e| panic!("{args}: {e:#}"))
}

fn refused(dir: &Path, args: Value) -> String {
    match run(dir, args.clone()) {
        Ok(out) => panic!("{args} was not refused:\n{out}"),
        Err(e) => format!("{e:#}"),
    }
}

fn file(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

fn context(dir: &Path, symbol: &str) -> String {
    tool_get_doc_context(&json!({
        "path": dir.to_string_lossy(),
        "symbol_id": symbol,
    }))
    .unwrap()
}

const TOTAL: &str = "src/app.py::compute_total";

#[test]
fn a_replace_shows_the_change_and_writes_nothing_until_applied() {
    let dir = project();
    let args = json!({
        "op": "replace", "target": TOTAL,
        "text": "Sums with a loop; items may be generators, so sum() is out.",
    });

    let dry = ok(dir.path(), args.clone());

    assert!(dry.contains("dry run"), "{dry}");
    assert!(
        dry.contains("-Sums the items in a loop") && dry.contains("+Sums with a loop"),
        "{dry}"
    );
    assert_eq!(file(dir.path(), "docs/glossary/src.md"), SRC_DOC);

    let mut apply = args;
    apply["apply"] = json!(true);
    let done = ok(dir.path(), apply);

    assert!(done.contains("applied"), "{done}");
    let now = file(dir.path(), "docs/glossary/src.md");
    assert!(now.contains("Sums with a loop; items may be generators, so sum() is out."));
    assert!(!now.contains("Sums the items in a loop"));
    assert!(
        now.contains("### `src/app.py::compute_total`") && now.contains("#### body"),
        "the key heading or a sub-heading was lost:\n{now}"
    );
    assert!(now.contains("accumulate in place"), "{now}");
    assert!(now.contains("never merges duplicates"), "{now}");
    assert!(
        context(dir.path(), TOTAL).contains("so sum() is out"),
        "the index was not refreshed"
    );
}

#[test]
fn a_replace_of_a_section_changes_only_that_block() {
    let dir = project();

    ok(
        dir.path(),
        json!({
            "op": "replace", "target": TOTAL, "section": "body",
            "text": "```text\nsum in place\n```", "apply": true,
        }),
    );

    let now = file(dir.path(), "docs/glossary/src.md");
    assert!(now.contains("sum in place") && !now.contains("accumulate in place"));
    assert!(now.contains("Sums the items in a loop"), "{now}");
    assert!(now.contains("#### body"));
}

#[test]
fn replacing_with_the_text_already_there_is_already_done() {
    let dir = project();

    let out = ok(
        dir.path(),
        json!({
            "op": "replace", "target": TOTAL, "apply": true,
            "text": "Sums the items in a loop, not with sum(): items may be generators.",
        }),
    );

    assert!(out.contains("already"), "{out}");
    assert_eq!(file(dir.path(), "docs/glossary/src.md"), SRC_DOC);
}

#[test]
fn an_edit_changes_one_phrase_and_is_idempotent() {
    let dir = project();
    let args = json!({
        "op": "edit", "target": TOTAL,
        "old": "not with sum()", "new": "rather than sum()",
    });

    let dry = ok(dir.path(), args.clone());
    assert!(
        dry.contains("dry run") && dry.contains("+Sums the items in a loop, rather than sum()")
    );
    assert_eq!(file(dir.path(), "docs/glossary/src.md"), SRC_DOC);

    let mut apply = args;
    apply["apply"] = json!(true);
    ok(dir.path(), apply.clone());
    let now = file(dir.path(), "docs/glossary/src.md");
    assert!(now.contains("rather than sum()") && !now.contains("not with sum()"));
    assert_eq!(
        now.len() as i64 - SRC_DOC.len() as i64,
        "rather than sum()".len() as i64 - "not with sum()".len() as i64,
        "more than the phrase changed"
    );

    let again = ok(dir.path(), apply);
    assert!(again.contains("already"), "{again}");
}

#[test]
fn an_edit_must_match_exactly_once_inside_its_own_entry() {
    let dir = project();

    let many = refused(
        dir.path(),
        json!({"op": "edit", "target": TOTAL, "old": "items", "new": "elements"}),
    );
    assert!(many.contains("2 matches"), "{many}");

    let none = refused(
        dir.path(),
        json!({"op": "edit", "target": TOTAL, "old": "no such words", "new": "plugh"}),
    );
    assert!(none.contains("not found"), "{none}");

    let outside = refused(
        dir.path(),
        json!({"op": "edit", "target": TOTAL, "old": "never merges duplicates", "new": "plugh"}),
    );
    assert!(
        outside.contains("outside") && outside.contains("src/app.py::Cart::add"),
        "{outside}"
    );
    assert_eq!(file(dir.path(), "docs/glossary/src.md"), SRC_DOC);
}

#[test]
fn replace_all_stays_inside_the_entry() {
    let dir = project();

    ok(
        dir.path(),
        json!({
            "op": "edit", "target": TOTAL, "old": "items", "new": "elements",
            "replace_all": true, "apply": true,
        }),
    );

    let now = file(dir.path(), "docs/glossary/src.md");
    assert!(now.contains("Sums the elements in a loop") && now.contains("elements may be"));
    assert!(now.contains("Appends to the cart"), "{now}");
}

#[test]
fn text_that_would_add_or_remove_an_entry_is_refused() {
    let dir = project();

    let added = refused(
        dir.path(),
        json!({
            "op": "replace", "target": TOTAL,
            "text": "Sums.\n\n### `src/app.py::module`\n\nSmuggled in.",
        }),
    );
    assert!(added.contains("entries"), "{added}");

    let edited = refused(
        dir.path(),
        json!({
            "op": "edit", "target": "src/app.py::Cart::add",
            "old": "never merges duplicates.",
            "new": "x.\n\n### `src/app.py::module`\n\nSmuggled in.",
        }),
    );
    assert!(edited.contains("entries"), "{edited}");
    assert_eq!(file(dir.path(), "docs/glossary/src.md"), SRC_DOC);
}

#[test]
fn a_new_entry_goes_after_the_last_entry_of_its_own_file() {
    let dir = project();

    let out = ok(
        dir.path(),
        json!({
            "op": "create", "target": "lib/util.py::third",
            "text": "Third, after the others.", "apply": true,
        }),
    );

    assert!(out.contains("docs/glossary/lib.md"), "{out}");
    let now = file(dir.path(), "docs/glossary/lib.md");
    let (other, third) = (
        now.find("### `lib/util.py::other`").unwrap(),
        now.find("### `lib/util.py::third`").unwrap(),
    );
    assert!(third > other, "{now}");
    assert!(now.ends_with("Third, after the others.\n"), "{now}");
    assert!(context(dir.path(), "lib/util.py::third").contains("Third, after the others."));
}

#[test]
fn a_new_entry_for_a_file_with_none_uses_its_directorys_doc() {
    let dir = project();

    let out = ok(
        dir.path(),
        json!({"op": "create", "target": "lib/more.py::fresh", "text": "Fresh.", "apply": true}),
    );

    assert!(out.contains("docs/glossary/lib.md"), "{out}");
    assert!(file(dir.path(), "docs/glossary/lib.md").contains("### `lib/more.py::fresh`"));
}

#[test]
fn a_new_entry_is_refused_rather_than_guessed() {
    let dir = project();

    let nowhere = refused(
        dir.path(),
        json!({"op": "create", "target": "other/x.py::lonely", "text": "Alone."}),
    );
    assert!(nowhere.contains("doc_file"), "{nowhere}");

    let exists = refused(
        dir.path(),
        json!({"op": "create", "target": TOTAL, "text": "Again."}),
    );
    assert!(exists.contains("already has an entry"), "{exists}");

    let gone = refused(
        dir.path(),
        json!({"op": "create", "target": "lib/util.py::nothing", "text": "?"}),
    );
    assert!(gone.contains("not in the graph"), "{gone}");

    let invented = refused(
        dir.path(),
        json!({
            "op": "create", "target": "other/x.py::lonely", "text": "Alone.",
            "doc_file": "docs/glossary/new.md",
        }),
    );
    assert!(invented.contains("does not exist"), "{invented}");
}

#[test]
fn an_explicit_doc_file_decides_where_a_new_entry_goes() {
    let dir = project();

    ok(
        dir.path(),
        json!({
            "op": "create", "target": "other/x.py::lonely", "text": "Alone.",
            "doc_file": "docs/glossary/src.md", "apply": true,
        }),
    );

    let now = file(dir.path(), "docs/glossary/src.md");
    assert!(now.contains("### `other/x.py::lonely`\n\nAlone."), "{now}");
}

#[test]
fn a_rekey_moves_the_heading_and_keeps_the_body() {
    let dir = project();
    let args = json!({
        "op": "rekey", "target": "lib/util.py::other", "new_target": "lib/util.py::third",
    });

    let dry = ok(dir.path(), args.clone());
    assert!(dry.contains("dry run"), "{dry}");
    assert!(
        dry.contains("1 prose mention"),
        "the mention of the old name is not reported:\n{dry}"
    );

    let mut apply = args;
    apply["apply"] = json!(true);
    ok(dir.path(), apply.clone());

    let now = file(dir.path(), "docs/glossary/lib.md");
    assert!(now.contains("### `lib/util.py::third`\n\nOthers."), "{now}");
    assert!(
        now.contains("Calls `lib/util.py::other`."),
        "a mention in prose was edited: {now}"
    );
    assert!(context(dir.path(), "lib/util.py::third").contains("Others."));
    assert!(ok(dir.path(), apply).contains("already"));
}

#[test]
fn a_rekey_is_refused_when_it_would_collide_or_point_nowhere() {
    let dir = project();

    let taken = refused(
        dir.path(),
        json!({"op": "rekey", "target": "lib/util.py::other", "new_target": "lib/util.py::helper"}),
    );
    assert!(taken.contains("already has an entry"), "{taken}");

    let nowhere = refused(
        dir.path(),
        json!({"op": "rekey", "target": "lib/util.py::other", "new_target": "lib/util.py::nope"}),
    );
    assert!(nowhere.contains("not in the graph"), "{nowhere}");

    // Stated, it goes through.
    ok(
        dir.path(),
        json!({
            "op": "rekey", "target": "lib/util.py::other", "new_target": "lib/util.py::nope",
            "force": true, "apply": true,
        }),
    );
    assert!(file(dir.path(), "docs/glossary/lib.md").contains("### `lib/util.py::nope`"));
}

#[test]
fn a_delete_needs_the_symbol_gone_or_force_and_is_a_no_op_twice() {
    let dir = project();
    let target = "lib/util.py::other";

    let alive = refused(dir.path(), json!({"op": "delete", "target": target}));
    assert!(alive.contains("still in the graph"), "{alive}");

    write(
        dir.path(),
        "lib/util.py",
        "def helper():\n    return 1\n\n\ndef third():\n    return 4\n",
    );
    reindex_code(dir.path());
    let dry = ok(dir.path(), json!({"op": "delete", "target": target}));
    assert!(dry.contains("dry run") && dry.contains("-Others."), "{dry}");
    assert_eq!(file(dir.path(), "docs/glossary/lib.md"), LIB_DOC);

    ok(
        dir.path(),
        json!({"op": "delete", "target": target, "apply": true}),
    );
    let now = file(dir.path(), "docs/glossary/lib.md");
    assert!(!now.contains("Others.") && !now.contains(&format!("`{target}`\n")));
    assert!(now.contains("Helps."), "{now}");
    assert!(
        !now.ends_with("\n\n"),
        "a blank line was left behind:\n{now:?}"
    );

    let again = ok(
        dir.path(),
        json!({"op": "delete", "target": target, "apply": true}),
    );
    assert!(again.contains("already"), "{again}");
}

#[test]
fn a_force_delete_removes_an_entry_whose_symbol_still_exists() {
    let dir = project();

    ok(
        dir.path(),
        json!({"op": "delete", "target": TOTAL, "force": true, "apply": true}),
    );

    let now = file(dir.path(), "docs/glossary/src.md");
    assert!(!now.contains("compute_total") && !now.contains("accumulate in place"));
    assert!(now.contains("Cart::add"), "{now}");
}

#[test]
fn a_review_is_recorded_only_when_applied() {
    let dir = committed();
    write(
        dir.path(),
        "src/app.py",
        &std::fs::read_to_string(dir.path().join("src/app.py"))
            .unwrap()
            .replace("total += item\n", "total += item * 2\n"),
    );
    commit_all(dir.path(), "2024-06-01T00:00:00Z", "double");
    reindex_code(dir.path());
    let stale = || {
        infigraph_mcp::tools::doc_sync::tool_stale_docs(
            &json!({"path": dir.path().to_string_lossy()}),
        )
        .unwrap()
    };
    assert!(stale().contains("1 of 4 entries possibly stale"));

    let dry = ok(dir.path(), json!({"op": "ack", "targets": [TOTAL]}));
    assert!(dry.contains("dry run") && dry.contains(TOTAL), "{dry}");
    assert!(stale().contains("1 of 4 entries possibly stale"));

    let done = ok(
        dir.path(),
        json!({"op": "ack", "targets": [TOTAL], "apply": true}),
    );
    assert!(done.contains("recorded"), "{done}");
    assert!(stale().contains("0 of 4 entries possibly stale"));
}

#[test]
fn a_rekey_can_review_the_entry_in_the_same_call() {
    let dir = committed();
    write(
        dir.path(),
        "lib/util.py",
        &format!("{UTIL}\n\ndef fourth():\n    return 6\n"),
    );
    commit_all(dir.path(), "2024-06-01T00:00:00Z", "add fourth");
    reindex_code(dir.path());

    let out = ok(
        dir.path(),
        json!({
            "op": "rekey", "target": "lib/util.py::other", "new_target": "lib/util.py::fourth",
            "ack": true, "apply": true,
        }),
    );

    assert!(out.contains("reviewed"), "{out}");
    let stale = infigraph_mcp::tools::doc_sync::tool_stale_docs(
        &json!({"path": dir.path().to_string_lossy(), "symbol": "lib/util.py::fourth"}),
    )
    .unwrap();
    assert!(stale.contains("0 of 1 entries possibly stale"), "{stale}");
}

#[test]
fn an_unknown_op_or_a_missing_argument_says_what_is_needed() {
    let dir = project();

    assert!(refused(dir.path(), json!({"op": "frobnicate", "target": TOTAL})).contains("op"));
    assert!(refused(dir.path(), json!({"op": "replace", "target": TOTAL})).contains("text"));
    assert!(refused(
        dir.path(),
        json!({"op": "edit", "target": TOTAL, "old": "a"})
    )
    .contains("new"));
    assert!(refused(dir.path(), json!({"op": "rekey", "target": TOTAL})).contains("new_target"));
}

#[test]
fn the_guard_still_blocks_an_edit_to_an_entry_the_tool_just_rewrote() {
    let dir = project();

    // The tool writes the file itself (a write of the infigraph process, not a
    // tool call the hook sees), and is not stopped by the guard.
    ok(
        dir.path(),
        json!({
            "op": "edit", "target": TOTAL, "old": "not with sum()", "new": "rather than sum()",
            "apply": true,
        }),
    );
    assert!(file(dir.path(), "docs/glossary/src.md").contains("rather than sum()"));

    // An agent's own Edit of that entry is what the guard denies, and it names
    // the tool to use instead.
    let call = |old: &str| {
        infigraph_docs::doc_guard::decide(&json!({
            "tool_name": "Edit", "cwd": dir.path(),
            "tool_input": {
                "file_path": dir.path().join("docs/glossary/src.md"),
                "old_string": old, "new_string": "x",
            },
        }))
    };
    match call("rather than sum()") {
        infigraph_docs::doc_guard::Decision::Deny(why) => {
            assert!(why.contains(TOTAL) && why.contains("doc_entry"), "{why}")
        }
        other => panic!("the entry was editable by hand: {other:?}"),
    }
    assert_eq!(
        call("# Glossary"),
        infigraph_docs::doc_guard::Decision::Allow,
        "an edit outside every entry must pass"
    );
}
