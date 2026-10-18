//! `stale_docs`: the glossary entries whose code changed after they did, and
//! what changed. Read-only; the report is `infigraph_docs::stale_docs`, the one
//! judgement `doctor --docs-sync` shares.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use infigraph_core::path_scope::PathScope;
use infigraph_docs::missing_docs::{missing_docs, Options as MissingOptions};
use infigraph_docs::stale_docs::{stale_docs, Options};

use super::helpers::{resolve_project_path, startup_project};

pub fn tool_stale_docs(args: &Value) -> Result<String> {
    let root = args
        .get("path")
        .and_then(|v| v.as_str())
        .map(|p| PathBuf::from(resolve_project_path(p)))
        .unwrap_or_else(startup_project);
    let root = root.canonicalize().unwrap_or(root);
    let options = Options::new(
        PathScope::new(&root, args.get("path_prefix").and_then(|v| v.as_str())),
        args.get("symbol")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        args.get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize),
    );
    Ok(stale_docs(&root, &options).render())
}

/// `missing_docs`: the symbols a diff adds that no glossary entry describes.
pub fn tool_missing_docs(args: &Value) -> Result<String> {
    let root = args
        .get("path")
        .and_then(|v| v.as_str())
        .map(|p| PathBuf::from(resolve_project_path(p)))
        .unwrap_or_else(startup_project);
    let root = root.canonicalize().unwrap_or(root);
    let options = MissingOptions::from_args(
        PathScope::new(&root, args.get("path_prefix").and_then(|v| v.as_str())),
        args.get("base")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        args.get("include_changed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        args.get("visibility").and_then(|v| v.as_str()),
        args.get("min_lines")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize),
        args.get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize),
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    Ok(missing_docs(&root, &options).render())
}
