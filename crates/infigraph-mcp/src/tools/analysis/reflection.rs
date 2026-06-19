use anyhow::{Context, Result};
use serde_json::Value;

use infigraph_core::graph::GraphStore;

pub fn tool_detect_reflection(args: &Value) -> Result<String> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let root = std::path::PathBuf::from(path)
        .canonicalize()
        .context("invalid path")?;
    let db_path = root.join(".infigraph").join("graph");
    let store = GraphStore::open(&db_path)?;

    let sites = infigraph_core::reflection::detect_reflection_sites(&store, &root)?;

    let mechanism_filter = args
        .get("mechanism")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());

    let filtered: Vec<_> = if let Some(ref m) = mechanism_filter {
        sites
            .iter()
            .filter(|s| s.mechanism.to_lowercase() == *m)
            .cloned()
            .collect()
    } else {
        sites
    };

    Ok(infigraph_core::reflection::format_reflection_sites(
        &filtered,
    ))
}
