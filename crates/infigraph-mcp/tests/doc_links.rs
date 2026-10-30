//! Documents linked to code: a glossary entry whose heading is a symbol id
//! (`### `path::name``) reaches `get_doc_context` for that symbol, so the
//! rationale an agent is about to need is in the call it already makes.
//!
//! A project here is a tiny Python tree, its graph, and its document index,
//! all built in a temp dir with the local backend.

use std::path::Path;

use serde_json::json;

use infigraph_mcp::tools::doctor::tool_doctor;
use infigraph_mcp::tools::graph::tool_get_doc_context;

mod support;

use support::docs::{
    code_project, commit, git, index_documents, project, reindex_code, write, APP,
};

const GLOSSARY: &str = "# Glossary\n\n\
### `src/app.py::compute_total`\n\nSums the items in a loop, not with sum(): items may be generators.\n\n\
#### body\n\n```text\naccumulate in place\n```\n\n\
### `src/app.py::module`\n\nThe cart helpers live here; nothing in this file may do I/O.\n";

fn context(dir: &Path, symbol: &str) -> String {
    tool_get_doc_context(&json!({
        "path": dir.to_string_lossy(),
        "symbol_id": symbol,
    }))
    .unwrap()
}

#[test]
fn a_glossary_entry_keyed_to_a_symbol_shows_up_in_its_doc_context() {
    let dir = project(&[("docs/glossary/src.md", GLOSSARY)]);
    let out = context(dir.path(), "src/app.py::compute_total");

    assert!(
        out.contains("Sums the items in a loop"),
        "the entry is missing:\n{out}"
    );
    assert!(
        out.contains("accumulate in place"),
        "its #### body sub-entry is missing:\n{out}"
    );
}

#[test]
fn any_symbol_also_shows_its_files_notes_in_a_separate_block() {
    let dir = project(&[("docs/glossary/src.md", GLOSSARY)]);
    let out = context(dir.path(), "src/app.py::Cart");

    assert!(
        out.contains("File-level notes (docs/glossary/src.md:"),
        "no file-level block:\n{out}"
    );
    assert!(out.contains("nothing in this file may do I/O"), "{out}");
    assert!(
        !out.contains("Glossary ("),
        "Cart has no entry of its own:\n{out}"
    );
}

#[test]
fn key_headings_of_levels_2_and_4_link_like_level_3() {
    let dir = project(&[(
        "docs/levels.md",
        "# Levels\n\n## `src/app.py::Cart`\n\nA cart is a bag.\n\n#### `src/app.py::compute_total`\n\nTotals are plain sums.\n",
    )]);

    let cart = context(dir.path(), "src/app.py::Cart");
    assert!(cart.contains("A cart is a bag."), "{cart}");
    assert!(
        !cart.contains("Totals are plain sums."),
        "a key heading ends the entry above it:\n{cart}"
    );
    let total = context(dir.path(), "src/app.py::compute_total");
    assert!(total.contains("Totals are plain sums."), "{total}");
}

#[test]
fn a_mention_links_only_when_it_names_exactly_one_symbol() {
    let shop = "class Cart:\n    def add(self, item):\n        self.items.append(item)\n";
    let dir = code_project(
        &[("src/shop.py", shop)],
        &[(
            "docs/guide.md",
            "# Guide\n\nTotals come from `app.py::compute_total`, never from `compute_total` alone.\n\nAdding is `Cart.add`.\n",
        )],
    );
    index_documents(dir.path());

    let total = context(dir.path(), "src/app.py::compute_total");
    assert!(
        total.contains("Mentioned in (heuristic"),
        "no mentions block:\n{total}"
    );
    assert!(total.contains("docs/guide.md:3"), "{total}");
    assert_eq!(
        total.matches("docs/guide.md:").count(),
        1,
        "the bare name must not add a second link:\n{total}"
    );

    for cart in ["src/app.py::Cart::add", "src/shop.py::Cart::add"] {
        let out = context(dir.path(), cart);
        assert!(
            !out.contains("Mentioned in"),
            "`Cart.add` names two symbols, so it links neither:\n{out}"
        );
    }
}

