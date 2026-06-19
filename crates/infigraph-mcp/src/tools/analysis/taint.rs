use anyhow::{Context, Result};
use serde_json::Value;

use infigraph_core::graph::GraphStore;

pub fn tool_detect_taint_flows(args: &Value) -> Result<String> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let root = std::path::PathBuf::from(path)
        .canonicalize()
        .context("invalid path")?;
    let db_path = root.join(".infigraph").join("graph");
    let store = GraphStore::open(&db_path)?;

    let flows = infigraph_core::taint::detect_taint_flows(&store, &root)?;

    let category_filter = args
        .get("category")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());
    let show_sanitized = args
        .get("show_sanitized")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let filtered: Vec<_> = flows
        .iter()
        .filter(|f| {
            category_filter
                .as_ref()
                .map_or(true, |c| f.sink_category.to_lowercase() == *c)
                && (show_sanitized || !f.sanitized)
        })
        .cloned()
        .collect();

    Ok(infigraph_core::taint::format_taint_flows(&filtered))
}

pub fn tool_detect_interprocedural_taint(args: &Value) -> Result<String> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let root = std::path::PathBuf::from(path)
        .canonicalize()
        .context("invalid path")?;
    let db_path = root.join(".infigraph").join("graph");
    let store = GraphStore::open(&db_path)?;

    let max_depth = args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(5) as u32;

    let flows = infigraph_core::taint::interprocedural::detect_interprocedural_taint(
        &store, &root, max_depth,
    )?;

    let category_filter = args
        .get("category")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());

    let filtered: Vec<_> = if let Some(ref c) = category_filter {
        flows
            .iter()
            .filter(|f| f.sink_category.to_lowercase() == *c)
            .cloned()
            .collect()
    } else {
        flows
    };

    Ok(infigraph_core::taint::interprocedural::format_interprocedural_flows(&filtered))
}

pub fn tool_detect_dynamic_urls(args: &Value) -> Result<String> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let root = std::path::PathBuf::from(path)
        .canonicalize()
        .context("invalid path")?;
    let db_path = root.join(".infigraph").join("graph");
    let store = GraphStore::open(&db_path)?;

    let urls = infigraph_core::taint::dynamic_urls::detect_dynamic_urls(&store, &root)?;

    Ok(infigraph_core::taint::dynamic_urls::format_dynamic_urls(
        &urls,
    ))
}

pub fn tool_detect_path_traversal(args: &Value) -> Result<String> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let root = std::path::PathBuf::from(path)
        .canonicalize()
        .context("invalid path")?;
    let db_path = root.join(".infigraph").join("graph");
    let store = GraphStore::open(&db_path)?;

    let max_depth = args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(5) as u32;

    let flows =
        infigraph_core::taint::path_traversal::detect_path_traversal(&store, &root, max_depth)?;

    Ok(infigraph_core::taint::path_traversal::format_path_traversal(&flows))
}
