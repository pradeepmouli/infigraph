pub mod backend;
pub mod chunk;
pub mod combined;
pub mod daemon_source;
pub mod daemon_store;
pub mod embed;
pub mod extract;
#[cfg(feature = "remote")]
pub mod neo4j_store;
pub mod ops;
pub mod pipelines;
pub mod query;
pub mod search;
pub mod store;
pub mod watch;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use backend::DocBackend;
use chunk::{Chunk, ChunkStrategy};
use extract::ExtractedDoc;
use infigraph_core::child::ChildTimeouts;
use pipelines::PipelineRun;
use store::DocStore;

pub mod links;

pub struct DocIndex {
    root: PathBuf,
    db_path: PathBuf,
    store: Option<Box<dyn DocBackend>>,
    skip_file_embeddings: bool,
    /// When set (remote multi-repo mode only), all doc/chunk IDs are prefixed
    /// with `{namespace}/`, mirroring `Infigraph::set_namespace` for the code
    /// graph — keeps repos sharing one Neo4j instance from colliding on
    /// identical relative paths (e.g. every repo's README.md).
    namespace: Option<String>,
    /// How long a pipeline plugin may take to start and to answer.
    pipeline_timeouts: ChildTimeouts,
    /// The shared docs lock, held for this index's lifetime when it opened
    /// the store locally for reading (`open_existing`). Declared last so
    /// the store closes before the lock is released.
    read_lock: Option<infigraph_core::lockfile::LockFile>,
}

pub struct DocIndexResult {
    pub total_files: usize,
    pub indexed_files: usize,
    pub total_chunks: usize,
    pub bfs_discovered: usize,
    pub new_chunks: Vec<Chunk>,
    pub changed_files: Vec<String>,
    /// What the pipeline plugins reported during this run. A plugin that
    /// failed never fails indexing; it lands here.
    pub pipeline_warnings: Vec<String>,
}

/// Whether a stored document id names a local file, which is the only kind
/// the disk walk can vouch for. A document from an external source carries a
/// URL-style id (`confluence://SPACE/123`), and a manifest node has no file at
/// all (the empty key); neither is "gone" because no local file matches.
fn is_local_document_id(id: &str) -> bool {
    !id.is_empty() && !id.contains("://")
}

impl DocIndex {
    pub fn open(root: &Path) -> Result<Self> {
        let tg_dir = root.join(".infigraph");
        if !infigraph_core::daemon::lifecycle::is_remote_backend() {
            std::fs::create_dir_all(&tg_dir)?;
        }
        let db_path = tg_dir.join("docs.kuzu");
        Ok(Self {
            root: root.to_path_buf(),
            db_path,
            store: None,
            skip_file_embeddings: false,
            namespace: None,
            pipeline_timeouts: ChildTimeouts::DEFAULT,
            read_lock: None,
        })
    }

    /// Open and initialize the project's existing document index, for a
    /// reader or for a write into a store that must already exist
    /// (confluence, manifest links). It never creates one: a project that
    /// has not opted in, or whose store is missing, is `DocsNotIndexed`, and
    /// nothing is written. That includes `.infigraph/` itself, which `open`
    /// would create. Remote mode is unaffected. A local open holds the shared
    /// docs lock for the index's lifetime, so `clean-docs` cannot delete the
    /// store under it.
    pub fn open_existing(root: &Path) -> Result<Self> {
        use infigraph_core::docs_switch;

        let remote = infigraph_core::daemon::lifecycle::is_remote_backend();
        if !remote && !docs_switch::docs_indexed(root) {
            return Err(docs_switch::DocsNotIndexed.into());
        }
        // A routed or remote store is not opened in this process.
        let read_lock = if remote || infigraph_core::daemon_backend_selected() {
            None
        } else {
            let lock = docs_switch::lock_docs_read(root)?;
            if !docs_switch::docs_indexed(root) {
                return Err(docs_switch::DocsNotIndexed.into());
            }
            Some(lock)
        };
        let mut idx = Self::open(root)?;
        idx.read_lock = read_lock;
        // A reader never repairs: it holds only the shared lock, and a wipe
        // and rebuild is a write. `index-docs` and the watcher repair, under
        // the exclusive lock.
        idx.init_inner(false)?;
        Ok(idx)
    }

