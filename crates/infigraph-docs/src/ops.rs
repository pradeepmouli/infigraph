//! Document indexing as operations (docs opt-in): the one executor,
//! `index_docs`, and `clean_docs`. With the doc watcher, these are the only
//! code that creates or deletes a project's document store, and all three
//! take the docs lock (`docs_switch::DOCS_OP_LOCK`) exclusively to do it.

use std::path::Path;

use anyhow::{Context, Result};
use infigraph_core::daemon_protocol::DocIndexStats;
use infigraph_core::docs_switch;
use infigraph_core::watch::sink::PathIndexer;

use crate::DocIndex;

/// The executor: under the docs lock, records `[docs] enabled = true` and
/// indexes against the local store in this process. `full` wipes and rebuilds (`reindex-docs`).
/// The daemon runs it for `WriteRequest::IndexDocs`. The CLI runs it
/// directly only when the process opted out of the daemon
/// (`INFIGRAPH_BACKEND=kuzu`); under the daemon backend `DocIndex::init`
/// would route, and a routed store cannot write.
pub fn index_docs(root: &Path, full: bool) -> Result<DocIndexStats> {
    docs_switch::ensure_switchable_root(root)?;
    let _op = docs_switch::lock_docs_op(root, docs_switch::DOCS_OP_WAIT)?;
    // Recorded under the lock, not before it: a `clean-docs` that got the
    // lock first has already turned the switch off and deleted the store,
    // and this run then turns it back on for the index it goes on to make.
    docs_switch::set_docs_enabled(root, true)?;
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
        pipeline_warnings: result.pipeline_warnings,
    })
}

