use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::Value;

use infigraph_core::embed;
use infigraph_core::search::BM25Index;

use super::docs::{open_doc_index, tool_search_docs};
use super::helpers::{degrade_banner, find_containing_symbol};

type SearchData = (Vec<Vec<String>>, Vec<(String, Vec<f32>)>);

struct SearchContext {
    db_path: PathBuf,
    db_mtime: SystemTime,
    rows: Arc<Vec<Vec<String>>>,
    bm25_unfiltered: Arc<BM25Index>,
    docs_unfiltered: Arc<Vec<(String, String)>>,
    symbol_embeddings: Arc<Vec<(String, Vec<f32>)>>,
}

static SEARCH_CTX: OnceLock<Mutex<Option<SearchContext>>> = OnceLock::new();

fn search_ctx_lock() -> &'static Mutex<Option<SearchContext>> {
    SEARCH_CTX.get_or_init(|| Mutex::new(None))
}

fn build_docs_from_rows(rows: &[Vec<String>]) -> Vec<(String, String)> {
    rows.iter()
        .map(|row| {
            let id = row[0].clone();
            let text = if row.get(4).is_some_and(|s| !s.is_empty()) {
                format!("{} {}: {}", row[2], row[1], row[4])
            } else {
                format!("{} {}", row[2], row[1])
            };
            (id, text)
        })
        .collect()
}

struct CachedSearchData {
    rows: Arc<Vec<Vec<String>>>,
    bm25: Arc<BM25Index>,
    docs: Arc<Vec<(String, String)>>,
    symbol_embeddings: Arc<Vec<(String, Vec<f32>)>>,
}

/// Cheap health check consulted before trusting a warm search-cache hit.
/// The cache key is `embeddings.bin`'s mtime, which a daemon crash or a
/// dead-holder WAL on the *graph* database does not touch -- without this,
/// a warm cache could silently keep serving stale data forever and never
/// execute `open_read_only_or_degrade`, so no quarantine or recovery
/// sentinel would ever get created (R3.1.4 adversarial review finding).
/// Mirrors the same dead-holder-WAL and crash-loop signals
/// `GraphStore::open_read_only_or_degrade` checks on a miss, without
/// opening the database itself.
fn graph_needs_recovery(infigraph_dir: &std::path::Path) -> bool {
    if infigraph_core::recovery::crash_loop_detected(infigraph_dir).is_some() {
        return true;
    }
    let graph_path = infigraph_dir.join("graph");
    let lock_path = infigraph_core::graph::db_lock_path(&graph_path);
    infigraph_core::graph::unclean_shutdown_wal_holder(&graph_path, &lock_path).is_some()
}

