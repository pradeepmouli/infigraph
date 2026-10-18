//! `doc_entry`: edit a glossary entry's lines, create, rekey, delete or review
//! one. The report is `infigraph_docs::doc_entry`; this only reads arguments.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::Value;

use infigraph_docs::doc_entry::{
    refuse_nested_checkout, refuse_unnamed_write, run, with_root_line, Op, Pick,
};

use super::helpers::{resolve_project_path, startup_project};

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn need(args: &Value, key: &str, op: &str) -> Result<String> {
    match text(args, key) {
        Some(v) => Ok(v.to_string()),
        None => bail!("`{op}` needs `{key}`"),
    }
}

/// Which of several entries with one key the call means.
fn pick(args: &Value) -> Pick {
    let number = |key: &str| args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize);
    Pick {
        nth: number("nth"),
        line: number("line"),
        doc_file: text(args, "doc_file").map(str::to_string),
    }
}

pub fn tool_doc_entry(args: &Value) -> Result<String> {
    tool_doc_entry_in(args, &startup_project())
}

/// `tool_doc_entry` for a server started in `startup`, the project a call
/// without `path` falls back to.
pub fn tool_doc_entry_in(args: &Value, startup: &Path) -> Result<String> {
    let given = args.get("path").and_then(|v| v.as_str());
    let root = given
        .map(|p| PathBuf::from(resolve_project_path(p)))
        .unwrap_or_else(|| startup.to_path_buf());
    let root = root.canonicalize().unwrap_or(root);
    if let Some(path) = given {
        refuse_nested_checkout(&root, Path::new(path))?;
    }
    refuse_unnamed_write(&root, given.is_some(), flag(args, "apply"))?;
    let op_name = need(args, "op", "doc_entry")?;
    let op = match op_name.as_str() {
        "replace" => Op::Replace {
            target: need(args, "target", "replace")?,
            text: need(args, "text", "replace")?,
            section: text(args, "section").map(str::to_string),
            pick: pick(args),
        },
        "edit" => Op::Edit {
            target: need(args, "target", "edit")?,
            old: need(args, "old", "edit")?,
            new: need(args, "new", "edit")?,
            replace_all: flag(args, "replace_all"),
            pick: pick(args),
        },
        "create" => Op::Create {
            target: need(args, "target", "create")?,
            text: need(args, "text", "create")?,
            doc_file: text(args, "doc_file").map(str::to_string),
        },
        "rekey" => Op::Rekey {
            target: need(args, "target", "rekey")?,
            new_target: need(args, "new_target", "rekey")?,
            force: flag(args, "force"),
            ack: flag(args, "ack"),
            pick: pick(args),
        },
        "delete" => Op::Delete {
            target: need(args, "target", "delete")?,
            force: flag(args, "force"),
            pick: pick(args),
        },
        "ack" => {
            let mut targets: Vec<String> = args
                .get("targets")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(one) = text(args, "target") {
                targets.push(one.to_string());
            }
            if targets.is_empty() {
                bail!("`ack` needs `targets` (symbol ids whose entries were reviewed)");
            }
            Op::Ack { targets }
        }
        other => bail!(
            "unknown op `{other}`: use replace, edit, create, rekey, delete or ack (`op` is required)"
        ),
    };
    Ok(with_root_line(
        &root,
        &run(&root, &op, flag(args, "apply"))?,
    ))
}