/// The executor for `IndexDocPaths`: indexes the documents at `upserts` and
/// removes those at, or under, `removals`, under the same exclusive docs lock
/// and with the same two guards as the watcher's reindex had. Documents are
/// opt-in, so a run never turns them on or creates a store: it returns
/// `false`, having written nothing, when the project has no `.infigraph/`
/// (taking the lock would create one) or `[docs] enabled` is off, and the
/// switch is read again under the lock because `clean-docs` turns it off
/// before it takes the lock to delete. Returns whether the store changed.
pub fn index_doc_paths(root: &Path, upserts: &[String], removals: &[String]) -> Result<bool> {
    if !root.join(".infigraph").is_dir() || !docs_switch::docs_enabled(root) {
        return Ok(false);
    }
    let _op = docs_switch::lock_docs_op(root, docs_switch::DOCS_OP_WAIT)?;
    if !docs_switch::docs_enabled(root) {
        return Ok(false);
    }
    // Work in flight for the daemon's idle exit (#203).
    let _busy = crate::watch::ReindexGuard::enter();
    let mut idx = DocIndex::open(root)?;
    idx.init()?;
    let paths = |rels: &[String]| -> Vec<std::path::PathBuf> {
        rels.iter().map(std::path::PathBuf::from).collect()
    };
    let mut wrote = false;
    if !removals.is_empty() {
        wrote |= idx.delete_paths(&paths(removals))?.wrote();
    }
    if !upserts.is_empty() {
        wrote |= idx.index_paths(&paths(upserts))?.wrote();
    }
    // Nothing written means nothing for a combined group to pick up.
    if wrote {
        match crate::combined::schedule_group_doc_refresh(root) {
            Ok(count) if count > 0 => {
                eprintln!("[docs] refreshing {count} combined document group(s)")
            }
            Ok(_) => {}
            Err(e) => eprintln!("[docs] combined refresh error: {e}"),
        }
    }
    Ok(wrote)
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

/// The executor for `AckDocEntries`: records and forgets entry
/// acknowledgements in the existing store, under the same exclusive docs lock
/// as an index run so a reindex cannot wipe the store between its read and
/// its write. It never creates the store, and never opts a project in: with no
/// store there is nothing a review could be recorded in.
pub fn ack_docs(
    root: &Path,
    acks: &[infigraph_core::daemon_protocol::DocAck],
    drop: &[String],
) -> Result<(usize, usize)> {
    let _op = docs_switch::lock_docs_op(root, docs_switch::DOCS_OP_WAIT)?;
    if !docs_switch::docs_indexed(root) {
        return Err(docs_switch::DocsNotIndexed.into());
    }
    let _busy = crate::watch::ReindexGuard::enter();
    let mut idx = DocIndex::open(root)?;
    idx.init_inner(false)?;
    idx.store()
        .context("doc store not initialized")?
        .upsert_acks(acks, drop)
}

/// How `ack_docs` runs, for every caller: under the daemon backend it asks
/// the daemon (`WriteRequest::AckDocEntries`), which runs [`ack_docs`] on its
/// docs handle; otherwise it runs here. A client never opens the store itself.
pub fn request_ack_docs(
    root: &Path,
    acks: &[infigraph_core::daemon_protocol::DocAck],
    drop: &[String],
) -> Result<(usize, usize)> {
    use infigraph_core::daemon_protocol::{WriteRequest, WriteResult};

    if !infigraph_core::daemon_backend_selected() {
        return ack_docs(root, acks, drop);
    }
    infigraph_core::daemon::lifecycle::ensure_daemon_for_routed_access(root)?;
    let result = infigraph_core::daemon::writes::submit(
        root,
        &WriteRequest::AckDocEntries {
            acks: acks.to_vec(),
            drop: drop.to_vec(),
        },
        infigraph_core::daemon::writes::WriteOpts {
            timeout: docs_switch::DOCS_OP_WAIT,
            cancel: None,
            on_slot_wait: None,
        },
    )?;
    match result {
        WriteResult::DocsAcked { stored, dropped } => Ok((stored, dropped)),
        WriteResult::Err { message } => {
            anyhow::bail!("recording acknowledgements failed: {message}")
        }
        other => {
            anyhow::bail!("recording acknowledgements returned an unexpected result: {other:?}")
        }
    }
}

/// How `index-docs` runs, for every caller: under the daemon backend it
/// asks the daemon (`WriteRequest::IndexDocs`), which runs [`index_docs`]
/// beside the store it owns; otherwise (`INFIGRAPH_BACKEND=kuzu`, or
/// remote) it runs [`index_docs`] here. There is no second indexer.
pub fn request_index_docs(root: &Path, full: bool) -> Result<DocIndexStats> {
    use infigraph_core::daemon_protocol::{WriteRequest, WriteResult};

    if !infigraph_core::daemon_backend_selected() {
        return index_docs(root, full);
    }
    infigraph_core::daemon::lifecycle::ensure_daemon_for_routed_access(root)?;
    let result = infigraph_core::daemon::writes::submit(
        root,
        &WriteRequest::IndexDocs { full },
        infigraph_core::daemon::writes::WriteOpts {
            timeout: docs_switch::DOCS_OP_WAIT,
            cancel: None,
            on_slot_wait: None,
        },
    )?;
    match result {
        WriteResult::DocsIndexed(stats) => Ok(stats),
        WriteResult::Err { message } => anyhow::bail!("document indexing failed: {message}"),
        other => anyhow::bail!("document indexing returned an unexpected result: {other:?}"),
    }
}

/// [`request_index_docs`], but only for a project that has opted in:
/// `None` for one that has not. A group build and MCP's in-process
/// `index_project` refresh documents this way, so neither opts a project in
/// on its owner's behalf.
pub fn request_index_docs_if_enabled(root: &Path) -> Result<Option<DocIndexStats>> {
    if !docs_switch::docs_enabled(root) {
        return Ok(None);
    }
    request_index_docs(root, false).map(Some)
}

/// The document step of a group build (CLI and MCP) for one repo: refresh
/// its documents if it opted in, and touch nothing if it did not. Returns
/// the files a breadth-first link discovery added.
pub fn refresh_docs_if_enabled(root: &Path) -> Result<usize> {
    Ok(request_index_docs_if_enabled(root)?.map_or(0, |stats| stats.bfs_discovered))
}

/// The report `index-docs` and `reindex-docs` print, and MCP's in-process
/// fallbacks return.
pub fn stats_report(what: &str, stats: &DocIndexStats, elapsed: std::time::Duration) -> String {
    let mut report = format!(
        "{what} complete in {:.1}s\n  Files scanned: {}\n  Files indexed: {}\n  Chunks created: {}\n  Total documents in store: {}\n  Total chunks in store: {}",
        elapsed.as_secs_f64(),
        stats.files_scanned,
        stats.files_indexed,
        stats.chunks_created,
        stats.documents_in_store,
        stats.chunks_in_store
    );
    if !stats.pipeline_warnings.is_empty() {
        report.push_str(&format!(
            "\n  Pipeline warnings ({}):",
            stats.pipeline_warnings.len()
        ));
        for warning in &stats.pipeline_warnings {
            report.push_str(&format!("\n    - {warning}"));
        }
    }
    report
}
