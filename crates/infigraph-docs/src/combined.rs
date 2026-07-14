//! Combined document graph for repository groups.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use fs2::FileExt;
use infigraph_core::embed::{
    build_hnsw_index, invalidate_embeddings_cache, invalidate_hnsw_cache, load_embeddings,
    save_embeddings,
};
use infigraph_core::multi::combined::{prefix_edge_parquet, prefix_parquet_columns};
use infigraph_core::multi::Registry;

use crate::search::{hybrid_doc_search_in_dir, DocSearchResult};
use crate::store::DocStore;

static REFRESHING_GROUPS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CombinedDocStats {
    pub documents: usize,
    pub chunks: usize,
    pub links: usize,
    pub intra_repo_links: usize,
    pub cross_repo_links: usize,
    pub sources: usize,
    pub embeddings: usize,
}

struct PreparedRepo {
    documents: PathBuf,
    document_count: usize,
    chunks: PathBuf,
    chunk_count: usize,
    has_chunks: PathBuf,
    has_chunk_count: usize,
    links: PathBuf,
    link_count: usize,
    sources: PathBuf,
    source_count: usize,
    from_sources: PathBuf,
    from_source_count: usize,
}

/// Build (or replace) one physical document graph for every repository in a group.
pub fn build_combined_docs(registry: &Registry, group_name: &str) -> Result<CombinedDocStats> {
    let group = registry
        .groups
        .get(group_name)
        .with_context(|| format!("group '{}' not found", group_name))?;

    let group_root = combined_docs_root(group_name)?;
    let graph_dir = group_root.join(".infigraph");
    std::fs::create_dir_all(&graph_dir)?;
    let _lock_file = acquire_build_lock(&graph_dir)?;

    let tmp_dir = tempfile::Builder::new()
        .prefix("docs-build-")
        .tempdir_in(&graph_dir)
        .context("failed to create temp dir for combined docs")?;
    let tmp = tmp_dir.path();
    let artifacts = tmp.join("artifacts");
    std::fs::create_dir_all(&artifacts)?;
    let mut prepared = Vec::new();
    let mut repo_docs: HashMap<String, (PathBuf, HashSet<String>)> = HashMap::new();
    let repo_aliases = build_repo_aliases(registry, &group.repos);
    let mut known_chunk_ids = HashSet::new();
    let mut combined_embeddings = HashMap::new();
    let mut embedding_dimension = None;

    // DocStore intentionally serializes Kuzu access process-wide. Export each source
    // completely before opening the next source or the combined destination.
    for (repo_idx, repo_name) in group.repos.iter().enumerate() {
        let entry = registry
            .repos
            .get(repo_name)
            .with_context(|| format!("repo '{}' not in registry", repo_name))?;
        let source_path = entry.path.join(".infigraph").join("docs.kuzu");
        if !source_path.exists() {
            eprintln!(
                "  [combined-docs] skip {} — documents not indexed",
                repo_name
            );
            continue;
        }

        let store = DocStore::open(&source_path)?;
        let doc_ids: HashSet<String> = store.get_doc_hashes()?.into_keys().collect();
        let conn = store.connection()?;
        let prefix = format!("[{}]::", repo_name);
        let base = format!("repo_{repo_idx}");

        let documents = tmp.join(format!("{base}_documents.parquet"));
        let documents_prefixed = tmp.join(format!("{base}_documents_prefixed.parquet"));
        copy_to(
            &conn,
            "MATCH (d:Document) WHERE d.file IS NOT NULL AND d.file <> '' \
             RETURN d.id, d.title, d.file, d.format, d.content_hash, d.page_count, d.chunk_count",
            &documents,
            repo_name,
            "Document",
        )?;
        let document_count = prefix_parquet_columns(
            &documents,
            &documents_prefixed,
            &prefix,
            &[0, 2],
            None,
            &mut HashSet::new(),
            0,
        )?;

        let chunks = tmp.join(format!("{base}_chunks.parquet"));
        let chunks_prefixed = tmp.join(format!("{base}_chunks_prefixed.parquet"));
        copy_to(
            &conn,
            "MATCH (c:Chunk) RETURN c.id, c.doc_file, c.idx, c.heading, c.text, \
             c.start_offset, c.end_offset, c.page, c.content_hash",
            &chunks,
            repo_name,
            "Chunk",
        )?;
        let chunk_count = prefix_parquet_columns(
            &chunks,
            &chunks_prefixed,
            &prefix,
            &[0, 1],
            None,
            &mut known_chunk_ids,
            0,
        )?;

        let has_chunks = tmp.join(format!("{base}_has_chunks.parquet"));
        let has_chunks_prefixed = tmp.join(format!("{base}_has_chunks_prefixed.parquet"));
        copy_to(
            &conn,
            "MATCH (d:Document)-[:HAS_CHUNK]->(c:Chunk) RETURN d.id, c.id",
            &has_chunks,
            repo_name,
            "HAS_CHUNK",
        )?;
        let has_chunk_count =
            prefix_edge_parquet(&has_chunks, &has_chunks_prefixed, &prefix, &[0, 1])?;

        let links = tmp.join(format!("{base}_links.parquet"));
        let links_prefixed = tmp.join(format!("{base}_links_prefixed.parquet"));
        copy_to(
            &conn,
            "MATCH (a:Document)-[r:LINKS_TO]->(b:Document) \
             WHERE a.file IS NOT NULL AND b.file IS NOT NULL \
             RETURN a.id, b.id, r.url, r.link_type",
            &links,
            repo_name,
            "LINKS_TO",
        )?;
        let link_count = prefix_edge_parquet(&links, &links_prefixed, &prefix, &[0, 1])?;

        let sources = tmp.join(format!("{base}_sources.parquet"));
        let sources_prefixed = tmp.join(format!("{base}_sources_prefixed.parquet"));
        copy_to(
            &conn,
            "MATCH (s:Source) RETURN s.id, s.source_type, s.base_url, s.space_key, s.last_synced",
            &sources,
            repo_name,
            "Source",
        )?;
        let source_count = prefix_parquet_columns(
            &sources,
            &sources_prefixed,
            &prefix,
            &[0],
            None,
            &mut HashSet::new(),
            0,
        )?;

        let from_sources = tmp.join(format!("{base}_from_sources.parquet"));
        let from_sources_prefixed = tmp.join(format!("{base}_from_sources_prefixed.parquet"));
        copy_to(
            &conn,
            "MATCH (d:Document)-[:FROM_SOURCE]->(s:Source) RETURN d.id, s.id",
            &from_sources,
            repo_name,
            "FROM_SOURCE",
        )?;
        let from_source_count =
            prefix_edge_parquet(&from_sources, &from_sources_prefixed, &prefix, &[0, 1])?;

        drop(conn);
        drop(store);

        let embeddings_path = entry.path.join(".infigraph").join("docs_embeddings.bin");
        let repo_embeddings = if embeddings_path.exists() {
            load_embeddings(&embeddings_path).with_context(|| {
                format!("invalid document embeddings for repository '{repo_name}'")
            })?
        } else {
            Vec::new()
        };
        for (id, vector) in repo_embeddings {
            if let Some(expected) = embedding_dimension {
                anyhow::ensure!(
                    vector.len() == expected,
                    "document embedding dimension mismatch in repository '{}': expected {}, got {}",
                    repo_name,
                    expected,
                    vector.len()
                );
            } else {
                embedding_dimension = Some(vector.len());
            }
            let combined_id = format!("{prefix}{id}");
            if known_chunk_ids.contains(&combined_id) {
                combined_embeddings.insert(combined_id, vector);
            }
        }

        repo_docs.insert(repo_name.clone(), (entry.path.clone(), doc_ids));
        prepared.push(PreparedRepo {
            documents: documents_prefixed,
            document_count,
            chunks: chunks_prefixed,
            chunk_count,
            has_chunks: has_chunks_prefixed,
            has_chunk_count,
            links: links_prefixed,
            link_count,
            sources: sources_prefixed,
            source_count,
            from_sources: from_sources_prefixed,
            from_source_count,
        });
    }

    let combined_path = artifacts.join("docs.kuzu");
    let combined_store = DocStore::open(&combined_path)?;
    let conn = combined_store.connection()?;
    let mut documents = 0;
    let mut chunks = 0;
    let mut intra_repo_links = 0;
    let mut sources = 0;

    for repo in &prepared {
        if repo.document_count > 0 {
            copy_from(
                &conn,
                "Document (id, title, file, format, content_hash, page_count, chunk_count)",
                &repo.documents,
            )?;
            documents += repo.document_count;
        }
        if repo.chunk_count > 0 {
            copy_from(
                &conn,
                "Chunk (id, doc_file, idx, heading, text, start_offset, end_offset, page, content_hash)",
                &repo.chunks,
            )?;
            chunks += repo.chunk_count;
        }
        if repo.source_count > 0 {
            copy_from(
                &conn,
                "Source (id, source_type, base_url, space_key, last_synced)",
                &repo.sources,
            )?;
            sources += repo.source_count;
        }
        if repo.has_chunk_count > 0 {
            copy_from(&conn, "HAS_CHUNK", &repo.has_chunks)?;
        }
        if repo.link_count > 0 {
            copy_from(&conn, "LINKS_TO", &repo.links)?;
            intra_repo_links += repo.link_count;
        }
        if repo.from_source_count > 0 {
            copy_from(&conn, "FROM_SOURCE", &repo.from_sources)?;
        }
    }
    drop(conn);

    let cross_repo_links = link_cross_repo_docs(&combined_store, &repo_docs, &repo_aliases);

    let mut embeddings: Vec<(String, Vec<f32>)> = combined_embeddings.into_iter().collect();
    embeddings.sort_by(|a, b| a.0.cmp(&b.0));
    let embeddings_path = artifacts.join("docs_embeddings.bin");
    save_embeddings(&embeddings_path, &embeddings)?;
    invalidate_embeddings_cache();

    if embeddings.len() >= combined_hnsw_threshold() {
        let hnsw_path = artifacts.join("docs_hnsw_index.usearch");
        invalidate_hnsw_cache();
        if let Err(error) = build_hnsw_index(&embeddings, &hnsw_path, &embeddings_path) {
            eprintln!(
                "warning: combined doc HNSW build failed ({error}), vector search will use brute-force"
            );
        }
    }

    drop(combined_store);
    publish_combined_docs(&graph_dir, &artifacts)?;

    Ok(CombinedDocStats {
        documents,
        chunks,
        links: intra_repo_links + cross_repo_links,
        intra_repo_links,
        cross_repo_links,
        sources,
        embeddings: embeddings.len(),
    })
}

