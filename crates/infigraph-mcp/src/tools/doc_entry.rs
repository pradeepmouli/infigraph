//! `doc_entry`: edit a glossary entry's lines, create, rekey, delete or review
//! one. The report is `infigraph_docs::doc_entry`; this only reads arguments.

use std::path::PathBuf;

use anyhow::{bail, Result};
use serde_json::Value;

use infigraph_docs::doc_entry::{run, Op};

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

pub fn tool_doc_entry(args: &Value) -> Result<String> {
    let root = args
        .get("path")
        .and_then(|v| v.as_str())
        .map(|p| PathBuf::from(resolve_project_path(p)))
        .unwrap_or_else(startup_project);
    let root = root.canonicalize().unwrap_or(root);
    let op_name = need(args, "op", "doc_entry")?;
    let op = match op_name.as_str() {
        "replace" => Op::Replace {
            target: need(args, "target", "replace")?,
            text: need(args, "text", "replace")?,
            section: text(args, "section").map(str::to_string),
        },
        "edit" => Op::Edit {
            target: need(args, "target", "edit")?,
            old: need(args, "old", "edit")?,
            new: need(args, "new", "edit")?,
            replace_all: flag(args, "replace_all"),
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
        },
        "delete" => Op::Delete {
            target: need(args, "target", "delete")?,
            force: flag(args, "force"),
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
    run(&root, &op, flag(args, "apply"))
}