fn remote_cache_key() -> SystemTime {
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

fn get_or_build_search_ctx(
    args: &Value,
) -> Result<(
    CachedSearchData,
    Option<infigraph_core::graph::DegradeReason>,
)> {
    let raw_path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
    let path = super::helpers::resolve_project_path(raw_path);
    let tg_root = PathBuf::from(&path).join(".infigraph");
    let canon = tg_root.canonicalize().unwrap_or_else(|_| tg_root.clone());

    let is_remote = infigraph_core::daemon::lifecycle::is_remote_backend();

    let mtime = if is_remote {
        remote_cache_key()
    } else {
        let emb_file = tg_root.join("embeddings.bin");
        std::fs::metadata(&emb_file)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    };

    {
        let guard = search_ctx_lock().lock().unwrap();
        if let Some(ctx) = guard.as_ref() {
            if ctx.db_path == canon
                && ctx.db_mtime == mtime
                && (is_remote || !graph_needs_recovery(&tg_root))
            {
                // A pure cache hit predates any crash this call could
                // detect (a live connection isn't reopened) -- R3.1.4b's
                // degrade banner only fires on an actual miss-path open
                // below. A fresh crash surfaces on the next real miss
                // (e.g. once the auto-rebuild completes and embeddings.bin's
                // mtime changes) -- or, now, immediately: `graph_needs_recovery`
                // above forces a miss the moment a dead-holder WAL or a
                // tripped crash-loop breaker shows up, even mid-cache-hit.
                return Ok((
                    CachedSearchData {
                        rows: Arc::clone(&ctx.rows),
                        bm25: Arc::clone(&ctx.bm25_unfiltered),
                        docs: Arc::clone(&ctx.docs_unfiltered),
                        symbol_embeddings: Arc::clone(&ctx.symbol_embeddings),
                    },
                    None,
                ));
            }
        }
    }

    let ((rows, symbol_embeddings), degrade_reason) = if is_remote {
        (get_search_data_remote(&path)?, None)
    } else {
        get_search_data_local(args, &path)?
    };

    let docs = build_docs_from_rows(&rows);
    let bm25 = BM25Index::build(docs.clone());

    let rows = Arc::new(rows);
    let bm25 = Arc::new(bm25);
    let docs = Arc::new(docs);
    let symbol_embeddings = Arc::new(symbol_embeddings);

    let data = CachedSearchData {
        rows: Arc::clone(&rows),
        bm25: Arc::clone(&bm25),
        docs: Arc::clone(&docs),
        symbol_embeddings: Arc::clone(&symbol_embeddings),
    };

    // A degraded read is served from a demoted snapshot, not the live
    // graph -- caching it under the live graph's mtime key would keep
    // serving stale-snapshot data even after the background rebuild
    // finishes and a fresh live graph exists. Cache only the healthy path.
    if degrade_reason.is_none() {
        let mut guard = search_ctx_lock().lock().unwrap();
        *guard = Some(SearchContext {
            db_path: canon,
            db_mtime: mtime,
            rows,
            bm25_unfiltered: bm25,
            docs_unfiltered: docs,
            symbol_embeddings,
        });
    }

    Ok((data, degrade_reason))
}

fn get_search_data_local(
    args: &Value,
    path: &str,
) -> Result<(SearchData, Option<infigraph_core::graph::DegradeReason>)> {
    let (prism, degrade_reason) = super::helpers::open_prism_read_only_or_degrade(args)?;
    let backend = prism.backend().context("not initialized")?;
    let rows = backend.get_symbols_for_search()?;

    let docs = build_docs_from_rows(&rows);
    let embedder = embed::best_embedder();
    let emb_path = PathBuf::from(path)
        .join(".infigraph")
        .join("embeddings.bin");
    let embeddings_map: HashMap<String, Vec<f32>> = if emb_path.exists() {
        // R3.3.3: warn (don't block or auto-rebuild) when embeddings.bin
        // was built from a graph generation older than the live one --
        // search still serves what's on disk, but the operator gets a
        // signal that a reindex would refresh semantic ranking.
        if let (Ok(current_gen), Some(recorded_gen)) = (
            backend.current_ast_generation(),
            embed::read_generation_marker(&emb_path),
        ) {
            if current_gen > 0 && recorded_gen < current_gen {
                eprintln!(
                    "[search] warn: embeddings.bin was built from graph generation \
                     {recorded_gen}, but the graph is now at generation {current_gen} -- \
                     semantic ranking may be stale until the next reindex rebuilds it"
                );
            }
        }
        embed::load_embeddings_cached(&emb_path)?
            .into_iter()
            .collect()
    } else {
        docs.iter()
            .map(|(id, text)| (id.clone(), embedder.embed(text).unwrap_or_default()))
            .collect()
    };

    let symbol_embeddings: Vec<(String, Vec<f32>)> = docs
        .iter()
        .filter_map(|(id, text)| {
            embeddings_map
                .get(id)
                .cloned()
                .or_else(|| embedder.embed(text).ok())
                .map(|emb| (id.clone(), emb))
        })
        .collect();

    Ok(((rows, symbol_embeddings), degrade_reason))
}

#[allow(unused)]
fn get_search_data_remote(path: &str) -> Result<SearchData> {
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

pub fn tool_search(args: &Value) -> Result<String> {
    let scope = args.get("scope").and_then(|s| s.as_str()).unwrap_or("all");

    if scope == "docs" {
        return tool_search_docs(args);
    }

    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .context("missing 'query'")?;
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(20) as usize;
    let kind_filter = args
        .get("kind")
        .and_then(|v| v.as_str())
        .map(str::to_lowercase);
    let file_pattern = args.get("file_pattern").and_then(|f| f.as_str());
    let path = &super::helpers::resolve_project_path(
        args.get("path").and_then(|p| p.as_str()).unwrap_or("."),
    );
    let use_regex = args.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);

    let (ctx, degrade_reason) = get_or_build_search_ctx(args)?;

    let rows = ctx.rows;
    if rows.is_empty() {
        return Ok("No symbols indexed. Run index_project first.".to_string());
    }

    let filtered_rows: Vec<&Vec<String>> = match &kind_filter {
        Some(k) => rows
            .iter()
            .filter(|row| row[2].to_lowercase() == *k)
            .collect(),
        None => rows.iter().collect(),
    };

    if filtered_rows.is_empty() {
        return Ok(format!(
            "No symbols found with kind '{}'.",
            kind_filter.unwrap_or_default()
        ));
    }

    let filtered_bm25;
    let filtered_docs;
    let (bm25_ref, docs_ref): (&BM25Index, &[(String, String)]) = if kind_filter.is_some() {
        filtered_docs = build_docs_from_rows(
            &filtered_rows
                .iter()
                .map(|r| (*r).clone())
                .collect::<Vec<_>>(),
        );
        filtered_bm25 = BM25Index::build(filtered_docs.clone());
        (&filtered_bm25, &filtered_docs)
    } else {
        (&ctx.bm25, &ctx.docs)
    };

    let embedder = embed::best_embedder();

    let filtered_symbol_embeddings;
    let symbol_embeddings_ref: &[(String, Vec<f32>)] = if kind_filter.is_some() {
        let ids: std::collections::HashSet<&str> =
            docs_ref.iter().map(|(id, _)| id.as_str()).collect();
        filtered_symbol_embeddings = ctx
            .symbol_embeddings
            .iter()
            .filter(|(id, _)| ids.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        &filtered_symbol_embeddings
    } else {
        &ctx.symbol_embeddings
    };

    // Compute raw scores once, blend with both alphas
    let oversample = limit * 2;
    let is_remote = infigraph_core::daemon::lifecycle::is_remote_backend();
    let tg_dir = PathBuf::from(path).join(".infigraph");
    let hnsw_path = tg_dir.join("hnsw_index.usearch");
    let emb_path = tg_dir.join("embeddings.bin");
    let (hnsw_opt, emb_opt): (Option<&std::path::Path>, Option<&std::path::Path>) = if is_remote {
        (None, None)
    } else {
        (Some(hnsw_path.as_path()), Some(emb_path.as_path()))
    };
    let raw = infigraph_core::search::compute_raw_scores(
        query,
        bm25_ref,
        embedder.as_ref(),
        symbol_embeddings_ref,
        oversample,
        hnsw_opt,
        emb_opt,
    )?;

    let keyword_results = infigraph_core::search::combine_scores(&raw, 0.3, limit);
    let semantic_results = infigraph_core::search::combine_scores(&raw, 0.85, limit);

    // Merge: keep max score per symbol_id
    let mut merged: std::collections::HashMap<String, infigraph_core::search::SearchResult> =
        std::collections::HashMap::new();
    for r in keyword_results.into_iter().chain(semantic_results) {
        merged
            .entry(r.symbol_id.clone())
            .and_modify(|existing| {
                if r.score > existing.score {
                    *existing = r.clone();
                }
            })
            .or_insert(r);
    }

    // Run grep search
    let root = PathBuf::from(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path));
    let grep_pattern = if use_regex {
        args.get("pattern")
            .and_then(|p| p.as_str())
            .unwrap_or(query)
            .to_string()
    } else {
        query
            .chars()
            .flat_map(|c| {
                if r"\.+*?()|[]{}^$-".contains(c) {
                    vec!['\\', c]
                } else {
                    vec![c]
                }
            })
            .collect::<String>()
    };
    let grep_results =
        infigraph_core::search::grep_search(&root, &grep_pattern, file_pattern, TEXT_MATCH_CAP)
            .unwrap_or_default();
    let text_hits = attribute_text_hits(&rows, &grep_results);

    // Sort merged results
    let mut symbol_results: Vec<infigraph_core::search::SearchResult> =
        merged.into_values().collect();
    symbol_results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Auto-escalate if results are weak
    let top_score = symbol_results.first().map(|r| r.score).unwrap_or(0.0);
    if (top_score < 0.4 || symbol_results.len() < 3) && limit < 100 {
        let esc_limit = (limit * 3).min(100);
        let esc_oversample = esc_limit * 2;
        let raw2 = infigraph_core::search::compute_raw_scores(
            query,
            bm25_ref,
            embedder.as_ref(),
            symbol_embeddings_ref,
            esc_oversample,
            hnsw_opt,
            emb_opt,
        )?;
        let kw2 = infigraph_core::search::combine_scores(&raw2, 0.3, esc_limit);
        let sem2 = infigraph_core::search::combine_scores(&raw2, 0.85, esc_limit);

        let mut esc_merged: std::collections::HashMap<
            String,
            infigraph_core::search::SearchResult,
        > = symbol_results
            .into_iter()
            .map(|r| (r.symbol_id.clone(), r))
            .collect();
        for r in kw2.into_iter().chain(sem2) {
            esc_merged
                .entry(r.symbol_id.clone())
                .and_modify(|existing| {
                    if r.score > existing.score {
                        *existing = r.clone();
                    }
                })
                .or_insert(r);
        }
        symbol_results = esc_merged.into_values().collect();
        symbol_results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let symbol_results = rank_with_text_hits(symbol_results, &text_hits, &rows, limit, use_regex);

    // Build row lookup
    let row_map: std::collections::HashMap<&str, &Vec<String>> =
        rows.iter().map(|row| (row[0].as_str(), row)).collect();

    // Format output
    let text_count = if grep_results.len() >= TEXT_MATCH_CAP {
        format!("{TEXT_MATCH_CAP}+")
    } else {
        text_hits.len().to_string()
    };
    let mut out = format!(
        "Search: '{}' ({} symbol results, {} text matches)\n\n",
        query,
        symbol_results.len(),
        text_count
    );

    for r in &symbol_results {
        if let Some(row) = row_map.get(r.symbol_id.as_str()) {
            let lines = match (
                row.get(5).filter(|s| !s.is_empty()),
                row.get(6).filter(|s| !s.is_empty()),
            ) {
                (Some(s), Some(e)) => format!(":L{}-{}", s, e),
                (Some(s), None) => format!(":L{}", s),
                _ => String::new(),
            };
            out.push_str(&format!(
                "{:.3}  {} {} ({}{})  id={}\n",
                r.score, row[2], row[1], row[3], lines, r.symbol_id
            ));
            if let Some(doc) = row.get(4).filter(|s| !s.is_empty()) {
                let preview: String = doc.chars().take(120).collect();
                out.push_str(&format!("       \"{}\"\n", preview));
            }
        }
    }

    out.push_str(&render_text_matches(
        &text_hits,
        &rows,
        text_matches_shown(limit, use_regex),
    ));

    // scope="all": append document results
    if scope == "all" {
        if let Ok(doc_idx) = open_doc_index(args) {
            if let Some(doc_store) = doc_idx.store() {
                let doc_limit = (limit / 2).max(5);
                if let Ok(doc_results) = infigraph_docs::search::hybrid_doc_search(
                    query, doc_store, &root, doc_limit, 0.5,
                ) {
                    if !doc_results.is_empty() {
                        out.push_str("\n---\nDocument matches:\n");
                        for dr in &doc_results {
                            let heading = dr.heading.as_deref().unwrap_or("");
                            out.push_str(&format!(
                                "  [{}] {} (score: {:.2})\n",
                                dr.doc_file, heading, dr.score
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
        out.push_str(&format!("No results for '{}'", query));
    }

    if !super::watch::watcher_running(&root) {
        if let Some(msg) = super::watch::auto_start_watch_opportunistic(path) {
            out.push_str(&format!("\n✓ Auto-started watcher: {msg}"));
        }
        super::docs::auto_start_doc_watch_opportunistic(path);
    }

    // R3.3.6 (#26): surface index staleness WITH the results instead of
    // leaving the caller to trust them blindly. The R3.3.5 dirty set is an
    // O(1) read of exactly the files whose edits were observed but not yet
    // drained into the graph -- far cheaper and more precise than the
    // filesystem-mtime walk R3.3.6 originally sketched (files with no
    // watcher observation are covered by the auto-start above: the next
    // edits get marked). Prepended so a truncating client still sees it.
    if let Some(banner) = staleness_banner(&root) {
        out.insert_str(0, &banner);
    }

    // R3.1.4b: more severe than the staleness banner above (serving
    // historical data from a demoted snapshot, not just a few files
    // lagging the live index), so it goes first.
    if let Some(ref reason) = degrade_reason {
        out.insert_str(0, &degrade_banner(reason));
    }

    Ok(out)
}

/// Most lines one search collects from its text leg. Far above any display
/// `limit` on purpose: the header's count has to be honest, and hits are
/// ranked before they are cut for display, not cut first in whatever order
/// the directory walk found them (#167).
const TEXT_MATCH_CAP: usize = 500;

/// A text-search hit and the symbol it falls inside, if any.
struct TextHit<'a> {
    hit: &'a infigraph_core::search::GrepMatch,
    symbol: Option<&'a str>,
}

/// Attribute each hit to the narrowest symbol containing it.
///
/// `Module` symbols are left out: every line of a file sits inside its
/// whole-file module, so attributing to one would make every hit belong to a
/// symbol nobody searched for -- which is how nearly every hit used to be
/// classed as "inside a symbol" and then dropped (#167). A line inside no
/// narrower symbol is a plain text match.
fn attribute_text_hits<'a>(
    rows: &'a [Vec<String>],
    hits: &'a [infigraph_core::search::GrepMatch],
) -> Vec<TextHit<'a>> {
    let intervals: Vec<(&str, usize, usize, &str)> = rows
        .iter()
        .filter(|row| !row[2].eq_ignore_ascii_case("module"))
        .filter_map(|row| {
            let start: usize = row.get(5)?.parse().ok()?;
            let end: usize = row.get(6)?.parse().ok()?;
            Some((row[3].as_str(), start, end, row[0].as_str()))
        })
        .collect();
    hits.iter()
        .map(|hit| TextHit {
            hit,
            symbol: find_containing_symbol(&intervals, &hit.file, hit.line_number),
        })
        .collect()
}

/// Order the symbol results, putting symbols that contain a text hit first,
/// then truncate to `limit`.
///
/// A literal the caller typed that actually occurs is stronger evidence than
/// any semantic neighbour, so its symbols lead -- and are added when they did
/// not rank at all (at score 1.0, since the match is exact). For a plain query
/// only while the literal is specific: more hits than `limit` means something
/// like `error`, and promoting those would evict every semantic result. Such
/// hits are still listed by [`render_text_matches`], just not ranked. A
/// `regex` search is the caller asking for the text itself, so it always
/// ranks its hits first.
fn rank_with_text_hits(
    mut results: Vec<infigraph_core::search::SearchResult>,
    hits: &[TextHit<'_>],
    rows: &[Vec<String>],
    limit: usize,
    regex: bool,
) -> Vec<infigraph_core::search::SearchResult> {
    let mut ranked = Vec::new();
    if regex || hits.len() <= limit {
        let mut seen = std::collections::HashSet::new();
        for id in hits.iter().filter_map(|h| h.symbol) {
            if !seen.insert(id) {
                continue;
            }
            if let Some(pos) = results.iter().position(|r| r.symbol_id == id) {
                ranked.push(results.remove(pos));
            } else if let Some(row) = rows.iter().find(|row| row[0] == id) {
                ranked.push(infigraph_core::search::SearchResult {
                    symbol_id: row[0].clone(),
                    name: row[1].clone(),
                    kind: row[2].clone(),
                    file: row[3].clone(),
                    score: 1.0,
                    bm25_score: 0.0,
                    vector_score: 0.0,
                    docstring: row.get(4).filter(|d| !d.is_empty()).cloned(),
                });
            }
        }
    }
    ranked.extend(results);
    ranked.truncate(limit);
    ranked
}

/// How many text matches a search lists: every one it collected for a `regex`
/// search, the first `limit` otherwise.
///
/// `regex: true` is how a caller enumerates -- every call site before a
/// rename -- and cutting that at `limit` (which also sizes the symbol list)
/// would send them to `search_code` for the rest. A plain query wants the
/// best few lines alongside its symbols, not a wall of them.
fn text_matches_shown(limit: usize, regex: bool) -> usize {
    if regex {
        TEXT_MATCH_CAP
    } else {
        limit
    }
}

/// The `Text matches:` section: every hit up to `limit`, each naming the symbol
/// it falls in.
///
/// One section rather than a `grep:` line under each symbol: search output is
/// compressed at `Summary`, and `compress_search` keeps this section but drops
/// indented lines, so per-symbol lines vanished for exactly the callers who
/// most needed them.
fn render_text_matches(hits: &[TextHit<'_>], rows: &[Vec<String>], limit: usize) -> String {
    if hits.is_empty() {
        return String::new();
    }
    let names: std::collections::HashMap<&str, &str> = hits
        .iter()
        .filter_map(|h| h.symbol)
        .filter_map(|id| {
            let row = rows.iter().find(|row| row[0] == id)?;
            Some((id, row[1].as_str()))
        })
        .collect();
    let mut out = String::from("\n---\nText matches:\n");
    for h in hits.iter().take(limit) {
        out.push_str(&format!(
            "{}:{}: {}",
            h.hit.file,
            h.hit.line_number,
            h.hit.line_text.trim()
        ));
        if let Some(name) = h.symbol.and_then(|id| names.get(id)) {
            out.push_str(&format!("  (in {name})"));
        }
        out.push('\n');
    }
    if hits.len() > limit {
        out.push_str(&format!(
            "  ... ({} more text matches)\n",
            hits.len() - limit
        ));
    }
    out
}

/// One-line warning when the project's persistent dirty set (R3.3.5) says
/// edits are awaiting reindex; `None` when everything known is drained.
fn staleness_banner(root: &std::path::Path) -> Option<String> {
    let pending = infigraph_core::dirty::pending_dirty(&root.join(".infigraph")).ok()?;
    if pending.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = pending.iter().map(String::as_str).collect();
    names.sort_unstable();
    let sample = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
    let more = if names.len() > 3 { ", ..." } else { "" };

    // #153: distinguish "behind" from "blocked". Once the runaway-growth
    // breaker latches, every write path refuses, so the watcher will never
    // drain these -- promising that it will is the line that let this repo
    // sit wedged for hours with no other user-visible symptom. Reuses
    // doctor's own check rather than re-deriving the condition here.
    if infigraph_core::doctor::check_one_growth_breaker(root)
        .is_some_and(|c| c.status == infigraph_core::doctor::CheckStatus::Fail)
    {
        return Some(format!(
            "⚠ indexing is BLOCKED -- {} file(s) changed since the last index ({sample}{more}), \
             and the runaway-growth breaker is refusing every write, so the watcher cannot \
             drain them. Run `infigraph rebuild` to rebuild and unblock; `infigraph \
             doctor` has the details.\n\n",
            names.len()
        ));
    }

    Some(format!(
        "⚠ results may be stale -- {} file(s) changed since the last index ({sample}{more}); \
         the watcher drains these shortly, or run index_project to force it\n\n",
        names.len()
    ))
}

pub fn tool_search_symbols(args: &Value) -> Result<String> {
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .context("missing 'query'")?;
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
    let path = &super::helpers::resolve_project_path(
        args.get("path").and_then(|p| p.as_str()).unwrap_or("."),
    );

    let (ctx, _degrade_reason) = get_or_build_search_ctx(args)?;
    let rows = ctx.rows;

    if rows.is_empty() {
        return Ok("No symbols indexed. Run index_project first.".to_string());
    }

    let embedder = embed::best_embedder();

    let (hnsw_path, emb_path) = if infigraph_core::daemon::lifecycle::is_remote_backend() {
        (None, None)
    } else {
        let tg_dir = PathBuf::from(path).join(".infigraph");
        (
            Some(tg_dir.join("hnsw_index.usearch")),
            Some(tg_dir.join("embeddings.bin")),
        )
    };
    let results = infigraph_core::search::hybrid_search(
        query,
        &ctx.bm25,
        embedder.as_ref(),
        &ctx.symbol_embeddings,
        limit,
        0.3,
        hnsw_path.as_deref(),
        emb_path.as_deref(),
    )?;

    let mut out = String::new();
    for r in &results {
        if let Some(row) = rows.iter().find(|row| row[0] == r.symbol_id) {
            let lines = match (
                row.get(5).filter(|s| !s.is_empty()),
                row.get(6).filter(|s| !s.is_empty()),
            ) {
                (Some(s), Some(e)) => format!(":L{}-{}", s, e),
                (Some(s), None) => format!(":L{}", s),
                _ => String::new(),
            };
            out.push_str(&format!(
                "{:.3}  {} {} ({}{})  id={}\n",
                r.score, row[2], row[1], row[3], lines, r.symbol_id
            ));
        }
    }
    if out.is_empty() {
        out = format!("No results for '{}'", query);
    }
    Ok(out)
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

pub fn tool_semantic_search(args: &Value) -> Result<String> {
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .context("missing 'query'")?;
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
    let kind_filter = args
        .get("kind")
        .and_then(|v| v.as_str())
        .map(str::to_lowercase);
    let path = &super::helpers::resolve_project_path(
        args.get("path").and_then(|p| p.as_str()).unwrap_or("."),
    );

    let (ctx, _degrade_reason) = get_or_build_search_ctx(args)?;
    let rows = ctx.rows;

    if rows.is_empty() {
        return Ok("No symbols indexed. Run index_project first.".to_string());
    }

    let filtered_rows: Vec<&Vec<String>> = match &kind_filter {
        Some(k) => rows
            .iter()
            .filter(|row| row[2].to_lowercase() == *k)
            .collect(),
        None => rows.iter().collect(),
    };

    if filtered_rows.is_empty() {
        return Ok(format!(
            "No symbols found with kind '{}'.",
            kind_filter.unwrap_or_default()
        ));
    }

    let filtered_bm25_sem;
    let filtered_docs_sem;
    let (bm25_ref_sem, docs_ref_sem): (&BM25Index, &[(String, String)]) = if kind_filter.is_some() {
        filtered_docs_sem = build_docs_from_rows(
            &filtered_rows
                .iter()
                .map(|r| (*r).clone())
                .collect::<Vec<_>>(),
        );
        filtered_bm25_sem = BM25Index::build(filtered_docs_sem.clone());
        (&filtered_bm25_sem, &filtered_docs_sem)
    } else {
        (&ctx.bm25, &ctx.docs)
    };

    let embedder = embed::best_embedder();

    let filtered_sym_emb_sem;
    let sym_emb_ref_sem: &[(String, Vec<f32>)] = if kind_filter.is_some() {
        let ids: std::collections::HashSet<&str> =
            docs_ref_sem.iter().map(|(id, _)| id.as_str()).collect();
        filtered_sym_emb_sem = ctx
            .symbol_embeddings
            .iter()
            .filter(|(id, _)| ids.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        &filtered_sym_emb_sem
    } else {
        &ctx.symbol_embeddings
    };

    let tg_dir = PathBuf::from(path).join(".infigraph");
    let hnsw_path = tg_dir.join("hnsw_index.usearch");
    let emb_path = tg_dir.join("embeddings.bin");
    let (hnsw_opt, emb_opt): (Option<&std::path::Path>, Option<&std::path::Path>) =
        if infigraph_core::daemon::lifecycle::is_remote_backend() {
            (None, None)
        } else {
            (Some(hnsw_path.as_path()), Some(emb_path.as_path()))
        };
    let results = infigraph_core::search::hybrid_search(
        query,
        bm25_ref_sem,
        embedder.as_ref(),
        sym_emb_ref_sem,
        limit,
        0.85,
        hnsw_opt,
        emb_opt,
    )?;

    let row_map: HashMap<&str, &Vec<String>> = filtered_rows
        .iter()
        .map(|row| (row[0].as_str(), *row))
        .collect();

    let mut out = format!("Semantic search: '{}'\n\n", query);
    for r in &results {
        if let Some(row) = row_map.get(r.symbol_id.as_str()) {
            let line = row.get(5).map(|s| s.as_str()).unwrap_or("?");
            let doc = row
                .get(4)
                .filter(|s| !s.is_empty())
                .map(|s| format!("\n     {}", s.chars().take(120).collect::<String>()))
                .unwrap_or_default();
            out.push_str(&format!(
                "{:.3}  {} {} ({}:{}){}\n",
                r.score, row[2], row[1], row[3], line, doc
            ));
        }
    }
    if out.trim_end().ends_with('\'') {
        out.push_str("No results found.");
    }
    Ok(out)
}

#[cfg(test)]
mod staleness_banner_tests {
    use super::staleness_banner;

    /// #153 defect 2: a latched growth breaker means the watcher will never
    /// drain -- indexing is refused outright, not merely behind. Telling the
    /// caller "the watcher drains these shortly" is then actively wrong, and
    /// it was the only user-visible symptom while this repo sat wedged for
    /// hours on 2026-09-08.
    #[test]
    fn a_latched_growth_breaker_says_blocked_not_merely_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let ig = tmp.path().join(".infigraph");
        std::fs::create_dir_all(&ig).unwrap();
        infigraph_core::dirty::mark_dirty(&ig, &["a.py".to_string()]).unwrap();

        // 20MB graph against a 1MB baseline: 20x, past the 10x default.
        std::fs::write(ig.join("graph"), vec![0u8; 20_000_000]).unwrap();
        std::fs::write(
            ig.join("graph.health.json"),
            r#"{"healthy_size_bytes": 1000000}"#,
        )
        .unwrap();

        let banner = staleness_banner(tmp.path()).expect("dirty files must still yield a banner");
        assert!(
            banner.to_lowercase().contains("blocked"),
            "must say indexing is blocked, not just stale: {banner}"
        );
        assert!(
            banner.contains("infigraph rebuild"),
            "must name the remedy that unblocks it: {banner}"
        );
        assert!(
            !banner.contains("drains these shortly"),
            "must not promise a drain that cannot happen: {banner}"
        );
    }

    #[test]
    fn empty_or_absent_dirty_set_yields_no_banner() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(staleness_banner(tmp.path()).is_none());
    }

    #[test]
    fn pending_dirty_files_yield_a_banner_naming_count_and_sample() {
        let tmp = tempfile::tempdir().unwrap();
        let ig = tmp.path().join(".infigraph");
        // `mark_dirty` returns Ok WITHOUT recording anything when
        // `.infigraph/` is absent -- deliberately, so a watcher's last events
        // after a project is deleted cannot resurrect the root (#136). These
        // tests are about the banner, not that guard (which has its own test,
        // `dirty::mark_dirty_does_not_create_a_missing_infigraph_dir`), so the
        // directory has to exist or the mark below is a silent no-op and the
        // banner has nothing to report.
        std::fs::create_dir_all(&ig).unwrap();
        infigraph_core::dirty::mark_dirty(
            &ig,
            &[
                "a.py".to_string(),
                "b.py".to_string(),
                "c.py".to_string(),
                "d.py".to_string(),
            ],
        )
        .unwrap();

        let banner = staleness_banner(tmp.path()).expect("4 pending files must warn");
        assert!(banner.contains("4 file(s)"), "{banner}");
        assert!(
            banner.contains("a.py"),
            "sorted sample must start at a.py: {banner}"
        );
        assert!(
            banner.contains("..."),
            "overflow marker for >3 files: {banner}"
        );
        assert!(
            banner.starts_with('\u{26a0}'),
            "must be a visible warning: {banner}"
        );
    }

    #[test]
    fn banner_clears_once_the_dirty_set_is_drained() {
        let tmp = tempfile::tempdir().unwrap();
        let ig = tmp.path().join(".infigraph");
        // Must exist before marking -- see the note in the test above.
        std::fs::create_dir_all(&ig).unwrap();
        infigraph_core::dirty::mark_dirty(&ig, &["a.py".to_string()]).unwrap();
        assert!(staleness_banner(tmp.path()).is_some());
        infigraph_core::dirty::clear_dirty(&ig, &["a.py".to_string()]).unwrap();
        assert!(
            staleness_banner(tmp.path()).is_none(),
            "a drained dirty set must stop warning"
        );
    }
}

/// #167: `search`'s text matches were computed and then almost always
/// discarded -- a hit counted only if it fell outside every symbol (never,
/// given whole-file `Module` symbols), and a hit inside a symbol showed only
/// if that symbol had already ranked semantically.
#[cfg(test)]
mod text_match_tests {
    use super::{
        attribute_text_hits, rank_with_text_hits, render_text_matches, text_matches_shown, TextHit,
        TEXT_MATCH_CAP,
    };
    use infigraph_core::search::{GrepMatch, SearchResult};

    /// A row as `get_or_build_search_ctx` produces it:
    /// `[id, name, kind, file, docstring, start_line, end_line]`.
    fn row(id: &str, kind: &str, file: &str, start: usize, end: usize) -> Vec<String> {
        let name = id.rsplit("::").next().unwrap();
        vec![
            id.into(),
            name.into(),
            kind.into(),
            file.into(),
            String::new(),
            start.to_string(),
            end.to_string(),
        ]
    }

    fn hit(file: &str, line: usize, text: &str) -> GrepMatch {
        GrepMatch {
            file: file.into(),
            line_number: line,
            line_text: text.into(),
        }
    }

    fn result(id: &str, score: f32) -> SearchResult {
        SearchResult {
            symbol_id: id.into(),
            name: id.into(),
            kind: "Function".into(),
            file: "x.rs".into(),
            score,
            bm25_score: 0.0,
            vector_score: 0.0,
            docstring: None,
        }
    }

    fn ids(results: &[SearchResult]) -> Vec<&str> {
        results.iter().map(|r| r.symbol_id.as_str()).collect()
    }

    /// Every line of a file sits inside its `Module` symbol, so the module
    /// can never be the answer -- the narrowest symbol around the line is.
    #[test]
    fn a_hit_belongs_to_the_innermost_symbol_not_the_whole_file_module() {
        let rows = vec![
            row("a.rs::crate", "Module", "a.rs", 0, 300),
            row("a.rs::Outer", "Class", "a.rs", 10, 200),
            row("a.rs::Outer::inner", "Method", "a.rs", 50, 60),
        ];
        let hits = vec![hit("a.rs", 55, "the literal")];
        let attributed = attribute_text_hits(&rows, &hits);
        assert_eq!(attributed[0].symbol, Some("a.rs::Outer::inner"));
    }

    #[test]
    fn a_hit_outside_every_narrower_symbol_is_a_plain_text_match() {
        let rows = vec![
            row("a.rs::crate", "Module", "a.rs", 0, 300),
            row("a.rs::f", "Function", "a.rs", 10, 20),
        ];
        let hits = vec![hit("a.rs", 3, "use the::literal;")];
        assert_eq!(attribute_text_hits(&rows, &hits)[0].symbol, None);
    }

    /// The reported case: the literal exists, but the function holding it did
    /// not rank semantically. It must now appear, ahead of the neighbours.
    #[test]
    fn a_specific_literal_pulls_its_unranked_symbol_into_the_results_first() {
        let rows = vec![
            row("d.rs::run_write_coordinator", "Function", "d.rs", 485, 1400),
            row("d.rs::COORDINATOR_TICK", "Variable", "d.rs", 31, 31),
        ];
        let hits = vec![hit("d.rs", 1134, "// Piggybacks on this loop's tick")];
        let semantic = vec![
            result("d.rs::COORDINATOR_TICK", 0.97),
            result("z.rs::other", 0.85),
        ];

        let ranked = rank_with_text_hits(
            semantic,
            &attribute_text_hits(&rows, &hits),
            &rows,
            5,
            false,
        );

        assert_eq!(
            ids(&ranked),
            [
                "d.rs::run_write_coordinator",
                "d.rs::COORDINATOR_TICK",
                "z.rs::other"
            ]
        );
    }

    #[test]
    fn a_symbol_already_ranked_is_moved_up_not_duplicated() {
        let rows = vec![row("a.rs::f", "Function", "a.rs", 1, 9)];
        let hits = vec![hit("a.rs", 4, "literal"), hit("a.rs", 5, "literal again")];
        let semantic = vec![result("b.rs::g", 0.9), result("a.rs::f", 0.6)];

        let ranked = rank_with_text_hits(
            semantic,
            &attribute_text_hits(&rows, &hits),
            &rows,
            5,
            false,
        );

        assert_eq!(ids(&ranked), ["a.rs::f", "b.rs::g"]);
    }

    /// A literal matching more lines than the caller asked for is not specific
    /// evidence (think `error`); promoting its symbols would evict every
    /// semantic result. Its lines are still listed, just not ranked.
    #[test]
    fn a_common_literal_leaves_the_semantic_order_alone() {
        let rows = vec![
            row("a.rs::f", "Function", "a.rs", 1, 9),
            row("a.rs::g", "Function", "a.rs", 10, 19),
        ];
        let hits = vec![
            hit("a.rs", 2, "error"),
            hit("a.rs", 11, "error"),
            hit("a.rs", 12, "error"),
        ];
        let semantic = vec![result("b.rs::best", 0.9), result("b.rs::next", 0.8)];

        let ranked = rank_with_text_hits(
            semantic,
            &attribute_text_hits(&rows, &hits),
            &rows,
            2,
            false,
        );

        assert_eq!(ids(&ranked), ["b.rs::best", "b.rs::next"]);
    }

    /// `regex: true` is the caller saying "I want the text", so the specificity
    /// guard above does not apply: a pattern matching 13 call sites must list
    /// their symbols, not semantic neighbours of the pattern's words.
    #[test]
    fn a_regex_search_ranks_its_hit_symbols_first_however_many_there_are() {
        let rows = vec![
            row("a.rs::f", "Function", "a.rs", 1, 9),
            row("a.rs::g", "Function", "a.rs", 10, 19),
        ];
        let hits = vec![
            hit("a.rs", 2, "x"),
            hit("a.rs", 11, "x"),
            hit("a.rs", 12, "x"),
        ];
        let semantic = vec![result("b.rs::best", 0.9), result("b.rs::next", 0.8)];

        let ranked =
            rank_with_text_hits(semantic, &attribute_text_hits(&rows, &hits), &rows, 2, true);

        assert_eq!(ids(&ranked), ["a.rs::f", "a.rs::g"]);
    }

    /// Search output is compressed at `Summary`, which strips indented lines
    /// under a symbol but keeps the `Text matches:` section -- so every hit,
    /// attributed or not, is rendered there, naming its symbol.
    #[test]
    fn every_hit_is_listed_in_the_text_matches_section_with_its_symbol() {
        let rows = vec![row("a.rs::f", "Function", "a.rs", 1, 9)];
        let hits = vec![hit("a.rs", 4, "  inside f  "), hit("b.rs", 7, "top level")];
        let attributed: Vec<TextHit> = attribute_text_hits(&rows, &hits);

        let section = render_text_matches(&attributed, &rows, 10);

        assert!(section.starts_with("\n---\nText matches:\n"), "{section}");
        assert!(section.contains("a.rs:4: inside f  (in f)"), "{section}");
        assert!(section.contains("b.rs:7: top level\n"), "{section}");
        assert!(!section.contains("grep:"), "{section}");
    }

    #[test]
    fn the_rendered_section_is_capped_at_the_limit_and_says_how_many_it_left_out() {
        let rows: Vec<Vec<String>> = Vec::new();
        let hits: Vec<GrepMatch> = (1..=5).map(|n| hit("a.rs", n, "x")).collect();
        let section = render_text_matches(&attribute_text_hits(&rows, &hits), &rows, 2);
        assert_eq!(section.matches("a.rs:").count(), 2, "{section}");
        assert!(section.contains("3 more"), "{section}");
    }

    /// `regex: true` is how a caller enumerates -- every call site before a
    /// rename -- so it lists every match the search collected, not the first
    /// `limit` (which also sizes the symbol list). That is what lets `search`
    /// stand in for `search_code` rather than send callers to a second tool.
    #[test]
    fn a_regex_search_lists_every_collected_match_and_a_plain_one_stops_at_the_limit() {
        assert_eq!(text_matches_shown(20, true), TEXT_MATCH_CAP);
        assert_eq!(text_matches_shown(20, false), 20);
    }
}
