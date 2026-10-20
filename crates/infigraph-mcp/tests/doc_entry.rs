//! `doc_entry`: the mechanical part of keeping a glossary entry in step with
//! its code. The agent supplies the prose; the tool finds the entry, edits
//! exactly its lines, and refreshes the index. Dry-run unless `apply` is set.
//!
//! A project here is a tiny Python tree with two glossary files, built with
//! the local backend (and, where review is involved, a git history).

use std::path::Path;

use serde_json::{json, Value};

use infigraph_mcp::tools::doc_entry::{tool_doc_entry, tool_doc_entry_in};
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

/// A `<file>::module` key is live while the graph has the file (the rule
/// `doctor` applies), so an entry can be created, rekeyed to, and kept for one.
#[test]
fn a_module_key_is_live_while_its_file_is_in_the_graph() {
    let dir = project();
    let notes = "lib/util.py::module";

    ok(
        dir.path(),
        json!({"op": "create", "target": notes, "text": "Helpers.", "apply": true}),
    );
    assert!(file(dir.path(), "docs/glossary/lib.md").contains("### `lib/util.py::module`"));

    let gone = refused(
        dir.path(),
        json!({"op": "create", "target": "lib/gone.py::module", "text": "Nothing."}),
    );
    assert!(
        gone.contains("not in the graph") && gone.contains("lib/gone.py"),
        "{gone}"
    );

    let rekey = ok(
        dir.path(),
        json!({"op": "rekey", "target": "lib/util.py::other", "new_target": "lib/more.py::module"}),
    );
    assert!(rekey.contains("dry run"), "{rekey}");
    let nowhere = refused(
        dir.path(),
        json!({"op": "rekey", "target": "lib/util.py::other", "new_target": "lib/gone.py::module"}),
    );
    assert!(nowhere.contains("not in the graph"), "{nowhere}");

    let delete = refused(dir.path(), json!({"op": "delete", "target": notes}));
    assert!(delete.contains("still in the graph"), "{delete}");

    // A file's notes have no symbol lines, so there is nothing to review them against.
    let review = ok(dir.path(), json!({"op": "ack", "target": notes}));
    assert!(review.contains("no symbol lines"), "{review}");
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

// ---- an index that cannot be refreshed ---------------------------------------

mod refresh_fails {
    use super::*;
    use infigraph_docs::doc_entry::{run_with, Op, Pick};

    const WHY: &str = "the graph's growth breaker is tripped";

    /// Applies `op` with a document-index refresh that fails.
    fn apply(dir: &Path, op: &Op) -> String {
        run_with(dir, op, true, &|_| anyhow::bail!(WHY))
            .unwrap_or_else(|e| panic!("{op:?} failed instead of reporting: {e:#}"))
    }

    fn edit(old: &str, new: &str) -> Op {
        Op::Edit {
            target: TOTAL.to_string(),
            old: old.to_string(),
            new: new.to_string(),
            replace_all: false,
            pick: Pick::default(),
        }
    }

    fn says_not_refreshed(out: &str) {
        assert!(
            out.contains(&format!("document index NOT refreshed: {WHY}")),
            "{out}"
        );
    }

    #[test]
    fn an_edit_is_written_and_says_the_index_is_behind_and_is_then_already_done() {
        let dir = project();
        let op = edit("not with sum()", "rather than sum()");

        let first = apply(dir.path(), &op);

        says_not_refreshed(&first);
        assert!(file(dir.path(), "docs/glossary/src.md").contains("rather than sum()"));
        assert!(apply(dir.path(), &op).contains("already done"));
    }

    #[test]
    fn a_replace_is_written_and_then_already_done() {
        let dir = project();
        let op = Op::Replace {
            target: TOTAL.to_string(),
            text: "Sums with a loop.".to_string(),
            section: None,
            pick: Pick::default(),
        };

        says_not_refreshed(&apply(dir.path(), &op));

        assert!(file(dir.path(), "docs/glossary/src.md").contains("Sums with a loop."));
        assert!(apply(dir.path(), &op).contains("already done"));
    }

    #[test]
    fn a_rekey_is_written_and_then_already_done_though_the_index_still_has_the_old_key() {
        let dir = project();
        let op = Op::Rekey {
            target: "lib/util.py::other".to_string(),
            new_target: "lib/util.py::third".to_string(),
            force: false,
            ack: false,
            pick: Pick::default(),
        };

        says_not_refreshed(&apply(dir.path(), &op));

        let now = file(dir.path(), "docs/glossary/lib.md");
        assert!(now.contains("### `lib/util.py::third`\n\nOthers."), "{now}");
        let again = apply(dir.path(), &op);
        assert!(again.contains("already done"), "{again}");
        assert_eq!(file(dir.path(), "docs/glossary/lib.md"), now);
    }

    #[test]
    fn a_delete_is_written_and_then_already_done() {
        let dir = project();
        let op = Op::Delete {
            target: TOTAL.to_string(),
            force: true,
            pick: Pick::default(),
        };

        says_not_refreshed(&apply(dir.path(), &op));

        let now = file(dir.path(), "docs/glossary/src.md");
        assert!(!now.contains("compute_total"), "{now}");
        let again = apply(dir.path(), &op);
        assert!(again.contains("already done"), "{again}");
        assert_eq!(file(dir.path(), "docs/glossary/src.md"), now);
    }

    #[test]
    fn a_create_is_written_once_though_the_index_never_heard_of_it() {
        let dir = project();
        let op = Op::Create {
            target: "lib/util.py::third".to_string(),
            text: "Third.".to_string(),
            doc_file: None,
        };

        says_not_refreshed(&apply(dir.path(), &op));

        let once = file(dir.path(), "docs/glossary/lib.md");
        let again = apply(dir.path(), &op);
        assert!(again.contains("already done"), "{again}");
        assert_eq!(file(dir.path(), "docs/glossary/lib.md"), once);
        assert_eq!(
            once.matches("### `lib/util.py::third`").count(),
            1,
            "{once}"
        );
    }

    #[test]
    fn a_refresh_that_works_says_so() {
        let dir = project();
        let out = ok(
            dir.path(),
            json!({"op": "edit", "target": TOTAL, "old": "in a loop", "new": "by looping", "apply": true}),
        );
        assert!(out.contains("document index refreshed"), "{out}");
        assert!(!out.contains("NOT refreshed"), "{out}");
    }
}

// ---- ranges come from the file as it lies, not from the index ---------------

mod live_ranges {
    use super::*;

    /// `docs/glossary/src.md` with `lines` extra lines above its first entry,
    /// written by hand: the index still has the old line numbers.
    fn push_entries_down(dir: &Path, lines: usize) {
        let text = file(dir, "docs/glossary/src.md");
        let pad = "A new paragraph above every entry.\n\n".repeat(lines);
        write(
            dir,
            "docs/glossary/src.md",
            &text.replacen("# Glossary\n\n", &format!("# Glossary\n\n{pad}"), 1),
        );
    }

    fn guard(dir: &Path, old: &str) -> infigraph_docs::doc_guard::Decision {
        infigraph_docs::doc_guard::decide(&json!({
            "tool_name": "Edit", "cwd": dir,
            "tool_input": {
                "file_path": dir.join("docs/glossary/src.md"),
                "old_string": old, "new_string": "x",
            },
        }))
    }

    #[test]
    fn the_guard_follows_the_lines_the_file_has_now() {
        let dir = project();
        push_entries_down(dir.path(), 8);

        // The entry moved down 16 lines; the index still says it is on line 3.
        match guard(dir.path(), "Sums the items in a loop") {
            infigraph_docs::doc_guard::Decision::Deny(why) => {
                assert!(why.contains(TOTAL) && why.contains("src.md:19-"), "{why}")
            }
            other => panic!("an edit inside the moved entry was allowed: {other:?}"),
        }
        // The new text sits where the entry used to be in the index: outside.
        assert_eq!(
            guard(dir.path(), "A new paragraph above every entry."),
            infigraph_docs::doc_guard::Decision::Allow
        );
    }

    #[test]
    fn doc_entry_edits_the_lines_the_file_has_now() {
        let dir = project();
        push_entries_down(dir.path(), 8);

        ok(
            dir.path(),
            json!({
                "op": "edit", "target": TOTAL, "old": "in a loop", "new": "by looping",
                "apply": true,
            }),
        );

        let now = file(dir.path(), "docs/glossary/src.md");
        assert!(
            now.contains("Sums the items by looping, not with sum()"),
            "{now}"
        );
        assert_eq!(now.matches("A new paragraph above every entry.").count(), 8);
    }
}

// ---- one key, several entries --------------------------------------------------

mod duplicates {
    use super::*;

    const DUP: &str = "docs/glossary/dup.md";
    const THIRD: &str = "lib/util.py::third";
    const FIRST_COPY: &str = "# Dup\n\n### `lib/util.py::third`\n\nFirst third.\n\n\
        ### `lib/util.py::other`\n\nOther, here too.\n\n\
        ### `lib/util.py::third`\n\nSecond third.\n";

    /// The project plus `dup.md`, where `third` has two entries and `other`
    /// (which `lib.md` also has) one.
    fn project_with_duplicates() -> tempfile::TempDir {
        let dir = project();
        write(dir.path(), DUP, FIRST_COPY);
        index_documents(dir.path());
        dir
    }

    #[test]
    fn deleting_the_second_of_two_leaves_the_first_and_the_rest_byte_for_byte() {
        let dir = project_with_duplicates();

        let out = ok(
            dir.path(),
            json!({"op": "delete", "target": THIRD, "nth": 2, "force": true, "apply": true}),
        );

        assert!(out.contains("applied"), "{out}");
        assert_eq!(
            file(dir.path(), DUP),
            "# Dup\n\n### `lib/util.py::third`\n\nFirst third.\n\n\
             ### `lib/util.py::other`\n\nOther, here too.\n"
        );
        assert_eq!(file(dir.path(), "docs/glossary/lib.md"), LIB_DOC);
        assert_eq!(file(dir.path(), "docs/glossary/src.md"), SRC_DOC);
    }

    #[test]
    fn a_repeated_delete_of_a_second_entry_that_is_gone_is_already_done() {
        let dir = project_with_duplicates();
        let args = json!({"op": "delete", "target": THIRD, "nth": 2, "force": true, "apply": true});
        ok(dir.path(), args.clone());
        let after_one = file(dir.path(), DUP);

        let again = ok(dir.path(), args);

        assert!(again.contains("already done"), "{again}");
        assert!(again.contains("no 2nd entry (1 found)"), "{again}");
        assert_eq!(file(dir.path(), DUP), after_one);
    }

    #[test]
    fn without_a_selector_every_entry_is_named_and_so_is_the_way_to_choose() {
        let dir = project_with_duplicates();

        for op in ["delete", "edit", "replace", "rekey"] {
            let mut args = json!({
                "op": op, "target": THIRD, "old": "third", "new": "x", "text": "y",
                "new_target": "lib/more.py::fresh", "force": true,
            });
            args["apply"] = json!(true);
            let why = refused(dir.path(), args);
            assert!(
                why.contains("docs/glossary/dup.md:3") && why.contains("docs/glossary/dup.md:11"),
                "{op}: {why}"
            );
            assert!(why.contains("nth") && why.contains("line"), "{op}: {why}");
        }
        assert_eq!(
            file(dir.path(), DUP),
            FIRST_COPY,
            "a refusal wrote something"
        );
    }

    #[test]
    fn an_edit_by_nth_or_by_line_changes_only_that_entry() {
        let dir = project_with_duplicates();

        ok(
            dir.path(),
            json!({"op": "edit", "target": THIRD, "old": "third", "new": "3rd", "nth": 1, "apply": true}),
        );
        let first = file(dir.path(), DUP);
        assert!(
            first.contains("First 3rd.") && first.contains("Second third."),
            "{first}"
        );

        ok(
            dir.path(),
            json!({"op": "edit", "target": THIRD, "old": "third", "new": "3rd", "line": 11, "apply": true}),
        );
        let both = file(dir.path(), DUP);
        assert!(
            both.contains("First 3rd.") && both.contains("Second 3rd."),
            "{both}"
        );
        assert!(both.contains("Other, here too."), "{both}");
    }

    #[test]
    fn a_replace_and_a_rekey_pick_their_entry_too() {
        let dir = project_with_duplicates();

        ok(
            dir.path(),
            json!({"op": "replace", "target": THIRD, "text": "Rewritten.", "nth": 2, "apply": true}),
        );
        assert!(file(dir.path(), DUP).ends_with("### `lib/util.py::third`\n\nRewritten.\n"));

        ok(
            dir.path(),
            json!({
                "op": "rekey", "target": THIRD, "new_target": "lib/more.py::fresh",
                "nth": 2, "apply": true,
            }),
        );
        let now = file(dir.path(), DUP);
        assert!(
            now.contains("### `lib/more.py::fresh`\n\nRewritten."),
            "{now}"
        );
        assert!(
            now.contains("### `lib/util.py::third`\n\nFirst third."),
            "{now}"
        );
    }

    #[test]
    fn a_selector_beyond_the_entries_there_are_is_an_error_naming_the_count() {
        let dir = project_with_duplicates();

        let nth = refused(
            dir.path(),
            json!({"op": "edit", "target": THIRD, "old": "third", "new": "x", "nth": 3}),
        );
        assert!(nth.contains("2 entries") && nth.contains("3"), "{nth}");
        let line = refused(
            dir.path(),
            json!({"op": "edit", "target": THIRD, "old": "third", "new": "x", "line": 7}),
        );
        assert!(
            line.contains("line 7") && line.contains("dup.md:3"),
            "{line}"
        );
    }

    #[test]
    fn the_same_key_in_two_documents_needs_doc_file() {
        let dir = project_with_duplicates();
        let other = "lib/util.py::other";

        let why = refused(
            dir.path(),
            json!({"op": "edit", "target": other, "old": "Others.", "new": "x"}),
        );
        assert!(
            why.contains("docs/glossary/dup.md:7") && why.contains("docs/glossary/lib.md:7"),
            "{why}"
        );
        assert!(why.contains("doc_file"), "{why}");

        ok(
            dir.path(),
            json!({
                "op": "edit", "target": other, "old": "here too", "new": "also here",
                "doc_file": DUP, "apply": true,
            }),
        );
        assert!(file(dir.path(), DUP).contains("Other, also here."));
        assert_eq!(file(dir.path(), "docs/glossary/lib.md"), LIB_DOC);
    }
}

// ---- a linked worktree is its own project ----------------------------------------

mod worktrees {
    use super::*;

    /// A committed project and a linked worktree of it inside its directory
    /// (`scratchpad/wt-x`, as the repo's convention puts them).
    fn project_and_worktree() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = committed();
        git(
            dir.path(),
            &["worktree", "add", "-q", "-b", "topic", "scratchpad/wt-x"],
            FIRST,
        );
        let wt = dir.path().join("scratchpad/wt-x");
        assert!(wt.join("docs/glossary/src.md").is_file());
        (dir, wt)
    }

    const EDIT: fn(bool) -> Value = |apply| {
        json!({
            "op": "edit", "target": TOTAL, "old": "in a loop", "new": "by looping",
            "apply": apply,
        })
    };

    #[test]
    fn a_worktree_with_its_own_index_is_edited_and_refreshed_on_its_own() {
        let (main, wt) = project_and_worktree();
        reindex_code(&wt);
        index_documents(&wt);

        let out = ok(&wt, EDIT(true));

        assert!(out.contains("document index refreshed"), "{out}");
        assert!(file(&wt, "docs/glossary/src.md").contains("by looping"));
        assert_eq!(file(main.path(), "docs/glossary/src.md"), SRC_DOC);
        assert!(
            context(&wt, TOTAL).contains("by looping"),
            "its index was not refreshed"
        );
        assert!(
            !context(main.path(), TOTAL).contains("by looping"),
            "the main checkout's index was refreshed"
        );
    }

    #[test]
    fn a_worktree_with_no_index_of_its_own_is_refused_rather_than_resolved_to_the_main_checkout() {
        let (main, wt) = project_and_worktree();

        let why = refused(&wt, EDIT(true));

        assert!(
            why.contains("no index of its own") && why.contains("worktree init"),
            "{why}"
        );
        assert_eq!(file(main.path(), "docs/glossary/src.md"), SRC_DOC);
        assert_eq!(file(&wt, "docs/glossary/src.md"), SRC_DOC);
    }

    /// A call with no `path`, from a server that was started in `startup`.
    fn from_server_in(startup: &Path, mut args: Value) -> anyhow::Result<String> {
        args.as_object_mut().unwrap().remove("path");
        tool_doc_entry_in(&args, startup)
    }

    #[test]
    fn every_report_starts_with_the_project_it_resolved() {
        let (_main, wt) = project_and_worktree();
        reindex_code(&wt);
        index_documents(&wt);

        let dry = ok(&wt, EDIT(false));
        let applied = ok(&wt, EDIT(true));
        let again = ok(&wt, EDIT(true));

        let root = wt.canonicalize().unwrap();
        for report in [&dry, &applied, &again] {
            let first = report.lines().next().unwrap();
            assert!(
                first.contains(&root.display().to_string()),
                "first line does not name {}:\n{report}",
                root.display()
            );
        }
        assert!(
            dry.lines().next().unwrap().ends_with("scratchpad/wt-x"),
            "{dry}"
        );
    }

    #[test]
    fn an_apply_with_no_path_is_refused_when_the_project_has_worktrees() {
        let (main, _wt) = project_and_worktree();

        let why = match from_server_in(main.path(), EDIT(true)) {
            Ok(out) => panic!("not refused:\n{out}"),
            Err(e) => format!("{e:#}"),
        };

        let root = main.path().canonicalize().unwrap();
        assert!(
            why.contains(&root.display().to_string()) && why.contains("`path`"),
            "{why}"
        );
        assert_eq!(file(main.path(), "docs/glossary/src.md"), SRC_DOC);
    }

    #[test]
    fn a_dry_run_with_no_path_is_allowed_and_names_the_root() {
        let (main, _wt) = project_and_worktree();

        let out = from_server_in(main.path(), EDIT(false)).unwrap();

        let root = main.path().canonicalize().unwrap();
        assert!(
            out.lines()
                .next()
                .unwrap()
                .contains(&root.display().to_string()),
            "{out}"
        );
        assert!(out.contains("dry run"), "{out}");
    }

    #[test]
    fn an_apply_with_no_path_still_works_in_a_project_without_worktrees() {
        let dir = project();

        let out = from_server_in(dir.path(), EDIT(true)).unwrap();

        assert!(out.contains("applied"), "{out}");
        assert!(file(dir.path(), "docs/glossary/src.md").contains("by looping"));
    }
}

