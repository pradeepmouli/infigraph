use anyhow::{Context, Result};
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;
use std::path::Path;

pub(crate) fn cmd_search(root: &Path, query: &str, limit: usize, alpha: f32) -> Result<()> {
    use infigraph_core::embed;

    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;

    let backend = prism.backend().context("graph not initialized")?;
    let rows =
        backend.raw_query("MATCH (s:Symbol) RETURN s.id, s.name, s.kind, s.file, s.docstring")?;

    if rows.is_empty() {
        println!("No symbols found. Run 'infigraph index' first.");
        return Ok(());
    }

    let docs: Vec<(String, String)> = rows
        .iter()
        .map(|row| {
            let id = row[0].clone();
            let name = &row[1];
            let kind = &row[2];
            let doc = row.get(4).map(|s| s.as_str()).unwrap_or("");
            let text = if doc.is_empty() {
                format!("{} {}", kind, name)
            } else {
                format!("{} {}: {}", kind, name, doc)
            };
            (id, text)
        })
        .collect();

    let emb_path = root.join(".infigraph").join("embeddings.bin");
    let bm25_cache_path = root.join(".infigraph").join("bm25_cache.bin");
    let emb_mtime = std::fs::metadata(&emb_path).and_then(|m| m.modified()).ok();
    let cache_mtime = std::fs::metadata(&bm25_cache_path)
        .and_then(|m| m.modified())
        .ok();
    let cache_fresh = match (emb_mtime, cache_mtime) {
        (Some(e), Some(c)) => c >= e,
        _ => false,
    };
    let bm25_index = if cache_fresh {
        match infigraph_core::search::BM25Index::load(&bm25_cache_path) {
            Ok(idx) => idx,
            Err(_) => {
                let idx = infigraph_core::search::BM25Index::build(docs.clone());
                let _ = idx.save(&bm25_cache_path);
                idx
            }
        }
    } else {
        let idx = infigraph_core::search::BM25Index::build(docs.clone());
        let _ = idx.save(&bm25_cache_path);
        idx
    };

    let embedder = embed::best_embedder();

    let symbol_embeddings: Vec<(String, Vec<f32>)> = if emb_path.exists() {
        embed::load_embeddings_cached(&emb_path)?
    } else {
        eprintln!("hint: run 'infigraph index' to cache embeddings for faster search");
        docs.iter()
            .map(|(id, text)| (id.clone(), embedder.embed(text).unwrap_or_default()))
            .collect()
    };

    let hnsw_path = root.join(".infigraph").join("hnsw_index.usearch");
    let results = infigraph_core::search::hybrid_search(
        query,
        &bm25_index,
        embedder.as_ref(),
        &symbol_embeddings,
        limit,
        alpha,
        Some(&hnsw_path),
        Some(&emb_path),
    )?;

    if results.is_empty() {
        println!("No results for '{}'", query);
        return Ok(());
    }

    println!("Results for '{}' (alpha={:.1}):", query, alpha);
    for r in &results {
        if let Some(row) = rows.iter().find(|row| row[0] == r.symbol_id) {
            println!(
                "  {:.3} (bm25:{:.2} vec:{:.2})  {:>8} {:30} {}  id={}",
                r.score, r.bm25_score, r.vector_score, row[2], row[1], row[3], r.symbol_id
            );
            let doc = row.get(4).map(|s| s.as_str()).unwrap_or("");
            if !doc.is_empty() {
                let preview: String = doc.chars().take(80).collect();
                println!("         {}", preview);
            }
        }
    }

    Ok(())
}

pub(crate) fn cmd_search_code(
    root: &Path,
    pattern: &str,
    file_pattern: Option<&str>,
    limit: usize,
) -> Result<()> {
    let root = root.canonicalize().context("invalid project root")?;

    let matches = infigraph_core::search::grep_search(&root, pattern, file_pattern, limit)?;

    if matches.is_empty() {
        println!("No matches for '{}'", pattern);
        return Ok(());
    }

    println!("{} match(es):", matches.len());
    for m in &matches {
        println!("  {}:{}: {}", m.file, m.line_number, m.line_text);
    }

    Ok(())
}

pub(crate) fn cmd_snippet(root: &Path, symbol_id: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;

    let backend = prism.backend().context("graph not initialized")?;
    let detail = backend
        .find_symbol_by_id(symbol_id)?
        .context(format!("symbol '{}' not found in graph", symbol_id))?;

    let file_path = prism.root().join(&detail.file);
    let snippet = infigraph_core::search::read_lines_from_file(
        &file_path,
        detail.start_line,
        detail.end_line,
    )?;

    println!(
        "// {} {} ({}:L{}-{})",
        detail.kind, detail.name, detail.file, detail.start_line, detail.end_line
    );
    println!("{}", snippet);

    Ok(())
}