    /// Set a namespace prefix (`org/repo`) for multi-repo doc indexing into a
    /// shared Neo4j instance. All doc/chunk IDs are prefixed with `{namespace}/`.
    pub fn set_namespace(&mut self, ns: &str) {
        self.namespace = Some(ns.to_string());
    }

    pub fn init(&mut self) -> Result<()> {
        self.init_inner(true)
    }

    /// [`init`](Self::init); `repair` says whether a store that will not open
    /// because it is corrupt may be wiped and rebuilt. Writers repair,
    /// readers report.
    fn init_inner(&mut self, repair: bool) -> Result<()> {
        #[cfg(feature = "remote")]
        if infigraph_core::daemon::lifecycle::is_remote_backend() {
            let neo = neo4j_store::Neo4jDocStore::connect_from_env()?;
            neo.init_schema()?;
            self.store = Some(Box::new(neo));
            return Ok(());
        }

        // Daemon-routed documents. Checked after the remote branch above:
        // `INFIGRAPH_BACKEND` holds one value, so `neo4j` and `daemon` are
        // mutually exclusive and Neo4j (a real client/server DB, which
        // routes writes too) wins outright.
        //
        // The daemon process itself is spawned with `INFIGRAPH_BACKEND`
        // removed (`daemon::lifecycle`), so it falls through to the local
        // `DocStore` below and never routes into its own read service --
        // which would deadlock, since indexing reads through a store it is
        // holding and `DocStore::open` takes the process-wide `DB_LOCK`.
        if infigraph_core::daemon_backend_selected() {
            // Same hard requirement the code graph has: with reads routed,
            // no daemon means no document reads at all. Start one (or fail
            // with an actionable message) rather than letting the first
            // read discover it.
            infigraph_core::daemon::lifecycle::ensure_daemon_for_routed_access(&self.root)?;
            self.store = Some(Box::new(daemon_store::DaemonDocStore::new(&self.root)));
            return Ok(());
        }

        // A daemon's doc watcher and read service open `docs.kuzu` for
        // moments at a time (the daemon opens it at startup, attaches, and
        // catches up at once), so a direct open landing in one of those
        // windows waits it out, as the graph's does, instead of refusing.
        let opened = infigraph_core::open_kuzu_with_retry(
            || DocStore::open(&self.db_path),
            || {
                infigraph_core::graph::lock_probe::probe_graph_lock(
                    &self.db_path,
                    infigraph_core::graph::lock_probe::ProbeFor::Write,
                )
            },
            std::time::Duration::from_secs(3),
        );
        match opened {
            Ok(store) => {
                self.store = Some(Box::new(store));
                Ok(())
            }
            Err(first_err) if infigraph_core::graph::open_failure_is_not_corruption(&first_err) => {
                // R3.1.1 / #143: a live holder's lock, a transient WAL race,
                // or a store written on another lbug version is not
                // corruption. Wiping here used to unlink a live doc
                // watcher's store out from under it (its later writes went
                // to an orphaned inode) and silently downgrade a
                // newer-version store; refuse and say why instead.
                let ctx =
                    infigraph_core::graph::non_corruption_open_context(&first_err, &self.db_path);
                Err(first_err.context(ctx))
            }
            Err(first_err) if !repair => Err(first_err.context(format!(
                "the document index at {} will not open and may be corrupt; \
                 run `infigraph reindex-docs` to rebuild it",
                self.db_path.display()
            ))),
            Err(first_err) => {
                eprintln!(
                    "[docs] open failed ({first_err}), wiping corrupt doc index and rebuilding..."
                );
                self.clean()?;
                let store = DocStore::open(&self.db_path).with_context(|| {
                    format!("docs kuzu still unreadable after wipe (was: {first_err})")
                })?;
                self.store = Some(Box::new(store));
                self.index()?;
                Ok(())
            }
        }
    }