// ---- the bytes outside the entry are the file's own ----------------------

/// A glossary of three entries; `ending` is what follows the last one, so the
/// file can end in a blank line, several, or no newline at all.
fn glossary_ending_in(ending: &str) -> String {
    let mut text = "# Lib\n\n\
### `lib/util.py::helper`\n\nHelps.\n\n\
### `lib/util.py::other`\n\nOthers.\n\n\
### `other/x.py::lonely`\n\nLonely."
        .to_string();
    text.push_str(ending);
    text
}

/// `(label, ending)`: a normal file, a blank line at the end, two, and no
/// final newline.
const ENDINGS: [(&str, &str); 4] = [
    ("plain", "\n"),
    ("one blank line at the end", "\n\n"),
    ("two blank lines at the end", "\n\n\n"),
    ("no final newline", ""),
];

fn project_with_glossary(text: &str) -> tempfile::TempDir {
    let dir = code_project(
        &[("lib/util.py", UTIL), ("other/x.py", ELSEWHERE)],
        &[("docs/glossary/lib.md", text)],
    );
    index_documents(dir.path());
    dir
}

/// How `after` differs from `before` as bytes: the common prefix and suffix
/// (never overlapping) and what each side has between them.
fn changed_region<'a>(before: &'a str, after: &'a str) -> (&'a str, &'a str) {
    let (b, a) = (before.as_bytes(), after.as_bytes());
    let prefix = b.iter().zip(a).take_while(|(x, y)| x == y).count();
    let suffix = b[prefix..]
        .iter()
        .rev()
        .zip(a[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    (
        &before[prefix..before.len() - suffix],
        &after[prefix..after.len() - suffix],
    )
}

#[test]
fn a_create_adds_the_entry_and_leaves_every_other_byte_of_the_file_alone() {
    for (label, ending) in ENDINGS {
        let before = glossary_ending_in(ending);
        let dir = project_with_glossary(&before);

        ok(
            dir.path(),
            json!({
                "op": "create", "target": "lib/util.py::third",
                "text": "Third.", "apply": true,
            }),
        );

        let after = file(dir.path(), "docs/glossary/lib.md");
        let (removed, added) = changed_region(&before, &after);
        assert_eq!(removed, "", "{label}: a create removed text:\n{after:?}");
        assert!(!added.is_empty(), "{label}: nothing was added");
        assert!(
            after.contains("### `lib/util.py::third`\n\nThird."),
            "{label}: {after:?}"
        );
        assert!(
            after.ends_with(&before[before.rfind("Lonely.").unwrap()..]),
            "{label}: the tail of the file changed: {after:?}"
        );
    }
}

#[test]
fn a_rekey_changes_only_the_heading_line() {
    for (label, ending) in ENDINGS {
        let before = glossary_ending_in(ending);
        let dir = project_with_glossary(&before);

        ok(
            dir.path(),
            json!({
                "op": "rekey", "target": "lib/util.py::other",
                "new_target": "lib/util.py::third", "apply": true,
            }),
        );

        let after = file(dir.path(), "docs/glossary/lib.md");
        let (removed, added) = changed_region(&before, &after);
        assert!(
            removed.contains("other") && !removed.contains('\n'),
            "{label}: {removed:?}"
        );
        assert!(
            added.contains("third") && !added.contains('\n'),
            "{label}: {added:?}"
        );
        assert!(
            after.ends_with(&before[before.rfind("Lonely.").unwrap()..]),
            "{label}: {after:?}"
        );
    }
}

#[test]
fn a_delete_removes_the_entry_and_leaves_every_other_byte_of_the_file_alone() {
    for (label, ending) in ENDINGS {
        let before = glossary_ending_in(ending);
        let dir = project_with_glossary(&before);

        ok(
            dir.path(),
            json!({
                "op": "delete", "target": "lib/util.py::other",
                "force": true, "apply": true,
            }),
        );

        let after = file(dir.path(), "docs/glossary/lib.md");
        let (removed, added) = changed_region(&before, &after);
        assert_eq!(added, "", "{label}: a delete added text: {after:?}");
        assert!(removed.contains("Others."), "{label}: {removed:?}");
        assert!(
            after.ends_with(&before[before.rfind("Lonely.").unwrap()..]),
            "{label}: {after:?}"
        );
    }
}

/// The create at the very end of the file: the blank lines the file ended
/// with are still the last lines after it.
#[test]
fn a_create_at_the_end_keeps_the_files_trailing_blank_lines_last() {
    let before = format!("{}\n\n", "# Lib\n\n### `lib/util.py::other`\n\nOthers.");
    let dir = project_with_glossary(&before);

    ok(
        dir.path(),
        json!({
            "op": "create", "target": "lib/util.py::third",
            "text": "Third.", "apply": true,
        }),
    );

    let after = file(dir.path(), "docs/glossary/lib.md");
    assert!(after.ends_with("Third.\n\n"), "{after:?}");
}
