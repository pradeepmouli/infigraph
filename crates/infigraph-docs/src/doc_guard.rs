//! The edit guard: a pre-edit hook decides whether an agent's `Edit`,
//! `MultiEdit` or `Write` of a Markdown file would change a glossary entry,
//! and if so points at `doc_entry` instead.
//!
//! An entry edited by hand through the agent's editor is where the lines go
//! wrong: a rekey that misses the heading, a body replaced with a transcription
//! error, an index that is not told. `doc_entry` does the mechanical part, so
//! the guard sends entry edits there (`edit` for a small fix, `replace` for a
//! rewrite) and leaves everything else alone.
//!
//! The decision reads the file as it lies now, with the scan the indexer
//! stores ([`code_links::entries`]): no store is opened, no daemon is asked,
//! and an index that has fallen behind cannot make the guard wrong. The only
//! configuration it reads is whether the project indexes documents. Anything it
//! cannot decide (no project, an unreadable file, an unknown tool, a parse
//! failure) is allowed: the guard is a nudge, never a gate that can wedge an
//! edit. It does not see the shell, so `sed -i` on a glossary is out of its
//! reach.
//!
//! The hook input is Claude Code's `PreToolUse` JSON (`tool_name`, `cwd`,
//! `tool_input`). Another agent's hook needs only to pass the same fields.

use std::path::{Path, PathBuf};

use infigraph_core::docs_switch;
use serde_json::Value;

use crate::code_links::{entries, Entry};

/// What the guard decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Refuse the tool call; the text is what the agent is told.
    Deny(String),
}

/// The decision for one hook input.
pub fn decide(input: &Value) -> Decision {
    let tool = input["tool_name"].as_str().unwrap_or_default();
    if !matches!(tool, "Edit" | "MultiEdit" | "Write") {
        return Decision::Allow;
    }
    let tool_input = &input["tool_input"];
    let Some(path) = tool_input["file_path"].as_str() else {
        return Decision::Allow;
    };
    let cwd = input["cwd"].as_str().map(PathBuf::from);
    let path = match (Path::new(path).is_absolute(), &cwd) {
        (true, _) => PathBuf::from(path),
        (false, Some(cwd)) => cwd.join(path),
        (false, None) => PathBuf::from(path),
    };
    if !is_markdown(&path) {
        return Decision::Allow;
    }
    let Some(root) = project_root(&path) else {
        return Decision::Allow;
    };
    if !docs_switch::docs_enabled(&root) {
        return Decision::Allow;
    }
    // A file that cannot be read (a new one, for a Write) has no entries.
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Decision::Allow;
    };
    let present = entries(&text);
    if present.is_empty() {
        return Decision::Allow;
    }
    let shown = path
        .strip_prefix(&root)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();
    match tool {
        "Write" => Decision::Deny(deny_message(&format!(
            "this replaces {shown}, which holds {} glossary {}",
            present.len(),
            if present.len() == 1 {
                "entry"
            } else {
                "entries"
            }
        ))),
        "Edit" => {
            let old = tool_input["old_string"].as_str().unwrap_or_default();
            first_hit(&text, &present, &[old], &shown)
        }
        _ => {
            let edits: Vec<&str> = tool_input["edits"]
                .as_array()
                .map(|a| a.iter().filter_map(|e| e["old_string"].as_str()).collect())
                .unwrap_or_default();
            first_hit(&text, &present, &edits, &shown)
        }
    }
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// The nearest ancestor of `path` that has a `.infigraph/`, the project it
/// belongs to (a worktree has its own).
fn project_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|dir| dir.join(".infigraph").is_dir())
        .map(Path::to_path_buf)
}

/// Denies at the first entry that any of the `olds` overlaps, else allows.
fn first_hit(text: &str, present: &[Entry], olds: &[&str], file: &str) -> Decision {
    for old in olds.iter().filter(|o| !o.is_empty()) {
        for (from, to) in line_ranges(text, old) {
            if let Some(hit) = present.iter().find(|e| from <= e.end && to >= e.start) {
                return Decision::Deny(deny_message(&format!(
                    "this changes the glossary entry for `{}` ({file}:{}-{})",
                    hit.key, hit.start, hit.end
                )));
            }
        }
    }
    Decision::Allow
}

