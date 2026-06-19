use anyhow::{Context, Result};
use serde_json::Value;

use infigraph_core::graph::GraphStore;

pub fn tool_detect_config_bindings(args: &Value) -> Result<String> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let root = std::path::PathBuf::from(path)
        .canonicalize()
        .context("invalid path")?;
    let db_path = root.join(".infigraph").join("graph");
    let store = GraphStore::open(&db_path)?;

    let bindings = infigraph_core::config::detect_config_bindings(&store)?;
    let config_files = infigraph_core::config::detect_config_files(&root);

    let kind_filter = args
        .get("kind")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());
    let profile_filter = args
        .get("profile")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());

    let filtered: Vec<_> = bindings
        .iter()
        .filter(|b| {
            kind_filter
                .as_ref()
                .map_or(true, |k| b.kind.to_lowercase() == *k)
                && profile_filter
                    .as_ref()
                    .map_or(true, |p| b.profile.to_lowercase() == *p)
        })
        .cloned()
        .collect();

    Ok(infigraph_core::config::format_config_bindings(
        &filtered,
        &config_files,
    ))
}
