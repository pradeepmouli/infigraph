use anyhow::{Context, Result};
use serde_json::Value;

use infigraph_core::graph::GraphStore;

pub fn tool_detect_cross_cutting(args: &Value) -> Result<String> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let root = std::path::PathBuf::from(path)
        .canonicalize()
        .context("invalid path")?;
    let db_path = root.join(".infigraph").join("graph");
    let store = GraphStore::open(&db_path)?;

    let matches = infigraph_core::concerns::detect_cross_cutting(&store)?;

    let kind_filter = args
        .get("kind")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());

    let filtered: Vec<_> = if let Some(ref k) = kind_filter {
        matches
            .iter()
            .filter(|m| m.kind.to_lowercase() == *k)
            .cloned()
            .collect()
    } else {
        matches
    };

    Ok(infigraph_core::concerns::format_concerns(&filtered))
}