/// The 1-based, inclusive lines each occurrence of `needle` in `text` touches.
fn line_ranges(text: &str, needle: &str) -> Vec<(usize, usize)> {
    let span = needle.trim_end_matches('\n').matches('\n').count();
    text.match_indices(needle)
        .map(|(at, _)| {
            let from = 1 + text[..at].matches('\n').count();
            (from, from + span)
        })
        .collect()
}

fn deny_message(what: &str) -> String {
    format!(
        "BLOCKED: {what}. Change glossary entries with doc_entry (CLI: `infigraph doc-entry \
         <op>`), which finds the exact lines and keeps the document index in step: op `edit` \
         (old -> new) for a small fix, `replace` for a rewrite, `rekey` for a renamed symbol, \
         `delete`, `create`, or `ack` to mark an entry reviewed. Edits outside entries are not \
         affected. If the doc_entry tool is unavailable, use the CLI or tell the user (e.g. to \
         reconnect the infigraph MCP server with /mcp) -- do not work around this hook."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DOC: &str = "# Glossary\n\nIntro text.\n\n### `src/a.py::f`\n\nWhy f.\n\n#### body\n\nInside f.\n\n### `src/a.py::g`\n\nWhy g.\n\n## Notes\n\nOutside every entry.\n";

    /// A project with docs on and `docs/g.md` written; the hook input for `input`.
    fn project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        std::fs::write(
            root.join(".infigraph/config.toml"),
            "[docs]\nenabled = true\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/g.md"), DOC).unwrap();
        (dir, root)
    }

    fn edit(root: &Path, old: &str) -> Value {
        json!({
            "tool_name": "Edit", "cwd": root,
            "tool_input": {"file_path": root.join("docs/g.md"), "old_string": old, "new_string": "x"},
        })
    }

    fn denied(d: Decision) -> String {
        match d {
            Decision::Deny(why) => why,
            Decision::Allow => panic!("allowed"),
        }
    }

    #[test]
    fn an_edit_inside_an_entry_is_denied_and_names_the_entry_and_the_ops() {
        let (_dir, root) = project();
        let why = denied(decide(&edit(&root, "Why f.")));
        assert!(
            why.contains("`src/a.py::f`") && why.contains("docs/g.md:5-11"),
            "{why}"
        );
        assert!(why.contains("`edit`") && why.contains("`replace`"), "{why}");
    }

    #[test]
    fn it_needs_no_index_at_all() {
        let (_dir, root) = project();
        assert!(
            !infigraph_core::docs_switch::docs_store_path(&root).exists(),
            "this project has no document store"
        );
        assert!(denied(decide(&edit(&root, "Why g."))).contains("src/a.py::g"));
        assert_eq!(decide(&edit(&root, "Intro text.")), Decision::Allow);
    }

    #[test]
    fn a_sub_entry_belongs_to_its_entry() {
        let (_dir, root) = project();
        assert!(denied(decide(&edit(&root, "Inside f."))).contains("src/a.py::f"));
    }

    #[test]
    fn the_key_heading_itself_is_part_of_the_entry() {
        let (_dir, root) = project();
        assert!(denied(decide(&edit(&root, "### `src/a.py::g`"))).contains("src/a.py::g"));
    }

    #[test]
    fn an_edit_outside_every_entry_is_allowed() {
        let (_dir, root) = project();
        assert_eq!(decide(&edit(&root, "Intro text.")), Decision::Allow);
        assert_eq!(
            decide(&edit(&root, "Outside every entry.")),
            Decision::Allow
        );
    }

    #[test]
    fn an_edit_spanning_into_an_entry_is_denied() {
        let (_dir, root) = project();
        assert!(
            denied(decide(&edit(&root, "Intro text.\n\n### `src/a.py::f`")))
                .contains("src/a.py::f")
        );
    }

    #[test]
    fn replace_all_checks_every_occurrence() {
        let (dir, root) = project();
        std::fs::write(
            root.join("docs/g.md"),
            "# G\n\nTwice: foo.\n\n### `src/a.py::f`\n\nTwice: foo.\n",
        )
        .unwrap();
        let input = json!({
            "tool_name": "Edit", "cwd": root,
            "tool_input": {"file_path": root.join("docs/g.md"), "old_string": "foo", "new_string": "bar", "replace_all": true},
        });
        assert!(denied(decide(&input)).contains("src/a.py::f"));
        drop(dir);
    }

    #[test]
    fn a_multi_edit_is_denied_when_any_of_its_edits_hits_an_entry() {
        let (_dir, root) = project();
        let input = json!({
            "tool_name": "MultiEdit", "cwd": root,
            "tool_input": {"file_path": root.join("docs/g.md"), "edits": [
                {"old_string": "Intro text.", "new_string": "a"},
                {"old_string": "Why g.", "new_string": "b"},
            ]},
        });
        assert!(denied(decide(&input)).contains("src/a.py::g"));
        let outside = json!({
            "tool_name": "MultiEdit", "cwd": root,
            "tool_input": {"file_path": root.join("docs/g.md"), "edits": [
                {"old_string": "Intro text.", "new_string": "a"},
                {"old_string": "Outside every entry.", "new_string": "b"},
            ]},
        });
        assert_eq!(decide(&outside), Decision::Allow);
    }

    #[test]
    fn a_write_over_a_file_with_entries_is_denied_but_a_new_file_is_not() {
        let (_dir, root) = project();
        let over = json!({
            "tool_name": "Write", "cwd": root,
            "tool_input": {"file_path": root.join("docs/g.md"), "content": "# gone\n"},
        });
        assert!(denied(decide(&over)).contains("2 glossary entries"));
        let fresh = json!({
            "tool_name": "Write", "cwd": root,
            "tool_input": {"file_path": root.join("docs/new.md"), "content": "# new\n"},
        });
        assert_eq!(decide(&fresh), Decision::Allow);
    }

    #[test]
    fn a_relative_path_is_read_against_the_cwd() {
        let (_dir, root) = project();
        let input = json!({
            "tool_name": "Edit", "cwd": root,
            "tool_input": {"file_path": "docs/g.md", "old_string": "Why f.", "new_string": "x"},
        });
        assert!(matches!(decide(&input), Decision::Deny(_)));
    }

    #[test]
    fn it_allows_whatever_it_cannot_judge() {
        let (_dir, root) = project();
        // Not Markdown, not a file tool, no path, no project.
        let code = json!({"tool_name": "Edit", "cwd": root,
            "tool_input": {"file_path": root.join("a.py"), "old_string": "x", "new_string": "y"}});
        assert_eq!(decide(&code), Decision::Allow);
        assert_eq!(decide(&json!({"tool_name": "Bash"})), Decision::Allow);
        assert_eq!(
            decide(&json!({"tool_name": "Edit", "tool_input": {}})),
            Decision::Allow
        );
        let stray = tempfile::tempdir().unwrap();
        std::fs::write(stray.path().join("g.md"), DOC).unwrap();
        let outside = json!({"tool_name": "Edit",
            "tool_input": {"file_path": stray.path().join("g.md"), "old_string": "Why f.", "new_string": "x"}});
        assert_eq!(decide(&outside), Decision::Allow);
        // A file that vanished.
        std::fs::remove_file(root.join("docs/g.md")).unwrap();
        assert_eq!(decide(&edit(&root, "Why f.")), Decision::Allow);
    }

    #[test]
    fn a_project_that_does_not_index_documents_is_left_alone() {
        let (_dir, root) = project();
        std::fs::write(
            root.join(".infigraph/config.toml"),
            "[docs]\nenabled = false\n",
        )
        .unwrap();
        assert_eq!(decide(&edit(&root, "Why f.")), Decision::Allow);
    }

    #[test]
    fn a_markdown_file_without_entries_is_left_alone() {
        let (_dir, root) = project();
        std::fs::write(root.join("docs/plain.md"), "# Plain\n\nJust prose.\n").unwrap();
        let input = json!({"tool_name": "Write", "cwd": root,
            "tool_input": {"file_path": root.join("docs/plain.md"), "content": "x"}});
        assert_eq!(decide(&input), Decision::Allow);
    }
}
