use crate::daemon_protocol::{write_atomic, WriteResult};
use crate::model::FileExtraction;
use crate::resolve::ResolveStats;
use crate::watch::queue::{DrainedQueue, PendingIndexItem, WaiterKind};
use crate::Infigraph;
use anyhow::Result;

/// What the unified drain actually did, for the batch-flush path's
/// existing downstream steps (embedding updates, cross-file-call event
/// emission) to consume -- mirrors the subset of `IndexResult` those steps
/// already use.
#[allow(dead_code)]
pub(crate) struct DrainOutcome {
    pub extractions: Vec<FileExtraction>,
    pub resolve_stats: ResolveStats,
}

/// Runs one combined pass -- extract, upsert, remove, resolve -- against
/// everything a `DrainedQueue` accumulated, and replies to every waiter
/// folded into it. This is the fix for the coalescing bug: there is
/// exactly one execution here, computed fresh against the drained
/// snapshot, so no operation plans against information another operation
/// has since made stale.
#[allow(dead_code)]
pub(crate) fn execute_drain(infigraph: &Infigraph, drained: DrainedQueue) -> Result<DrainOutcome> {
    let backend = infigraph
        .backend()
        .ok_or_else(|| anyhow::anyhow!("graph not initialized"))?;

    let mut to_extract: Vec<String> = Vec::new();
    let mut pre_parsed: Vec<FileExtraction> = Vec::new();
    let mut resolve_only: Vec<FileExtraction> = Vec::new();

    for (path, item) in drained.items {
        match item {
            PendingIndexItem::Raw => to_extract.push(path),
            PendingIndexItem::Structured(extraction) => pre_parsed.push(extraction),
            PendingIndexItem::ResolveOnly(extraction) => resolve_only.push(extraction),
        }
    }

    let mut whole_project_stale: Vec<String> = Vec::new();
    if drained.whole_project {
        let scan = infigraph.scan_changed_files(backend)?;
        for extraction in scan.extractions {
            // A whole-project scan finding a change for a path that ALSO
            // has an explicit pending item takes a back seat to that
            // explicit item (it's more specific/recent); only add what
            // isn't already covered.
            if !pre_parsed.iter().any(|e| e.file == extraction.file)
                && !to_extract.contains(&extraction.file)
            {
                to_extract.push(extraction.file);
            }
        }
        whole_project_stale = scan.stale_files;
    }

    let freshly_extracted: Vec<FileExtraction> = if to_extract.is_empty() {
        Vec::new()
    } else {
        infigraph.extract_paths(&to_extract)
    };

    let mut extractions = freshly_extracted;
    extractions.extend(pre_parsed);

    let mut removals: Vec<String> = drained.removals.into_iter().collect();
    removals.extend(whole_project_stale);
    for path in &removals {
        let _ = backend.remove_file(path);
    }

    if !extractions.is_empty() {
        let existing_hashes_empty = backend.get_file_hashes().unwrap_or_default().is_empty();
        backend.upsert_files_bulk(&extractions, existing_hashes_empty)?;
    }

    let use_learned = drained
        .waiters
        .iter()
        .any(|w| w.kind == WaiterKind::ResolveCalls && w.use_learned);
    let learned = if use_learned {
        Some(crate::learned::LearnedStore::load(infigraph.root()))
    } else {
        None
    };
    let mut resolve_extractions = extractions.clone();
    resolve_extractions.extend(resolve_only);
    let resolve_stats = backend
        .resolve_calls(&resolve_extractions, learned.as_ref())
        .unwrap_or_else(|e| {
            eprintln!("warning: call resolution failed: {e}");
            ResolveStats {
                total_calls: 0,
                resolved: 0,
                unresolved: 0,
                learned_resolved: 0,
                inherits_resolved: 0,
            }
        });

    for waiter in &drained.waiters {
        let result = match waiter.kind {
            WaiterKind::Index => WriteResult::Ok {
                total_files: extractions.len(),
                indexed_files: extractions.len(),
            },
            WaiterKind::UpsertFilesBulk => WriteResult::Ok {
                total_files: extractions.len(),
                indexed_files: extractions.len(),
            },
            WaiterKind::RemoveFiles => WriteResult::Ok {
                total_files: removals.len(),
                indexed_files: removals.len(),
            },
            WaiterKind::ResolveCalls => WriteResult::ResolveOk(resolve_stats.clone()),
        };
        write_atomic(&waiter.reply_path, &serde_json::to_string(&result)?)?;
    }

    Ok(DrainOutcome {
        extractions,
        resolve_stats,
    })
}
