//! Document indexing as operations (docs opt-in): the one executor,
//! `index_docs`, and `clean_docs`. With the doc watcher, these are the only
//! code that creates or deletes a project's document store, and all three
//! take the docs lock (`docs_switch::DOCS_OP_LOCK`) exclusively to do it.

use std::path::Path;

use anyhow::{Context, Result};
use infigraph_core::daemon_protocol::DocIndexStats;
use infigraph_core::docs_switch;

use crate::DocIndex;

/// The executor: records `[docs] enabled = true`, then indexes against the
/// local store in this process. `full` wipes and rebuilds (`reindex-docs`).
/// The daemon runs it for `WriteRequest::IndexDocs`. The CLI runs it
/// directly only when the process opted out of the daemon
/// (`INFIGRAPH_BACKEND=kuzu`); under the daemon backend `DocIndex::init`
/// would route, and a routed store cannot write.
pub fn index_docs(root: &Path, full: bool) -> Result<DocIndexStats> {
    docs_switch::set_docs_enabled(root, true)?;
    let _op = docs_switch::lock_docs_op(root, docs_switch::DOCS_OP_WAIT)?;
    // Work in flight for the daemon's idle exit (#203), like a watcher's
    // reindex.
    let _busy = crate::watch::ReindexGuard::enter();
    let mut idx = DocIndex::open(root)?;
    let result = if full {
        idx.reindex()?
    } else {
        idx.init()?;
        idx.index()?
    };
    let totals = idx.store().context("doc store not initialized")?.stats()?;
    Ok(DocIndexStats {
        files_scanned: result.total_files,
        files_indexed: result.indexed_files,
        chunks_created: result.total_chunks,
        bfs_discovered: result.bfs_discovered,
        documents_in_store: totals.document_count,
        chunks_in_store: totals.chunk_count,
    })
}

/// `clean-docs`: turns the switch off *first*, so a watcher stops wanting
/// to write, then deletes the index under the docs lock. That order is what
/// keeps the project out: any reindex that gets the lock after this one
/// re-reads the switch and finds it off.
pub fn clean_docs(root: &Path) -> Result<()> {
    docs_switch::set_docs_enabled(root, false)?;
    let _op = docs_switch::lock_docs_op(root, docs_switch::DOCS_OP_WAIT)?;
    DocIndex::open(root)?.clean()
}