fn combined_hnsw_threshold() -> usize {
    std::env::var("INFIGRAPH_DOC_HNSW_THRESHOLD")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200_000)
}

fn acquire_build_lock(graph_dir: &Path) -> Result<std::fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(graph_dir.join("docs-build.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn publish_combined_docs(graph_dir: &Path, staged: &Path) -> Result<PathBuf> {
    let generations = graph_dir.join("docs-generations");
    std::fs::create_dir_all(&generations)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let generation = generations.join(format!("gen-{nanos:020}-{}", std::process::id()));
    std::fs::rename(staged, &generation)?;

    let mut existing: Vec<_> = std::fs::read_dir(&generations)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .collect();
    existing.sort_by_key(|entry| entry.file_name());
    for stale in existing.into_iter().rev().skip(2) {
        let _ = std::fs::remove_dir_all(stale.path());
    }

    invalidate_embeddings_cache();
    invalidate_hnsw_cache();
    Ok(generation)
}

pub fn combined_docs_root(group_name: &str) -> Result<PathBuf> {
    infigraph_core::multi::combined::combined_graph_path(group_name)?
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .context("combined group path has no parent")
}

pub fn combined_docs_path(group_name: &str) -> Result<PathBuf> {
    Ok(combined_docs_artifact_dir(group_name)?.join("docs.kuzu"))
}

fn combined_docs_artifact_dir(group_name: &str) -> Result<PathBuf> {
    let graph_dir = combined_docs_root(group_name)?.join(".infigraph");
    let generations = graph_dir.join("docs-generations");
    if generations.exists() {
        let mut dirs: Vec<_> = std::fs::read_dir(&generations)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .collect();
        dirs.sort_by_key(|entry| entry.file_name());
        if let Some(active) = dirs.pop() {
            return Ok(active.path());
        }
    }
    Ok(graph_dir)
}

pub fn has_combined_docs(group_name: &str) -> bool {
    combined_docs_path(group_name)
        .map(|path| path.exists())
        .unwrap_or(false)
}

pub fn open_combined_docs(group_name: &str) -> Result<DocStore> {
    let path = combined_docs_path(group_name)?;
    if !path.exists() {
        anyhow::bail!(
            "Combined document store not found for group '{}'. Run group_build first.",
            group_name
        );
    }
    DocStore::open(&path)
}

pub fn combined_doc_search(
    group_name: &str,
    query: &str,
    limit: usize,
    alpha: f32,
) -> Result<Vec<DocSearchResult>> {
    let artifact_dir = combined_docs_artifact_dir(group_name)?;
    let store_path = artifact_dir.join("docs.kuzu");
    match DocStore::open(&store_path)
        .and_then(|store| hybrid_doc_search_in_dir(query, &store, &artifact_dir, limit, alpha))
    {
        Ok(results) => Ok(results),
        Err(first_err) => {
            eprintln!(
                "[combined-docs] search failed for group '{group_name}' ({first_err}), \
                 wiping corrupt generation and scheduling rebuild..."
            );
            wipe_active_generation(group_name);
            schedule_background_rebuild(group_name);
            Err(first_err.context(format!(
                "combined doc search failed for group '{group_name}'; corrupt generation wiped, rebuild scheduled"
            )))
        }
    }
}

pub fn combined_doc_query(group_name: &str, cypher: &str) -> Result<Vec<Vec<String>>> {
    let result = open_combined_docs(group_name).and_then(|store| {
        let conn = store.connection()?;
        let result = conn
            .query(cypher)
            .map_err(|error| anyhow::anyhow!("combined document query failed: {error}"))?;
        Ok(result
            .map(|row| row.iter().map(ToString::to_string).collect())
            .collect())
    });
    match result {
        Ok(rows) => Ok(rows),
        Err(first_err) if has_combined_docs(group_name) => {
            eprintln!(
                "[combined-docs] query failed for group '{group_name}' ({first_err}), \
                 wiping corrupt generation and scheduling rebuild..."
            );
            wipe_active_generation(group_name);
            schedule_background_rebuild(group_name);
            Err(first_err.context(format!(
                "combined doc query failed for group '{group_name}'; corrupt generation wiped, rebuild scheduled"
            )))
        }
        Err(err) => Err(err),
    }
}

fn wipe_active_generation(group_name: &str) {
    if let Ok(artifact_dir) = combined_docs_artifact_dir(group_name) {
        if artifact_dir.join("docs.kuzu").exists() {
            let _ = std::fs::remove_dir_all(&artifact_dir);
            invalidate_embeddings_cache();
            invalidate_hnsw_cache();
        }
    }
}

fn schedule_background_rebuild(group_name: &str) {
    let group_name = group_name.to_string();
    let refreshing = REFRESHING_GROUPS.get_or_init(|| Mutex::new(HashSet::new()));
    if !refreshing.lock().unwrap().insert(group_name.clone()) {
        return;
    }
    std::thread::spawn(move || {
        if let Ok(registry) = Registry::load() {
            if let Err(error) = build_combined_docs(&registry, &group_name) {
                eprintln!(
                    "[combined-docs] auto-recovery rebuild failed for group '{group_name}': {error}"
                );
            }
        }
        if let Some(refreshing) = REFRESHING_GROUPS.get() {
            refreshing.lock().unwrap().remove(&group_name);
        }
    });
}

pub fn schedule_group_doc_refresh(repo_root: &Path) -> Result<usize> {
    let registry = Registry::load()?;
    let canonical = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let groups: Vec<_> = registry
        .groups
        .iter()
        .filter(|(_, group)| {
            group.repos.iter().any(|repo_name| {
                registry
                    .repos
                    .get(repo_name)
                    .map(|entry| {
                        entry
                            .path
                            .canonicalize()
                            .unwrap_or_else(|_| entry.path.clone())
                            == canonical
                    })
                    .unwrap_or(false)
            })
        })
        .map(|(name, _)| name.clone())
        .filter(|name| has_combined_docs(name))
        .collect();

    let refreshing = REFRESHING_GROUPS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut scheduled = 0;
    for group_name in groups {
        if !refreshing.lock().unwrap().insert(group_name.clone()) {
            continue;
        }
        scheduled += 1;
        std::thread::spawn(move || {
            if let Ok(registry) = Registry::load() {
                if let Err(error) = build_combined_docs(&registry, &group_name) {
                    eprintln!(
                        "[combined-docs] watcher refresh failed for group '{}': {error}",
                        group_name
                    );
                }
            }
            if let Some(refreshing) = REFRESHING_GROUPS.get() {
                refreshing.lock().unwrap().remove(&group_name);
            }
        });
    }
    Ok(scheduled)
}

fn copy_to(
    conn: &kuzu::Connection<'_>,
    query: &str,
    output: &Path,
    repo_name: &str,
    table: &str,
) -> Result<()> {
    conn.query(&format!("COPY ({query}) TO '{}'", fwd_slash_path(output)))
        .map_err(|error| {
            anyhow::anyhow!("COPY {table} TO failed for repository '{repo_name}': {error}")
        })?;
    Ok(())
}

fn copy_from(conn: &kuzu::Connection<'_>, table: &str, input: &Path) -> Result<()> {
    conn.query(&format!("COPY {table} FROM '{}'", fwd_slash_path(input)))
        .map_err(|error| anyhow::anyhow!("COPY {table} FROM failed: {error}"))?;
    Ok(())
}

fn link_cross_repo_docs(
    store: &DocStore,
    repo_docs: &HashMap<String, (PathBuf, HashSet<String>)>,
    repo_aliases: &HashMap<String, Option<String>>,
) -> usize {
    let mut seen = HashSet::new();
    let mut created = 0;

    for (repo_name, (repo_root, doc_ids)) in repo_docs {
        for doc_id in doc_ids {
            let doc_path = repo_root.join(doc_id);
            let text = match std::fs::read_to_string(doc_path) {
                Ok(text) => text,
                Err(_) => continue,
            };

            for link in crate::links::extract_links(&text, doc_id) {
                if link.link_type != "github" {
                    continue;
                }
                let target_repo = match crate::links::extract_repo_from_url(&link.url)
                    .and_then(|alias| repo_aliases.get(&alias).cloned().flatten())
                {
                    Some(repo) if repo != *repo_name => repo,
                    _ => continue,
                };
                let target_path = match link.target_doc_id {
                    Some(path) => path,
                    None => continue,
                };
                let Some((_, target_ids)) = repo_docs.get(&target_repo) else {
                    continue;
                };
                let Some(target_path) = crate::links::resolve_doc_id(&target_path, target_ids)
                else {
                    continue;
                };

                let source_id = format!("[{repo_name}]::{doc_id}");
                let target_id = format!("[{target_repo}]::{target_path}");
                if seen.insert((source_id.clone(), target_id.clone(), link.url.clone()))
                    && store
                        .create_link(&source_id, &target_id, &link.url, "cross_repo")
                        .is_ok()
                {
                    created += 1;
                }
            }
        }
    }

    created
}

fn build_repo_aliases(
    registry: &Registry,
    repo_names: &[String],
) -> HashMap<String, Option<String>> {
    let mut aliases: HashMap<String, Option<String>> = HashMap::new();
    for repo_name in repo_names {
        let Some(entry) = registry.repos.get(repo_name) else {
            continue;
        };
        let mut candidates = vec![repo_name.clone(), entry.name.clone()];
        if let Some(name) = entry.path.file_name().and_then(|name| name.to_str()) {
            candidates.push(name.to_string());
        }
        if let Ok(config) = std::fs::read_to_string(entry.path.join(".git/config")) {
            for line in config.lines() {
                let Some(url) = line.trim().strip_prefix("url = ") else {
                    continue;
                };
                let slug = url
                    .trim_end_matches('/')
                    .trim_end_matches(".git")
                    .rsplit(['/', ':'])
                    .next()
                    .unwrap_or_default();
                if !slug.is_empty() {
                    candidates.push(slug.to_string());
                }
            }
        }
        for alias in candidates {
            aliases
                .entry(alias)
                .and_modify(|existing| {
                    if existing.as_deref() != Some(repo_name.as_str()) {
                        *existing = None;
                    }
                })
                .or_insert_with(|| Some(repo_name.clone()));
        }
    }
    aliases
}

fn fwd_slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_build_lock_serializes_writers() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_build_lock(dir.path()).unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join("docs-build.lock"))
            .unwrap();
        assert!(second.try_lock_exclusive().is_err());
        FileExt::unlock(&first).unwrap();
        second.try_lock_exclusive().unwrap();
    }
}