#[test]
fn a_project_that_has_not_opted_in_to_documents_gains_nothing() {
    let dir = code_project(&[], &[("docs/glossary/src.md", GLOSSARY)]);
    let out = context(dir.path(), "src/app.py::compute_total");

    assert!(!out.contains("Glossary"), "{out}");
    assert!(!out.contains("Doc links"), "{out}");
    assert!(
        !dir.path().join(".infigraph/docs.kuzu").exists(),
        "a read must not create the document store"
    );
}

#[test]
fn a_store_from_before_code_links_says_so_until_it_is_indexed_again() {
    let dir = project(&[("docs/glossary/src.md", GLOSSARY)]);
    {
        let store = infigraph_docs::store::DocStore::open(&dir.path().join(".infigraph/docs.kuzu"))
            .unwrap();
        let conn = store.connection().unwrap();
        conn.query("MATCH (l:CodeLink) DELETE l").unwrap();
        conn.query("MATCH (s:CodeLinkScan) DELETE s").unwrap();
    }

    let out = context(dir.path(), "src/app.py::compute_total");
    assert!(!out.contains("Sums the items"), "{out}");
    assert!(
        out.contains("documents are not scanned yet") && out.contains("infigraph index-docs"),
        "no degraded note:\n{out}"
    );
    let live = infigraph_core::degraded::live::for_root(dir.path());
    assert!(
        live.iter().any(|n| n.key == "doc-links-unscanned"),
        "{live:?}"
    );

    index_documents(dir.path()); // the content has not changed
    let out = context(dir.path(), "src/app.py::compute_total");
    assert!(out.contains("Sums the items"), "{out}");
    assert!(!out.contains("not scanned yet"), "{out}");
}

#[test]
fn a_qualified_mention_of_one_symbol_links_it() {
    let dir = code_project(
        &[],
        &[("docs/guide.md", "# Guide\n\nAdding is `Cart.add`.\n")],
    );
    index_documents(dir.path());

    let out = context(dir.path(), "src/app.py::Cart::add");
    assert!(out.contains("docs/guide.md:3"), "{out}");
}

fn doctor(dir: &Path) -> String {
    tool_doctor(&json!({ "path": dir.to_string_lossy() })).unwrap()
}

#[test]
fn doctor_lists_a_key_whose_symbol_was_renamed() {
    let dir = project(&[("docs/glossary/src.md", GLOSSARY)]);
    let healthy = doctor(dir.path());
    assert!(
        healthy.contains("keys resolve to a live symbol")
            && !healthy.contains("renamed or removed"),
        "{healthy}"
    );

    write(
        dir.path(),
        "src/app.py",
        &APP.replace("compute_total", "compute_sum"),
    );
    reindex_code(dir.path());
    let out = doctor(dir.path());

    assert!(
        out.contains("src/app.py::compute_total") && out.contains("no such symbol in the file"),
        "the renamed key is not listed:\n{out}"
    );
    assert!(out.contains("1 of 2 documents keys resolve"), "{out}");
}

#[test]
fn missing_symbols_lists_the_renamed_key_with_its_document_line_and_reason() {
    let dir = project(&[("docs/glossary/src.md", GLOSSARY)]);
    write(
        dir.path(),
        "src/app.py",
        &APP.replace("compute_total", "compute_sum"),
    );
    reindex_code(dir.path());

    let rows = infigraph_docs::link_check::missing_symbols(dir.path()).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.key, "src/app.py::compute_total");
    assert_eq!(row.document, "docs/glossary/src.md");
    assert!(row.line >= 1, "{row:?}");
    assert!(row.reason.contains("no such symbol"), "{row:?}");
}

