use super::docs::{open_doc_index, tool_search_docs};
use super::helpers::degrade_banner;
use anyhow::{Context, Result};
use infigraph_core::search::service::{self, CacheKey, SearchData, SearchMode, SearchRequest};
use serde_json::Value;
use std::path::PathBuf;
use std::time::SystemTime;

pub fn drop_search_cache() {
    service::drop_cache();
}
pub(crate) fn evict_idle_search_cache(timeout: std::time::Duration) -> bool {
    service::evict_idle(timeout)
}
pub(crate) fn remote_cache_key() -> SystemTime {
    #[cfg(feature = "remote")]
    {
        use infigraph_core::meta::PostgresMetaStore;
        if let Ok(pg) = PostgresMetaStore::connect_from_env_cached() {
            let count = pg.embedding_count("symbol").unwrap_or(0) as u64;
            return std::time::UNIX_EPOCH + std::time::Duration::from_secs(count);
        }
    }
    std::time::UNIX_EPOCH
}

fn get_search_data_local(
    args: &Value,
    path: &str,
) -> Result<(SearchData, Option<infigraph_core::graph::DegradeReason>)> {
    let (prism, degrade_reason) = super::helpers::open_prism_read_only_or_degrade(args)?;
    let backend = prism.backend().context("not initialized")?;
    let rows = backend.get_symbols_for_search()?;

    let data = infigraph_core::search::service::data_from_rows(
        std::path::Path::new(path),
        rows,
        backend.current_ast_generation().ok(),
    )?;
    Ok((data, degrade_reason))
}

#[allow(unused)]
pub(crate) fn get_search_data_remote(path: &str) -> Result<SearchData> {
    #[cfg(feature = "remote")]
    {
        use infigraph_core::graph::{GraphBackend, Neo4jBackend};
        use infigraph_core::meta::PostgresMetaStore;

        // Scope to the repo this path resolves to (org/repo from the group registry).
        // Without this, remote search loads EVERY repo's symbols + embeddings from the
        // shared graph and returns cross-project results.
        let ns = infigraph_core::multi::Registry::load()
            .ok()
            .and_then(|reg| reg.resolve_repo_namespace(std::path::Path::new(path)));

        let mut backend = Neo4jBackend::connect_from_env()?;
        if let Some(ref ns) = ns {
            backend.set_repo_filter(ns);
        }
        let rows = backend.get_symbols_for_search()?;

        let pg = PostgresMetaStore::connect_from_env_cached()?;
        let mut symbol_embeddings = pg.all_embeddings("symbol")?;
        if let Some(ref ns) = ns {
            // Embedding ids are namespaced symbol ids (org/repo/file::sym); keep only this repo's.
            let prefix = format!("{ns}/");
            symbol_embeddings.retain(|(id, _)| id.starts_with(&prefix));
        }

        Ok((rows, symbol_embeddings))
    }
    #[cfg(not(feature = "remote"))]
    {
        anyhow::bail!("remote mode requires --features remote")
    }
}

fn run_search(args: &Value, mode: SearchMode) -> Result<String> {
    let request = SearchRequest::from_args(mode, args)?;
    let path = super::helpers::resolve_project_path(
        args.get("path").and_then(Value::as_str).unwrap_or("."),
    );
    let root = PathBuf::from(&path);
    let mut degraded = None;
    let reply = if infigraph_core::validated_backend()? == infigraph_core::BackendChoice::Daemon
        && !infigraph_core::graph::daemon_kuzu_backend::direct_reads_enabled()
    {
        // The same lifecycle/lease entry point used by CodeIndex reads.
        infigraph_core::daemon::lifecycle::ensure_daemon_for_routed_access(&root)?;
        infigraph_core::daemon::search::symbols(&root, request.clone())?
    } else {
        let remote = infigraph_core::daemon::lifecycle::is_remote_backend();
        let key = if remote {
            CacheKey::remote(&root, remote_cache_key())
        } else {
            CacheKey::local(&root, 0, 0)
        };
        service::execute(key, &request, || {
            let (data, reason) = if remote {
                (get_search_data_remote(&path)?, None)
            } else {
                get_search_data_local(args, &path)?
            };
            let cacheable = reason.is_none();
            degraded = reason;
            Ok((data, cacheable))
        })?
    };
    let mut out = reply.text;
    if reply.postlude {
        if args.get("scope").and_then(Value::as_str).unwrap_or("all") == "all" {
            if let Ok(doc_idx) = open_doc_index(args) {
                if let Some(store) = doc_idx.store() {
                    if let Ok(results) = infigraph_docs::search::hybrid_doc_search(
                        &request.query,
                        store,
                        &root,
                        (request.limit / 2).max(5),
                        0.5,
                    ) {
                        if !results.is_empty() {
                            out.push_str("\n---\nDocument matches:\n");
                            for dr in results {
                                out.push_str(&format!(
                                    "  [{}] {} (score: {:.2})\n",
                                    dr.doc_file,
                                    dr.heading.as_deref().unwrap_or(""),
                                    dr.score
                                ));
                                let snippet: String = dr.text.chars().take(200).collect();
                                if !snippet.is_empty() {
                                    out.push_str(&format!("    {}\n", snippet));
                                }
                            }
                        }
                    }
                }
            }
        }
        if out.ends_with("\n\n") {
            out.push_str(&format!("No results for '{}'", request.query));
        }
        if !super::watch::watcher_running(&root) {
            if let Some(message) = super::watch::auto_start_watch_opportunistic(&path) {
                out.push_str(&format!("\n✓ Auto-started watcher: {message}"));
            }
            super::docs::auto_start_doc_watch_opportunistic(&path);
        }
        if let Some(banner) = reply.staleness {
            crate::banner::prepend(&mut out, &banner);
        }
    }
    if let Some(reason) = degraded {
        crate::banner::prepend(&mut out, &degrade_banner(&reason));
    }
    Ok(out)
}
pub fn tool_search(args: &Value) -> Result<String> {
    if args.get("scope").and_then(Value::as_str) == Some("docs") {
        return tool_search_docs(args);
    }
    run_search(args, SearchMode::Unified)
}
pub fn tool_search_symbols(args: &Value) -> Result<String> {
    run_search(args, SearchMode::Symbols)
}
pub fn tool_semantic_search(args: &Value) -> Result<String> {
    run_search(args, SearchMode::Semantic)
}

pub fn tool_search_code(args: &Value) -> Result<String> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let pattern = args
        .get("pattern")
        .and_then(|p| p.as_str())
        .context("missing 'pattern'")?;
    let file_pattern = args.get("file_pattern").and_then(|f| f.as_str());
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(50) as usize;

    let root = PathBuf::from(path).canonicalize().context("invalid path")?;

    let matches = infigraph_core::search::grep_search(&root, pattern, file_pattern, limit)?;

    if matches.is_empty() {
        return Ok(format!("No matches for '{}'", pattern));
    }

    let mut out = format!("{} match(es):\n", matches.len());
    for m in &matches {
        out.push_str(&format!("{}:{}: {}\n", m.file, m.line_number, m.line_text));
    }
    Ok(out)
}
