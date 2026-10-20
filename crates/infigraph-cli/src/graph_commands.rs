use std::path::Path;

use anyhow::{Context, Result};
use infigraph_core::graph::find_dead_code;
use infigraph_core::path_scope::PathScope;
use infigraph_core::CodeIndex;
use infigraph_languages::bundled_registry;

pub(crate) fn cmd_query(root: &Path, cypher: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = CodeIndex::open(root, registry)?;
    // Read-only, matching the `query_graph` MCP tool: `cypher` is arbitrary
    // user input, but rather than inspect it for write statements, this
    // relies on the backend's own read-only enforcement to reject them
    // clearly ("Cannot execute write operations in a read-only database")
    // -- exactly like `query_graph` already does.
    prism.init_read_only()?;

    let backend = prism.backend().context("graph not initialized")?;
    let rows = backend.raw_query(cypher)?;
    for row in &rows {
        println!("{}", row.join(" | "));
    }
    if rows.is_empty() {
        println!("(no results)");
    }
    Ok(())
}

pub(crate) fn cmd_export(
    root: &Path,
    format: &str,
    output: Option<std::path::PathBuf>,
) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = CodeIndex::open(root, registry)?;
    prism.init()?;

    let backend = prism.backend().context("graph not initialized")?;

    match output {
        Some(path) => {
            let file = std::fs::File::create(&path)
                .with_context(|| format!("failed to create output file: {}", path.display()))?;
            let mut writer = std::io::BufWriter::new(file);
            export_to_writer(backend, format, &mut writer)?;
            println!("Exported {} to {}", format, path.display());
        }
        None => {
            let stdout = std::io::stdout();
            let mut writer = std::io::BufWriter::new(stdout.lock());
            export_to_writer(backend, format, &mut writer)?;
        }
    }

    Ok(())
}

pub(crate) fn cmd_callers(root: &Path, symbol: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = CodeIndex::open(root, registry)?;
    prism.init()?;

    let backend = prism.backend().context("graph not initialized")?;
    let callers = backend.callers_of(symbol)?;
    if callers.is_empty() {
        println!("No callers found for '{}'", symbol);
        return Ok(());
    }

    println!("Callers of '{}' ({}):", symbol, callers.len());
    for caller in &callers {
        println!("  {}", caller);
    }
    Ok(())
}

pub(crate) fn cmd_callees(root: &Path, symbol: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = CodeIndex::open(root, registry)?;
    prism.init()?;

    let backend = prism.backend().context("graph not initialized")?;
    let callees = backend.callees_of(symbol)?;
    if callees.is_empty() {
        println!("No callees found for '{}'", symbol);
        return Ok(());
    }

    println!("Callees of '{}' ({}):", symbol, callees.len());
    for callee in &callees {
        println!("  {}", callee);
    }
    Ok(())
}

pub(crate) fn cmd_dead_code(root: &Path, path_prefix: Option<&str>) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = CodeIndex::open(root, registry)?;
    prism.init()?;

    let backend = prism.backend().context("graph not initialized")?;
    let scope = PathScope::new(prism.root(), path_prefix);
    let dead = find_dead_code(backend, &scope)?;

    if dead.is_empty() {
        match scope.prefix() {
            Some(prefix) => println!("No dead code found under {prefix}."),
            None => println!(
                "No dead code found (all non-entry-point, non-vendored functions have callers)."
            ),
        }
        return Ok(());
    }

    if let Some(prefix) = scope.prefix() {
        println!("Scope: {prefix}");
    }
    println!("Potentially dead code ({} symbols):", dead.len());
    let mut current_file = "";
    for r in &dead {
        if r.file != current_file {
            current_file = &r.file;
            println!("\n  {}:", current_file);
        }
        println!("    {:>8} {}", r.kind, r.name);
    }

    Ok(())
}

pub(crate) fn cmd_impact(root: &Path, symbol: &str, depth: u32) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = CodeIndex::open(root, registry)?;
    prism.init()?;

    let backend = prism.backend().context("graph not initialized")?;
    let impacted = backend.transitive_impact(symbol, depth)?;

    if impacted.is_empty() {
        println!("No symbols affected by changes to '{}'", symbol);
        return Ok(());
    }

    println!(
        "Symbols affected by changes to '{}' (depth={}):",
        symbol, depth
    );
    for row in &impacted {
        println!("  {:>8} {:30} {}", row.kind, row.name, row.file);
    }

    Ok(())
}

fn export_to_writer<W: std::io::Write>(
    backend: &dyn infigraph_core::graph::GraphBackend,
    format: &str,
    writer: &mut W,
) -> anyhow::Result<()> {
    match format {
        "cypher" => infigraph_core::export::export_cypher(backend, writer),
        "graphml" => infigraph_core::export::export_graphml(backend, writer),
        "json" => infigraph_core::export::export_json(backend, writer),
        _ => anyhow::bail!(
            "unknown export format '{}'. Supported formats: cypher, graphml, json",
            format
        ),
    }
}