    pub fn store(&self) -> Option<&dyn DocBackend> {
        self.store.as_deref()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Test seam: the timeouts pipeline plugins run under.
    #[doc(hidden)]
    pub fn set_pipeline_timeouts(&mut self, timeouts: ChildTimeouts) {
        self.pipeline_timeouts = timeouts;
    }

    /// A pipeline run for this index's project. Plugins are looked up for the
    /// project root, which is not always the doc root.
    fn pipeline_run(&self) -> PipelineRun {
        PipelineRun::for_project(
            &infigraph_core::project::resolve_project_root(&self.root),
            self.pipeline_timeouts,
        )
    }

    pub fn set_skip_file_embeddings(&mut self, skip: bool) {
        self.skip_file_embeddings = skip;
    }

    pub fn clean(&mut self) -> Result<()> {
        self.store = None;
        let tg_dir = self.root.join(".infigraph");
        if self.db_path.is_dir() {
            let _ = std::fs::remove_dir_all(&self.db_path);
        } else {
            let _ = std::fs::remove_file(&self.db_path);
        }
        // APPENDED names, not with_extension: Kuzu's WAL/lock siblings for
        // "docs.kuzu" are "docs.kuzu.wal"/"docs.kuzu.lock" -- with_extension
        // computed "docs.wal"/"docs.lock", files Kuzu never wrote, so the
        // real WAL survived every wipe. A leftover WAL carries the OLD
        // database's ID and makes the freshly rebuilt database at this path
        // permanently unopenable ("Database ID does not match"), and a
        // leftover lock payload from a dead holder re-trips the
        // unreplayed-WAL guard in DocStore::open, wedging init()'s
        // wipe-and-rebuild recovery in a refuse->wipe->refuse loop.
        infigraph_core::graph::remove_wal_family(&self.db_path);
        let _ = std::fs::remove_file(infigraph_core::graph::db_lock_path(&self.db_path));
        let _ = std::fs::remove_file(tg_dir.join("docs_embeddings.bin"));
        let _ = std::fs::remove_file(tg_dir.join("docs_hnsw_index.usearch"));
        let _ = std::fs::remove_file(tg_dir.join("docs_hnsw_index.meta"));
        infigraph_core::embed::invalidate_embeddings_cache();
        infigraph_core::embed::invalidate_hnsw_cache();
        Ok(())
    }

    pub fn reindex(&mut self) -> Result<DocIndexResult> {
        self.clean()?;
        self.init()?;
        self.index()
    }

    pub fn index(&self) -> Result<DocIndexResult> {
        let store = self.store.as_deref().context("call init() first")?;

        let (files, listing_complete) = self.collect_doc_files()?;
        let total = files.len();

        if total == 0 {
            // No document is left, so every stored one is stale: deleting the
            // last document must delete its rows too. But "found none" is only
            // "none" when the whole root could be listed; an unmounted volume,
            // a removed worktree or a permission error is not a reason to
            // delete what was indexed.
            let existing_hashes = store
                .get_doc_hashes()
                .context("doc index: failed to load existing document hashes")?;
            self.prune_stale_docs(store, &existing_hashes, &files, listing_complete);
            return Ok(DocIndexResult {
                total_files: 0,
                indexed_files: 0,
                total_chunks: 0,
                bfs_discovered: 0,
                new_chunks: vec![],
                changed_files: vec![],
                pipeline_warnings: vec![],
            });
        }

        // Never treat a failed hash load as "no documents yet": that
        // re-embeds every doc and, worse, turns the stale-doc prune below
        // into a silent no-op (#144) -- the same failure shape as the SCIP
        // symbol preload that once ballooned a graph.
        let existing_hashes = store
            .get_doc_hashes()
            .context("doc index: failed to load existing document hashes")?;

        let done = AtomicUsize::new(0);
        let root = &self.root;
        let ns = self.namespace.as_deref();

        let results: Vec<(ExtractedDoc, Vec<Chunk>)> = files
            .par_iter()
            .filter_map(|path| {
                let raw_rel = path
                    .strip_prefix(root)
                    .ok()?
                    .to_string_lossy()
                    .replace('\\', "/");
                let rel = match ns {
                    Some(prefix) => format!("{prefix}/{raw_rel}"),
                    None => raw_rel,
                };
                let bytes = std::fs::read(path).ok()?;
                let hash = {
                    let mut h = Sha256::new();
                    h.update(&bytes);
                    format!("{:x}", h.finalize())
                };

                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                let pct = n * 100 / total;
                let prev_pct = (n - 1) * 100 / total;
                if (pct / 25) > (prev_pct / 25) || n == total {
                    eprintln!("Doc indexing: {}/{} ({}%)", n, total, pct);
                }

                if existing_hashes.get(&rel).map(|s| s.as_str()) == Some(hash.as_str()) {
                    return None;
                }

                let ext = path.extension()?.to_string_lossy().to_lowercase();
                let doc = extract::extract_document(path, &bytes, &ext).ok()?;
                let strategy = ChunkStrategy::for_extension(&ext);
                let chunks = chunk::chunk_document(&doc, &rel, &hash, strategy);
                Some((
                    ExtractedDoc {
                        file: rel,
                        content_hash: hash,
                        ..doc
                    },
                    chunks,
                ))
            })
            .collect();

        let indexed = results.len();
        let total_chunks: usize = results.iter().map(|(_, c)| c.len()).sum();

        // Created when the first document is written, so a run that changes
        // nothing neither loads the plugin registry nor spawns a plugin.
        let mut pipelines: Option<PipelineRun> = None;

        if !results.is_empty() {
            let docs: Vec<&ExtractedDoc> = results.iter().map(|(d, _)| d).collect();
            let chunks: Vec<&Chunk> = results.iter().flat_map(|(_, c)| c.iter()).collect();
            store.upsert_docs(&docs, &chunks)?;
            pipelines
                .get_or_insert_with(|| self.pipeline_run())
                .apply(store, &docs);
        }

        let result_chunks: Vec<Chunk> = results.iter().flat_map(|(_, c)| c.clone()).collect();
        let result_changed: Vec<String> = results.iter().map(|(d, _)| d.file.clone()).collect();

        if total_chunks > 0 && !self.skip_file_embeddings {
            let all_chunks: Vec<&Chunk> = results.iter().flat_map(|(_, c)| c.iter()).collect();
            let changed_files: Vec<&str> = results.iter().map(|(d, _)| d.file.as_str()).collect();
            #[cfg(feature = "remote")]
            if infigraph_core::daemon::lifecycle::is_remote_backend() {
                if let Ok(pg) = infigraph_core::meta::PostgresMetaStore::connect_from_env_cached() {
                    embed::update_doc_embeddings_remote(store, &pg, &all_chunks, &changed_files)?;
                } else {
                    eprintln!(
                        "Warning: remote mode but Postgres unavailable, skipping doc embeddings"
                    );
                }
            } else {
                embed::update_doc_embeddings(store, &self.root, &all_chunks, &changed_files)?;
            }
            #[cfg(not(feature = "remote"))]
            embed::update_doc_embeddings(store, &self.root, &all_chunks, &changed_files)?;
        }

        self.prune_stale_docs(store, &existing_hashes, &files, listing_complete);

        // Extract links from indexed docs and create LINKS_TO edges.
        // Scope to this repo's namespace so cross-repo docs aren't offered as
        // link targets that don't actually exist in this repo's tree.
        let mut all_doc_ids: HashSet<String> = {
            let existing = store.get_doc_hashes().unwrap_or_default();
            existing
                .keys()
                .filter(|k| match ns {
                    Some(prefix) => k.starts_with(&format!("{prefix}/")),
                    None => true,
                })
                .cloned()
                .collect()
        };
        if !results.is_empty() {
            for (doc, _) in &results {
                links::extract_and_link_doc(store, doc, &all_doc_ids);
            }
        }

        // BFS: follow links to docs outside the doc root but within the repo
        let bfs_discovered = if let Some(repo_root) = find_repo_root(&self.root) {
            let n =
                self.bfs_follow_links(store, &mut pipelines, &mut all_doc_ids, &repo_root, 2, 50)?;
            if n > 0 {
                eprintln!("BFS: discovered and indexed {} doc(s) outside root", n);
            }
            n
        } else {
            0
        };

        let pipeline_warnings = pipelines.map(|run| run.finish(store)).unwrap_or_default();

        Ok(DocIndexResult {
            total_files: total,
            indexed_files: indexed,
            total_chunks,
            bfs_discovered,
            new_chunks: result_chunks,
            changed_files: result_changed,
            pipeline_warnings,
        })
    }

    /// Removes the documents of files that no longer exist on disk (and their
    /// pipelines, with them), if the listing of the root was complete. Only
    /// local document ids are candidates ([`is_local_document_id`]). Scoped to this repo's namespace on both sides:
    /// `existing_hashes` pools every repo sharing the store in remote mode, so
    /// an unscoped diff would flag every other repo's docs as "stale" and
    /// delete them.
    fn prune_stale_docs(
        &self,
        store: &dyn DocBackend,
        existing_hashes: &std::collections::HashMap<String, String>,
        files: &[PathBuf],
        listing_complete: bool,
    ) {
        // A walk error shortens `files`, and a short list reads as "these
        // documents were deleted". Only a complete listing may say that.
        if !listing_complete {
            eprintln!(
                "warn: could not list all of {}; leaving the stored documents alone",
                self.root.display()
            );
            return;
        }
        let ns = self.namespace.as_deref();
        let current_files: HashSet<String> = files
            .iter()
            .filter_map(|p| {
                p.strip_prefix(&self.root).ok().map(|r| {
                    let raw = r.to_string_lossy().replace('\\', "/");
                    match ns {
                        Some(prefix) => format!("{prefix}/{raw}"),
                        None => raw,
                    }
                })
            })
            .collect();
        let stale: Vec<String> = existing_hashes
            .keys()
            .filter(|k| match ns {
                Some(prefix) => k.starts_with(&format!("{prefix}/")),
                None => true,
            })
            .filter(|k| is_local_document_id(k))
            .filter(|k| !current_files.contains(k.as_str()))
            .cloned()
            .collect();
        if !stale.is_empty() {
            eprintln!("Doc pruning: removing {} stale doc(s)", stale.len());
            let stale_refs: Vec<&str> = stale.iter().map(|s| s.as_str()).collect();
            if let Err(e) = store.delete_docs_by_ids(&stale_refs) {
                eprintln!("warn: doc pruning failed, stale docs remain: {e:#}");
            }
        }
    }

    /// The document files under the root, and whether the walk could list
    /// everything it was pointed at (a walk error, such as a missing or
    /// unreadable directory, makes the list possibly short).
    fn collect_doc_files(&self) -> Result<(Vec<PathBuf>, bool)> {
        let mut files = Vec::new();
        let mut complete = true;
        let walker = infigraph_core::ignore_rules::walk_builder(&self.root).build();
        for result in walker {
            let entry = match result {
                Ok(e) => e,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                let path = entry.path().to_path_buf();
                if is_document_file(&path) {
                    files.push(path);
                }
            }
        }
        Ok((files, complete))
    }

    fn bfs_follow_links(
        &self,
        store: &dyn DocBackend,
        pipelines: &mut Option<PipelineRun>,
        indexed_docs: &mut HashSet<String>,
        repo_root: &Path,
        max_depth: usize,
        max_extra: usize,
    ) -> Result<usize> {
        let ignore_dirs = [
            ".infigraph",
            ".git",
            "node_modules",
            "__pycache__",
            ".venv",
            "venv",
            "target",
            "build",
            "dist",
            ".tox",
        ];
        let repo_root = repo_root
            .canonicalize()
            .unwrap_or_else(|_| repo_root.to_path_buf());
        let root_canonical = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let mut total_new = 0usize;
        let mut new_chunks = Vec::new();
        let mut changed_files = Vec::new();
        let ns = self.namespace.as_deref();
        let strip_ns = |id: &str| -> String {
            match ns {
                Some(prefix) => id
                    .strip_prefix(&format!("{prefix}/"))
                    .unwrap_or(id)
                    .to_string(),
                None => id.to_string(),
            }
        };
        let mut frontier: Vec<PathBuf> = indexed_docs
            .iter()
            .filter_map(|rel| {
                let p = self.root.join(strip_ns(rel));
                p.canonicalize().ok().filter(|c| c.is_file())
            })
            .collect();

        for _depth in 0..max_depth {
            if frontier.is_empty() || total_new >= max_extra {
                break;
            }
            let mut next_frontier = Vec::new();

            for doc_path in &frontier {
                if total_new >= max_extra {
                    break;
                }
                let text = match std::fs::read_to_string(doc_path) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let doc_file = doc_path.to_string_lossy();
                let extracted_links = links::extract_links(&text, &doc_file);

                for link in &extracted_links {
                    if total_new >= max_extra {
                        break;
                    }
                    let abs = match links::resolve_link_to_abs_path(&link.url, doc_path) {
                        Some(p) => p,
                        None => continue,
                    };
                    if !is_document_file(&abs) {
                        continue;
                    }
                    // Skip symlinks (check before canonicalize)
                    if let Ok(meta) = std::fs::symlink_metadata(&abs) {
                        if meta.file_type().is_symlink() {
                            continue;
                        }
                    }
                    let abs = abs.canonicalize().unwrap_or(abs);
                    if !abs.starts_with(&repo_root) {
                        continue;
                    }
                    // Check ignored dirs — only check path components relative to repo root
                    let rel_to_repo = abs.strip_prefix(&repo_root).unwrap_or(&abs);
                    let in_ignored = rel_to_repo.components().any(|c| {
                        if let std::path::Component::Normal(s) = c {
                            let s = s.to_string_lossy();
                            ignore_dirs.contains(&s.as_ref()) || s.starts_with('.')
                        } else {
                            false
                        }
                    });
                    if in_ignored {
                        continue;
                    }

                    // Build relative ID (relative to doc root for consistency, or absolute if outside)
                    let rel_id = if let Ok(rel) = abs.strip_prefix(&root_canonical) {
                        let raw = rel.to_string_lossy().replace('\\', "/");
                        match ns {
                            Some(prefix) => format!("{prefix}/{raw}"),
                            None => raw,
                        }
                    } else {
                        abs.to_string_lossy().replace('\\', "/")
                    };

                    if indexed_docs.contains(&rel_id) {
                        continue;
                    }

                    // Index this file
                    let bytes = match std::fs::read(&abs) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    let hash = {
                        let mut h = Sha256::new();
                        h.update(&bytes);
                        format!("{:x}", h.finalize())
                    };
                    let ext = match abs.extension() {
                        Some(e) => e.to_string_lossy().to_lowercase(),
                        None => continue,
                    };
                    let doc = match extract::extract_document(&abs, &bytes, &ext) {
                        Ok(d) => d,
                        Err(_) => continue,
                    };
                    let strategy = ChunkStrategy::for_extension(&ext);
                    let doc = ExtractedDoc {
                        file: rel_id.clone(),
                        content_hash: hash.clone(),
                        ..doc
                    };
                    let chunks = chunk::chunk_document(&doc, &rel_id, &hash, strategy);

                    let docs_ref = vec![&doc];
                    let chunks_ref: Vec<&Chunk> = chunks.iter().collect();
                    if store.upsert_docs(&docs_ref, &chunks_ref).is_ok() {
                        pipelines
                            .get_or_insert_with(|| self.pipeline_run())
                            .apply(store, &docs_ref);
                        indexed_docs.insert(rel_id.clone());
                        changed_files.push(rel_id);
                        new_chunks.extend(chunks);
                        next_frontier.push(abs);
                        total_new += 1;
                    }
                }
            }
            frontier = next_frontier;
        }

        if !new_chunks.is_empty() {
            let chunk_refs: Vec<&Chunk> = new_chunks.iter().collect();
            let changed_file_refs: Vec<&str> = changed_files.iter().map(String::as_str).collect();
            embed::update_doc_embeddings(store, &self.root, &chunk_refs, &changed_file_refs)?;
        }

        // Re-run link extraction for all docs (newly discovered may link to each other).
        // Scope to this repo's namespace — other repos' doc IDs don't resolve
        // under this repo's root and would just fail the read below, but
        // skipping them up front avoids wasted global-store round-trips.
        if total_new > 0 {
            let all_hashes = store.get_doc_hashes().unwrap_or_default();
            let all_ids: HashSet<String> = all_hashes
                .keys()
                .filter(|k| match ns {
                    Some(prefix) => k.starts_with(&format!("{prefix}/")),
                    None => true,
                })
                .cloned()
                .collect();
            for doc_id in all_ids.iter() {
                let doc_path = if doc_id.starts_with('/') {
                    PathBuf::from(doc_id)
                } else {
                    self.root.join(strip_ns(doc_id))
                };
                if let Ok(text) = std::fs::read_to_string(&doc_path) {
                    let doc = ExtractedDoc {
                        file: doc_id.clone(),
                        title: None,
                        content_hash: String::new(),
                        format: extract::DocFormat::Markdown,
                        text,
                        page_count: None,
                    };
                    links::extract_and_link_doc(store, &doc, &all_ids);
                }
            }
        }

        Ok(total_new)
    }
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn is_document_file(path: &Path) -> bool {
    let ext = match path.extension() {
        Some(e) => e.to_string_lossy().to_lowercase(),
        None => return false,
    };
    matches!(
        ext.as_str(),
        "md" | "markdown"
            | "txt"
            | "rst"
            | "adoc"
            | "org"
            | "pdf"
            | "docx"
            | "pptx"
            | "xlsx"
            | "rtf"
            | "html"
            | "htm"
            | "epub"
            | "xml"
            | "xsl"
            | "xsd"
            | "svg"
            | "plist"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_that_is_gone_is_an_incomplete_listing_not_an_empty_one() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let idx = DocIndex::open(&root).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        let (files, complete) = idx.collect_doc_files().unwrap();
        assert!(files.is_empty());
        assert!(!complete, "a missing root listed as complete");
    }

    #[test]
    fn only_file_ids_are_local_document_ids() {
        for local in ["a.md", "docs/a.md", "../README.md", "org/repo/a.md"] {
            assert!(is_local_document_id(local), "{local}");
        }
        for external in ["", "confluence://SP/1", "https://example.com/x"] {
            assert!(!is_local_document_id(external), "{external:?}");
        }
    }

    #[test]
    fn a_readable_root_with_no_documents_is_a_complete_listing() {
        let root = tempfile::tempdir().unwrap();
        let idx = DocIndex::open(root.path()).unwrap();
        let (files, complete) = idx.collect_doc_files().unwrap();
        assert!(files.is_empty() && complete);
    }
}
