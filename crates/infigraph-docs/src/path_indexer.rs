//! The document index as a [`PathIndexer`]: the same three operations the code
//! graph offers, over `docs.kuzu`. `index_paths` and `delete_paths` write only
//! the documents named; `index_all` is the whole-tree run that finds what no
//! event reported. All three assume the caller holds the docs lock
//! (`ops::index_doc_paths` does), as `index` always has.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use infigraph_core::watch::sink::{PathIndexer, PathOutcome};

use crate::{embed, find_repo_root, is_document_file, is_local_document_id, DocIndex};

impl DocIndex {
    /// The stored id of the document at `path` (absolute, or relative to the
    /// root): the same string `index` stores, namespace prefix included.
    fn id_of(&self, path: &Path) -> String {
        let raw = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        match self.namespace.as_deref() {
            Some(prefix) => format!("{prefix}/{raw}"),
            None => raw,
        }
    }

    fn absolute(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    /// Removes the documents at, or below, each of `targets`: a target that
    /// was a directory removes everything under `target/` and, because of the
    /// slash, nothing beside it (`docs/a` leaves `docs/ab.md` alone).
    fn delete_targets(&self, targets: &[String]) -> Result<bool> {
        let store = self.store.as_deref().context("call init() first")?;
        let stored = store
            .get_doc_hashes()
            .context("doc index: failed to load existing document hashes")?;
        let doomed: Vec<String> = stored
            .keys()
            .filter(|id| is_local_document_id(id))
            .filter(|id| {
                targets
                    .iter()
                    .any(|t| *id == t || id.starts_with(&format!("{t}/")))
            })
            .cloned()
            .collect();
        if doomed.is_empty() {
            return Ok(false);
        }
        let refs: Vec<&str> = doomed.iter().map(String::as_str).collect();
        store.delete_docs_by_ids(&refs)?;
        if !self.skip_file_embeddings {
            embed::update_doc_embeddings(store, &self.root, &[], &refs)?;
        }
        Ok(true)
    }
}

impl PathIndexer for DocIndex {
    fn index_paths(&self, paths: &[PathBuf]) -> Result<PathOutcome> {
        let store = self.store.as_deref().context("call init() first")?;
        let mut files = Vec::new();
        let mut gone = Vec::new();
        for path in paths {
            let abs = self.absolute(path);
            if !abs.exists() {
                // A rename's old half, or a delete that raced the event.
                gone.push(self.id_of(&abs));
            } else if abs.is_file() && is_document_file(&abs) {
                files.push(abs);
            }
        }
        let mut wrote = if gone.is_empty() {
            false
        } else {
            self.delete_targets(&gone)?
        };
        if files.is_empty() {
            return Ok(PathOutcome {
                files: usize::from(wrote),
            });
        }

        let existing_hashes = store
            .get_doc_hashes()
            .context("doc index: failed to load existing document hashes")?;
        let results = self.extract_documents(&files, &existing_hashes, false);
        if results.is_empty() {
            return Ok(PathOutcome {
                files: usize::from(wrote),
            });
        }
        wrote = true;

        let mut pipelines = None;
        self.store_documents(store, &results, &mut pipelines)?;
        let mut all_doc_ids = self.link_documents(store, &results);
        let seeds: HashSet<String> = results.iter().map(|(doc, _)| doc.file.clone()).collect();
        if let Some(repo_root) = find_repo_root(&self.root) {
            self.bfs_follow_links(
                store,
                &mut pipelines,
                &mut all_doc_ids,
                Some(&seeds),
                &repo_root,
                2,
                50,
            )?;
        }
        for warning in pipelines.map(|run| run.finish(store)).unwrap_or_default() {
            eprintln!("[docs] pipeline warning: {warning}");
        }
        self.note_embedding_health(store);
        Ok(PathOutcome {
            files: usize::from(wrote),
        })
    }

    fn delete_paths(&self, paths: &[PathBuf]) -> Result<PathOutcome> {
        let targets: Vec<String> = paths
            .iter()
            .map(|path| self.id_of(&self.absolute(path)))
            .collect();
        let wrote = self.delete_targets(&targets)?;
        if wrote {
            if let Some(store) = self.store.as_deref() {
                self.note_embedding_health(store);
            }
        }
        Ok(PathOutcome {
            files: usize::from(wrote),
        })
    }

    fn index_all(&self) -> Result<PathOutcome> {
        let result = self.index()?;
        Ok(PathOutcome {
            files: usize::from(
                result.indexed_files > 0 || result.bfs_discovered > 0 || result.pruned > 0,
            ),
        })
    }
}