pub(crate) fn cmd_find_refs(root: &Path, symbol: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;
    let backend = prism.backend().context("graph not initialized")?;
    let refs = backend.find_all_references(symbol)?;
    if refs.is_empty() {
        println!("No references found for '{}'", symbol);
        return Ok(());
    }
    println!("References to '{}' ({} total):\n", symbol, refs.len());
    for r in &refs {
        println!("  {}:{:<6} in {}", r.file, r.line, r.caller_name);
    }
    Ok(())
}

pub(crate) fn cmd_search_docs(root: &Path, query: &str, limit: usize) -> Result<()> {
    let idx = match infigraph_docs::DocIndex::open_existing(root) {
        Ok(idx) => idx,
        Err(e) if e.is::<infigraph_core::docs_switch::DocsNotIndexed>() => {
            println!("{e}");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let store = idx.store().context("doc store not initialized")?;

    let results = infigraph_docs::search::hybrid_doc_search(query, store, root, limit, 0.5)?;
    if results.is_empty() {
        println!("No results. Run 'infigraph index-docs' first.");
        return Ok(());
    }
    for r in &results {
        let heading = r.heading.as_deref().unwrap_or("(no heading)");
        println!("{:.3}  {} > {}", r.score, r.doc_file, heading);
        let snippet: String = r.text.chars().take(120).collect();
        println!("      {}\n", snippet);
    }
    Ok(())
}

/// The prose for `replace` and `create`: `--text`, or `--text-file` ('-' is
/// stdin).
fn prose(text: Option<String>, file: Option<std::path::PathBuf>) -> Result<String> {
    match (text, file) {
        (Some(text), _) => Ok(text),
        (None, Some(path)) if path.as_os_str() == "-" => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("cannot read the text from stdin")?;
            Ok(buf)
        }
        (None, Some(path)) => std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display())),
        (None, None) => anyhow::bail!("this needs the prose: --text <TEXT> or --text-file <FILE>"),
    }
}

/// `infigraph doc-guard`: the pre-edit hook's decision. Reads the tool call as
/// JSON on stdin and prints the deny decision (Claude Code's `PreToolUse`
/// output) when it would edit a glossary entry by hand. Prints nothing, and
/// never fails, otherwise: a guard that cannot decide allows.
pub(crate) fn cmd_doc_guard() {
    use std::io::Read;
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(call) = serde_json::from_str::<serde_json::Value>(&input) else {
        return;
    };
    if let infigraph_docs::doc_guard::Decision::Deny(reason) =
        infigraph_docs::doc_guard::decide(&call)
    {
        println!(
            "{}",
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }})
        );
    }
}

/// `infigraph doc-entry`: the same operations as the MCP `doc_entry` tool.
pub(crate) fn cmd_doc_entry(root: &Path, op: crate::DocEntryOp, apply: bool) -> Result<()> {
    use crate::DocEntryOp as O;
    use infigraph_docs::doc_entry::Op;
    let op = match op {
        O::Edit {
            target,
            old,
            new,
            replace_all,
        } => Op::Edit {
            target,
            old,
            new,
            replace_all,
        },
        O::Replace {
            target,
            text,
            text_file,
            section,
        } => Op::Replace {
            target,
            text: prose(text, text_file)?,
            section,
        },
        O::Create {
            target,
            text,
            text_file,
            doc_file,
        } => Op::Create {
            target,
            text: prose(text, text_file)?,
            doc_file,
        },
        O::Rekey {
            target,
            new_target,
            force,
            ack,
        } => Op::Rekey {
            target,
            new_target,
            force,
            ack,
        },
        O::Delete { target, force } => Op::Delete { target, force },
        O::Ack { targets } => Op::Ack { targets },
    };
    print!("{}", infigraph_docs::doc_entry::run(root, &op, apply)?);
    Ok(())
}

/// `infigraph stale-docs`: the same report as the MCP `stale_docs` tool.
pub(crate) fn cmd_stale_docs(
    root: &Path,
    path_prefix: Option<&str>,
    symbol: Option<String>,
    limit: usize,
) -> Result<()> {
    let options = infigraph_docs::stale_docs::Options::new(
        infigraph_core::path_scope::PathScope::new(root, path_prefix),
        symbol,
        Some(limit),
    );
    print!(
        "{}",
        infigraph_docs::stale_docs::stale_docs(root, &options).render()
    );
    Ok(())
}
