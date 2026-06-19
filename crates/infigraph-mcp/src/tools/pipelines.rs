use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

use super::docs::open_doc_index;

pub fn tool_pipeline_plugins(args: &Value) -> Result<String> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let project_dir = PathBuf::from(path).join("pipelines");
    let registry = infigraph_pipeline_plugin::load_pipeline_plugins(if project_dir.is_dir() {
        Some(project_dir.as_path())
    } else {
        None
    })?;

    if registry.is_empty() {
        return Ok("No pipeline plugins loaded.\n\nTo add plugins, create directories under ~/.infigraph/pipelines/ or <project>/pipelines/ with a plugin.toml file.".to_string());
    }

    let mut out = String::from("Loaded pipeline plugins:\n\n");
    for driver in registry.plugins() {
        let cfg = &driver.config().plugin;
        out.push_str(&format!(
            "- **{}** (id: `{}`)\n  Command: {:?}\n  Schema columns: {}\n  Detect patterns: {}\n\n",
            cfg.name,
            cfg.plugin_id,
            cfg.command,
            cfg.schema.len(),
            cfg.detect_patterns.len(),
        ));
    }
    Ok(out)
}

pub fn tool_pipeline_deps(args: &Value) -> Result<String> {
    let idx = open_doc_index(args)?;
    let store = idx.store().context("DocStore not initialized")?;

    let deps = store.get_pipeline_deps()?;
    if deps.is_empty() {
        return Ok("No pipeline dependencies found. Run pipeline indexing first.".to_string());
    }

    let mut out = format!("{} pipeline dependencies:\n\n", deps.len());
    for (from, to, dep_type) in &deps {
        out.push_str(&format!("  {} → {} ({})\n", from, to, dep_type));
    }
    Ok(out)
}

pub fn tool_pipeline_impact(args: &Value) -> Result<String> {
    let idx = open_doc_index(args)?;
    let store = idx.store().context("DocStore not initialized")?;

    let table_name = args
        .get("table_name")
        .and_then(|v| v.as_str())
        .context("missing 'table_name'")?;
    let max_depth = args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(3) as u32;

    let results = store.impact_analysis(table_name, max_depth)?;
    if results.is_empty() {
        return Ok(format!("No pipelines impacted by table '{table_name}'."));
    }

    let mut out = format!(
        "{} pipelines impacted by '{}':\n\n",
        results.len(),
        table_name
    );
    for r in &results {
        out.push_str(&format!(
            "  [depth={}] {} ({}) — {}\n",
            r.depth, r.pipeline_name, r.impact_type, r.path
        ));
    }
    Ok(out)
}

pub fn tool_pipeline_compliance(args: &Value) -> Result<String> {
    let idx = open_doc_index(args)?;
    let store = idx.store().context("DocStore not initialized")?;

    let scope = args
        .get("scope")
        .and_then(|v| v.as_str())
        .context("missing 'scope'")?;
    let plugin_id = args
        .get("plugin_id")
        .and_then(|v| v.as_str())
        .unwrap_or("intuit");

    let rows = store.query_plugin_table(plugin_id, "compliance", scope)?;
    if rows.is_empty() {
        return Ok(format!(
            "No pipelines matching compliance scope '{scope}' in plugin '{plugin_id}'."
        ));
    }

    let mut out = format!(
        "{} pipelines matching compliance '{}' (plugin: {}):\n\n",
        rows.len(),
        scope,
        plugin_id,
    );
    for row in &rows {
        out.push_str(&format!("  {}\n", row));
    }
    Ok(out)
}

pub fn tool_pipeline_query(args: &Value) -> Result<String> {
    let idx = open_doc_index(args)?;
    let store = idx.store().context("DocStore not initialized")?;

    let plugin_id = args
        .get("plugin_id")
        .and_then(|v| v.as_str())
        .context("missing 'plugin_id'")?;
    let field = args
        .get("field")
        .and_then(|v| v.as_str())
        .context("missing 'field'")?;
    let value = args
        .get("value")
        .and_then(|v| v.as_str())
        .context("missing 'value'")?;

    let rows = store.query_plugin_table(plugin_id, field, value)?;
    if rows.is_empty() {
        return Ok(format!(
            "No results for {field}='{value}' in Pipeline_{plugin_id}."
        ));
    }

    let mut out = format!(
        "{} results for {}='{}' in Pipeline_{}:\n\n",
        rows.len(),
        field,
        value,
        plugin_id,
    );
    for row in &rows {
        out.push_str(&format!("  {}\n", row));
    }
    Ok(out)
}