#[test]
fn doctor_says_what_to_write_instead_of_a_dotted_key() {
    let dir = project(&[(
        "docs/glossary/src.md",
        "# G\n\n### `src/app.py::Cart.add`\n\nAdds.\n",
    )]);
    let out = doctor(dir.path());
    assert!(
        out.contains("did you mean src/app.py::Cart::add"),
        "no hint for the dotted spelling:\n{out}"
    );
}

#[test]
fn doctor_explains_a_key_for_a_name_the_file_has_several_of() {
    let twice = "def dup():\n    return 1\n\n\ndef dup():\n    return 2\n";
    let dir = code_project(
        &[("src/twice.py", twice)],
        &[(
            "docs/glossary/t.md",
            "# G\n\n### `src/twice.py::dup`\n\nWhich dup?\n",
        )],
    );
    index_documents(dir.path());
    let out = doctor(dir.path());
    assert!(
        out.contains("symbols have that name") && out.contains("src/twice.py::dup"),
        "{out}"
    );
}

#[test]
fn the_default_doctor_says_the_sync_check_was_not_run() {
    let dir = project(&[("docs/glossary/src.md", GLOSSARY)]);
    let out = doctor(dir.path());
    assert!(
        out.contains("not checked (run `infigraph doctor --docs-sync`)"),
        "{out}"
    );
}

#[test]
fn a_project_with_no_documents_gains_no_document_checks() {
    let dir = code_project(&[], &[]);
    let out = doctor(dir.path());
    assert!(!out.contains("document links"), "{out}");
}

#[test]
fn an_entry_older_than_its_code_says_so_until_it_is_touched_again() {
    let dir = code_project(&[], &[("docs/glossary/src.md", GLOSSARY)]);
    git(dir.path(), &["init", "-q"], "2024-01-01T00:00:00Z");
    commit(dir.path(), "2024-01-01T00:00:00Z");
    index_documents(dir.path());

    let fresh = context(dir.path(), "src/app.py::compute_total");
    assert!(fresh.contains("Glossary ("), "{fresh}");
    assert!(
        !fresh.contains("possibly stale"),
        "code and entry came in one commit:\n{fresh}"
    );

    // The code changes, same line count, and is committed later.
    write(
        dir.path(),
        "src/app.py",
        &APP.replace("total += item", "total += item * 2"),
    );
    commit(dir.path(), "2024-06-01T00:00:00Z");
    reindex_code(dir.path());
    let stale = context(dir.path(), "src/app.py::compute_total");
    assert!(
        stale.contains("possibly stale: code changed 2024-06-01")
            && stale.contains("entry last touched 2024-01-01"),
        "no staleness label:\n{stale}"
    );

    let report = infigraph_docs::link_check::check_project(dir.path(), true);
    let sync = report
        .iter()
        .find(|c| c.name.ends_with("doc/code sync"))
        .unwrap_or_else(|| panic!("no sync check in {report:#?}"));
    assert!(
        sync.message.contains("1 of 1 entries possibly stale")
            && sync.message.contains("src/app.py::compute_total"),
        "{}",
        sync.message
    );

    // Touching the entry settles it.
    write(
        dir.path(),
        "docs/glossary/src.md",
        &GLOSSARY.replace("generators.", "generators; still true."),
    );
    commit(dir.path(), "2024-07-01T00:00:00Z");
    index_documents(dir.path());
    let settled = context(dir.path(), "src/app.py::compute_total");
    assert!(!settled.contains("possibly stale"), "{settled}");
}

#[test]
fn outside_a_git_checkout_entries_show_without_a_label_and_the_mode_is_unknown() {
    let dir = project(&[("docs/glossary/src.md", GLOSSARY)]);
    let out = context(dir.path(), "src/app.py::compute_total");

    assert!(out.contains("Sums the items in a loop"), "{out}");
    assert!(
        out.contains("Doc/code sync not judged"),
        "no note about the missing history:\n{out}"
    );
    let live = infigraph_core::degraded::live::for_root(dir.path());
    assert!(live.iter().any(|n| n.key == "doc-sync-unknown"), "{live:?}");
}
