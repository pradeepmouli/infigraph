use std::path::Path;

use anyhow::{Context, Result};

pub(crate) fn cmd_pipeline_plugins(root: &Path) -> Result<()> {
    let project_dir = root.join("pipelines");
    let registry = infigraph_pipeline_plugin::load_pipeline_plugins(if project_dir.is_dir() {
        Some(project_dir.as_path())
    } else {
        None
    })?;

    if registry.is_empty() {
        println!(
            "No pipeline plugins loaded.\n\n\
             To add plugins, create directories under ~/.infigraph/pipelines/ \
             or <project>/pipelines/ with a plugin.toml file."
        );
        return Ok(());
    }

    println!("Loaded pipeline plugins:\n");
    for driver in registry.plugins() {
        let cfg = &driver.config().plugin;
        println!(
            "- {} (id: {})\n  Command: {:?}\n  Schema columns: {}\n  Detect patterns: {}\n",
            cfg.name,
            cfg.plugin_id,
            cfg.command,
            cfg.schema.len(),
            cfg.detect_patterns.len(),
        );
    }
    Ok(())
}

pub(crate) fn cmd_pipeline_deps(root: &Path) -> Result<()> {
    let idx = infigraph_docs::DocIndex::open_existing(root)?;
    let store = idx.store().context("DocStore not initialized")?;

    let deps = store.get_pipeline_deps()?;
    if deps.is_empty() {
        println!("No pipeline dependencies found. Run pipeline indexing first.");
        return Ok(());
    }

    println!("{} pipeline dependencies:\n", deps.len());
    for (from, to, dep_type) in &deps {
        println!("  {} → {} ({})", from, to, dep_type);
    }
    Ok(())
}

pub(crate) fn cmd_pipeline_impact(root: &Path, table_name: &str, max_depth: u32) -> Result<()> {
    let idx = infigraph_docs::DocIndex::open_existing(root)?;
    let store = idx.store().context("DocStore not initialized")?;

    let results = store.impact_analysis(table_name, max_depth)?;
    if results.is_empty() {
        println!("No pipelines impacted by table '{}'.", table_name);
        return Ok(());
    }

    println!(
        "{} pipelines impacted by '{}':\n",
        results.len(),
        table_name
    );
    for r in &results {
        println!(
            "  [depth={}] {} ({}) — {}",
            r.depth, r.pipeline_name, r.impact_type, r.path
        );
    }
    Ok(())
}

pub(crate) fn cmd_pipeline_compliance(root: &Path, scope: &str, plugin_id: &str) -> Result<()> {
    let idx = infigraph_docs::DocIndex::open_existing(root)?;
    let store = idx.store().context("DocStore not initialized")?;

    let rows = store.query_plugin_table(plugin_id, "compliance", scope)?;
    if rows.is_empty() {
        println!(
            "No pipelines matching compliance scope '{}' in plugin '{}'.",
            scope, plugin_id
        );
        return Ok(());
    }

    println!(
        "{} pipelines matching compliance '{}' (plugin: {}):\n",
        rows.len(),
        scope,
        plugin_id,
    );
    for row in &rows {
        println!("  {}", row);
    }
    Ok(())
}

pub(crate) fn cmd_pipeline_query(
    root: &Path,
    plugin_id: &str,
    field: &str,
    value: &str,
) -> Result<()> {
    let idx = infigraph_docs::DocIndex::open_existing(root)?;
    let store = idx.store().context("DocStore not initialized")?;

    let rows = store.query_plugin_table(plugin_id, field, value)?;
    if rows.is_empty() {
        println!(
            "No results for {}='{}' in Pipeline_{}.",
            field, value, plugin_id
        );
        return Ok(());
    }

    println!(
        "{} results for {}='{}' in Pipeline_{}:\n",
        rows.len(),
        field,
        value,
        plugin_id,
    );
    for row in &rows {
        println!("  {}", row);
    }
    Ok(())
}
