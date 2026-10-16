use anyhow::{Context, Result};
use serde_json::Value;

use infigraph_core::clone_detect::find::{find_clones, store_similar_edges, Options};

use super::super::helpers::{open_prism, path_scope};

pub fn tool_detect_clones(args: &Value) -> Result<String> {
    let prism = open_prism(args)?;
    let threshold = args
        .get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.92) as f32;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
    let store_edges = args
        .get("store_edges")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let kinds = args
        .get("kinds")
        .and_then(|v| v.as_str())
        .unwrap_or("Function,Method");

    let backend = prism.backend().context("not initialized")?;

    // Pairs are compared within the scope, so a clone of in-scope code that
    // lives outside it is not looked for.
    let scope = path_scope(&prism, args);
    let options = Options::for_project(prism.root(), threshold, limit, kinds, scope.clone())?;
    let report = find_clones(backend, prism.root(), &options)?;

    if report.symbols < 2 {
        let mut out = match scope.prefix() {
            Some(prefix) => format!("Not enough symbols under {prefix} to compare."),
            None => "Not enough symbols to compare. Run index_project first.".to_string(),
        };
        for note in report.caveats() {
            out.push_str(&format!("\n{note}"));
        }
        return Ok(out);
    }

    if report.pairs.is_empty() {
        let under = scope
            .prefix()
            .map(|prefix| format!(" under {prefix}"))
            .unwrap_or_default();
        let mut out = format!(
            "No clones found above threshold {:.2} across {} symbols ({}){under}.",
            threshold, report.symbols, kinds
        );
        for note in report.caveats() {
            out.push_str(&format!("\n{note}"));
        }
        return Ok(out);
    }

    if store_edges {
        for warning in store_similar_edges(backend, &report.pairs) {
            crate::mcp_log("WARN", &warning);
        }
    }

    let mut out = String::new();
    if let Some(prefix) = scope.prefix() {
        out.push_str(&format!("Scope: {prefix}\n"));
    }
    out.push_str(&format!(
        "Clone detection: {} pairs found (threshold={:.2}, symbols={}, kinds={})\n",
        report.pairs.len(),
        threshold,
        report.symbols,
        kinds
    ));
    out.push_str(&format!("({})\n", report.how()));
    for note in report.caveats() {
        out.push_str(&format!("{note}\n"));
    }
    out.push('\n');

    for pair in &report.pairs {
        out.push_str(&format!(
            "{:.3}  {} ({}) <-> {} ({})  [{}]\n       {} vs {}\n",
            pair.score,
            pair.a.name,
            pair.a.id,
            pair.b.name,
            pair.b.id,
            pair.signal.label(),
            pair.a.file,
            pair.b.file
        ));
    }

    Ok(out)
}

pub fn tool_refactor(args: &Value) -> Result<String> {
    let prism = open_prism(args)?;
    let backend = prism.backend().context("not initialized")?;

    let target = args.get("target").and_then(|v| v.as_str());
    let focus_str = args.get("focus").and_then(|v| v.as_str()).unwrap_or("all");
    let focus = infigraph_core::refactor::Focus::parse(focus_str);
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

    let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
    let emb_path = std::path::PathBuf::from(path)
        .join(".infigraph")
        .join("embeddings.bin");
    let emb_ref = if emb_path.exists() {
        Some(emb_path.as_path())
    } else {
        None
    };

    let recs = infigraph_core::refactor::analyze(backend, emb_ref, target, focus, limit)?;
    Ok(infigraph_core::refactor::format_recommendations(
        &recs, target,
    ))
}
