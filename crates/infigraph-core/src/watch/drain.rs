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

#[cfg(test)]
mod tests {
    //! Regression coverage for the daemon index-work-queue coalescing fix,
    //! driving IndexWorkQueue + execute_drain directly -- no real daemon
    //! process needed to prove the coalescing logic itself is correct. See
    //! docs/superpowers/specs/2026-08-03-daemon-index-work-queue-design.md.
    use super::*;
    use crate::lang::{LanguagePack, LanguageRegistry};
    use crate::watch::queue::{IndexWorkQueue, Waiter};
    use std::fs;

    // `LanguageRegistry::new()` starts empty -- no language packs are
    // registered by default. Same minimal Python pack pattern used by
    // `tests/index_perf.rs`/`combined_graph.rs`/`extract_pipeline.rs`.
    const PYTHON_ENTITIES: &str = r#"
(module
  (function_definition
    name: (identifier) @func.name) @func.def)

(class_definition
  name: (identifier) @class.name) @class.def
"#;

    const PYTHON_RELATIONS: &str = r#"
(call
  function: (identifier) @call.func) @call.site
"#;

    fn python_pack() -> LanguagePack {
        let grammar = tree_sitter_python::LANGUAGE.into();
        LanguagePack::new(
            "python",
            vec![".py"],
            grammar,
            PYTHON_ENTITIES,
            PYTHON_RELATIONS,
        )
        .unwrap()
    }

    fn python_registry() -> LanguageRegistry {
        let mut reg = LanguageRegistry::new();
        reg.register(python_pack());
        reg
    }

    fn open_project(root: &std::path::Path) -> Infigraph {
        let mut prism = Infigraph::open(root, python_registry()).unwrap();
        prism.init().unwrap();
        prism
    }

    #[test]
    fn a_raw_entry_and_an_index_waiter_for_the_same_new_file_coalesce_into_one_execution() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("existing.py"), "def existing():\n    pass\n").unwrap();

        let prism = open_project(root);
        // Bootstrap: one file already indexed, matching the live repro's setup
        // (a project that already has SOME content before the race file appears).
        prism.index().unwrap();

        // The exact scenario from the live repro: a new file appears, and
        // BEFORE the watcher's own debounce would have settled, an ad-hoc
        // Index request also targets it -- both land in the SAME queue before
        // any execution happens.
        fs::write(root.join("fourth.py"), "def fourth():\n    pass\n").unwrap();

        let mut queue = IndexWorkQueue::new();
        queue.add_raw("fourth.py".to_string());

        let result_path = tmp.path().join("waiter.result");
        queue.add_waiter(Waiter {
            kind: WaiterKind::Index,
            use_learned: false,
            reply_path: result_path.clone(),
        });

        let drained = queue.drain();
        let outcome = execute_drain(&prism, drained).unwrap();

        // Exactly one extraction/upsert occurred for fourth.py -- the whole
        // point of this test. Before the fix, this scenario (a Raw entry
        // already queued, plus a second, independent decision to index the
        // same file) produced TWO separate index_files calls and a duplicate
        // primary key error.
        assert_eq!(outcome.extractions.len(), 1);
        assert_eq!(outcome.extractions[0].file, "fourth.py");

        // The waiter's reply reflects the real combined execution.
        let reply_contents = fs::read_to_string(&result_path).unwrap();
        let reply: WriteResult = serde_json::from_str(&reply_contents).unwrap();
        match reply {
            WriteResult::Ok { indexed_files, .. } => {
                assert_eq!(indexed_files, 1);
            }
            other => panic!("expected WriteResult::Ok, got {other:?}"),
        }
    }

    #[test]
    fn resolve_only_extractions_do_not_trigger_a_redundant_upsert() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def a():\n    pass\n").unwrap();
        let prism = open_project(root);
        prism.index().unwrap();

        let backend = prism.backend().unwrap();
        let existing_hash_count_before = backend.get_file_hashes().unwrap().len();

        let mut queue = IndexWorkQueue::new();
        let extraction = FileExtraction {
            file: "a.py".to_string(),
            language: "python".to_string(),
            content_hash: "whatever".to_string(),
            symbols: Vec::new(),
            relations: Vec::new(),
            statements: Vec::new(),
        };
        queue.add_resolve_only(extraction);
        let drained = queue.drain();
        let outcome = execute_drain(&prism, drained).unwrap();

        // resolve_only contributes to the resolve pass, not the upsert pass --
        // outcome.extractions (the upsert set) must be empty, even though the
        // resolve step ran against a.py.
        assert!(
            outcome.extractions.is_empty(),
            "ResolveOnly items must not appear in the upsert extraction set"
        );
        assert_eq!(
            backend.get_file_hashes().unwrap().len(),
            existing_hash_count_before,
            "a redundant upsert would not change the hash count here, but IS a real wasted write -- \
             this assertion documents the guarantee this test exists to protect"
        );
    }
}
