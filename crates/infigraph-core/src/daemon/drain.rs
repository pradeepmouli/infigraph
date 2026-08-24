use crate::daemon::queue::{DrainedQueue, PendingIndexItem, WaiterKind};
use crate::daemon_protocol::{write_atomic, WriteResult};
use crate::model::FileExtraction;
use crate::resolve::ResolveStats;
use crate::Infigraph;
use anyhow::Result;
use std::path::Path;

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
            // isn't already covered. `scan_changed_files` already parsed
            // this file -- keep that parse (push into `pre_parsed`) instead
            // of discarding it down to just `.file` and re-parsing it from
            // disk via `extract_paths` below.
            if !pre_parsed.iter().any(|e| e.file == extraction.file)
                && !to_extract.contains(&extraction.file)
            {
                pre_parsed.push(extraction);
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
    let extractions_count = extractions.len();

    let mut removals: Vec<String> = drained.removals.into_iter().collect();
    removals.extend(whole_project_stale);
    for path in &removals {
        let _ = backend.remove_file(path);
    }
    // A removal that came from a real filesystem event, where whether the
    // now-gone path was a file or a directory can no longer be determined --
    // also scan for and remove anything still in the graph under it as a
    // directory prefix. A no-op query for a path that was actually a file.
    for prefix in &drained.removal_prefixes {
        let _ = infigraph.remove_files_by_prefix(Path::new(prefix));
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
    // Moved into the resolve set (rather than cloned) to avoid a full deep
    // copy of every symbol/relation/statement in the batch -- the
    // upsert-only view (returned in `DrainOutcome`, and used for the
    // Index/UpsertFilesBulk waiter replies below) is restored afterward by
    // truncating off the `resolve_only` tail, since nothing else needs
    // those elements once `resolve_calls` has run.
    extractions.extend(resolve_only);
    let resolve_stats = backend
        .resolve_calls(&extractions, learned.as_ref())
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
    extractions.truncate(extractions_count);

    for waiter in &drained.waiters {
        // Scoped to the waiter's own requested paths when it named any --
        // otherwise a targeted request (e.g. a single-file `Index`) would
        // report a count that includes unrelated concurrent work folded
        // into the same drain. `paths: None` means the waiter is inherently
        // whole-batch scoped (whole-project `Index`), for which the
        // batch-wide count below is the correct answer.
        let result = match waiter.kind {
            WaiterKind::Index | WaiterKind::UpsertFilesBulk => {
                let count = match &waiter.paths {
                    Some(paths) => extractions
                        .iter()
                        .filter(|&e| paths.contains(&e.file))
                        .count(),
                    None => extractions.len(),
                };
                WriteResult::Ok {
                    total_files: count,
                    indexed_files: count,
                }
            }
            WaiterKind::RemoveFiles => {
                let count = match &waiter.paths {
                    Some(paths) => removals.iter().filter(|&r| paths.contains(r)).count(),
                    None => removals.len(),
                };
                WriteResult::Ok {
                    total_files: count,
                    indexed_files: count,
                }
            }
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
    use crate::daemon::queue::{IndexWorkQueue, Waiter};
    use crate::lang::{LanguagePack, LanguageRegistry};
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

    /// Drives a `FullReindex` request synchronously (no real daemon loop
    /// needed), against a real temp-dir project: `try_start_full_reindex`
    /// (loop-thread gating + spawning the background build) followed by
    /// `finish_full_reindex` (loop-thread swap), exactly like a real
    /// `run_write_coordinator` tick's reap block, minus the loop
    /// itself. Replaces the old direct call to `serve_full_reindex_request`,
    /// which ran everything inline on one thread and no longer exists now
    /// that the build runs on `drain_rt`.
    fn drive_full_reindex_sync<MR>(
        root: &std::path::Path,
        request_path: &std::path::Path,
        queue: &std::sync::Arc<std::sync::Mutex<crate::daemon::queue::IndexWorkQueue>>,
        make_registry: &MR,
        held: &mut Option<std::sync::Arc<crate::Infigraph>>,
    ) where
        MR: Fn() -> Result<crate::lang::LanguageRegistry>,
    {
        let drain_rt = tokio::runtime::Runtime::new().unwrap();
        // Fresh, unparented token: none of this file's tests exercise
        // cancellation, so there's no daemon-lifetime token to thread
        // through this synchronous test helper.
        let daemon_token = tokio_util::sync::CancellationToken::new();
        if let Some(in_flight) = crate::daemon::try_start_full_reindex(
            root,
            request_path,
            queue,
            make_registry,
            false,
            false,
            &drain_rt,
            &daemon_token,
        ) {
            let registry = std::sync::Arc::new(make_registry().unwrap());
            let (guard, _) = crate::daemon::finish_full_reindex(
                root,
                &in_flight.reply_path,
                &registry,
                held,
                drain_rt.block_on(in_flight.task.join()),
            );
            std::fs::remove_file(&in_flight.request_path).ok();
            drop(guard);
        }
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
            paths: None,
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

    /// Drives a `FullReindex` request directly (no real daemon process)
    /// against a real temp-dir project: seeds a graph, submits FullReindex,
    /// confirms the old content is genuinely gone and the graph is rebuilt,
    /// and confirms the old graph directory was quarantined (renamed aside),
    /// not deleted.
    #[test]
    fn full_reindex_rebuilds_the_graph_and_quarantines_the_old_one() {
        use crate::graph::GraphBackend;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("old.py"), "def old_symbol():\n    pass\n").unwrap();

        let prism = open_project(root);
        prism.index().unwrap();

        let old_graph_path = root.join(".infigraph").join("graph");
        assert!(old_graph_path.exists());

        // Simulate what changed between the bootstrap index and the full
        // reindex request: old.py is replaced by new.py, matching a real
        // "the codebase moved on" scenario a full reindex is meant to catch.
        fs::remove_file(root.join("old.py")).unwrap();
        fs::write(root.join("new.py"), "def new_symbol():\n    pass\n").unwrap();

        let queue = std::sync::Arc::new(std::sync::Mutex::new(
            crate::daemon::queue::IndexWorkQueue::new(),
        ));
        let mut held: Option<std::sync::Arc<crate::Infigraph>> = None;
        let make_registry = || Ok(python_registry());

        let request_path = root.join(".infigraph").join("fullreindex.request");
        fs::write(
            &request_path,
            serde_json::to_string(&crate::daemon_protocol::WriteRequest::FullReindex).unwrap(),
        )
        .unwrap();

        drive_full_reindex_sync(root, &request_path, &queue, &make_registry, &mut held);

        let reply_path = request_path.with_extension("result");
        let reply: crate::daemon_protocol::WriteResult =
            serde_json::from_str(&fs::read_to_string(&reply_path).unwrap()).unwrap();
        match reply {
            crate::daemon_protocol::WriteResult::FullReindexOk { indexed_files, .. } => {
                assert_eq!(
                    indexed_files, 1,
                    "only new.py should be in the rebuilt graph"
                );
            }
            other => panic!("expected WriteResult::FullReindexOk, got {other:?}"),
        }

        // Verify against a completely fresh read connection, not the
        // (now-poisoned) `held` handle from before the swap.
        let verify = crate::graph::KuzuBackend::open_read_only(&old_graph_path).unwrap();
        let rows = verify.raw_query("MATCH (s:Symbol) RETURN s.name").unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert!(
            names.contains(&"new_symbol"),
            "rebuilt graph must contain the new symbol"
        );
        assert!(
            !names.contains(&"old_symbol"),
            "rebuilt graph must not contain the old symbol"
        );

        let aside: Vec<String> = fs::read_dir(root.join(".infigraph"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("graph.previous.") || n.starts_with("graph.corrupt."))
            .collect();
        // Count only BASE pool entries ("graph.previous.<ts>", all digits
        // after the infix): the retire also relocates WAL-family siblings
        // sharing the stem ("graph.previous.<ts>.wal"), which are part of
        // the same single retired entry, not extra entries.
        let base_entries: Vec<&String> = aside
            .iter()
            .filter(|n| {
                n.strip_prefix("graph.previous.")
                    .or_else(|| n.strip_prefix("graph.corrupt."))
                    .is_some_and(|rest| {
                        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
                    })
            })
            .collect();
        assert_eq!(
            base_entries.len(),
            1,
            "the old graph must be moved aside (renamed), not deleted: {aside:?}"
        );
        assert!(
            aside.iter().all(|n| n.starts_with("graph.previous.")),
            "a healthy superseded graph (and its WAL siblings) must go to the rollback \
             pool, not the corruption-evidence pool: {aside:?}"
        );
    }

    /// Regression coverage for the WAL-family half of the final review's C1:
    /// a rebuild interrupted by a SIGKILL can leave `graph.rebuilding.wal*`
    /// behind with its own (now dead) database ID. The stale-leftover cleanup
    /// used to be gated on the *base image* existing and only removed that,
    /// so a surviving WAL sibling was never cleaned -- and a fresh
    /// `graph.rebuilding` created beside it is permanently unopenable
    /// ("Database ID ... does not match"), wedging every future full reindex
    /// with no self-healing path. This test seeds exactly that state: a WAL
    /// sibling with NO base image, which is what the old guard skipped.
    #[test]
    fn full_reindex_clears_a_stale_rebuilding_wal_left_by_an_interrupted_run() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def a():\n    pass\n").unwrap();

        let prism = open_project(root);
        prism.index().unwrap();

        let infigraph_dir = root.join(".infigraph");
        let stale_wal = infigraph_dir.join("graph.rebuilding.wal");
        let stale_checkpoint = infigraph_dir.join("graph.rebuilding.wal.checkpoint");
        fs::write(&stale_wal, b"a dead database's write-ahead log").unwrap();
        fs::write(&stale_checkpoint, b"and its checkpoint temp file").unwrap();
        assert!(
            !infigraph_dir.join("graph.rebuilding").exists(),
            "the interesting case is a WAL sibling with no base image"
        );

        let queue = std::sync::Arc::new(std::sync::Mutex::new(
            crate::daemon::queue::IndexWorkQueue::new(),
        ));
        let mut held: Option<std::sync::Arc<crate::Infigraph>> = None;
        let make_registry = || Ok(python_registry());

        let request_path = infigraph_dir.join("fullreindex.request");
        fs::write(
            &request_path,
            serde_json::to_string(&crate::daemon_protocol::WriteRequest::FullReindex).unwrap(),
        )
        .unwrap();

        drive_full_reindex_sync(root, &request_path, &queue, &make_registry, &mut held);

        let reply: crate::daemon_protocol::WriteResult = serde_json::from_str(
            &fs::read_to_string(request_path.with_extension("result")).unwrap(),
        )
        .unwrap();
        assert!(
            matches!(
                reply,
                crate::daemon_protocol::WriteResult::FullReindexOk { .. }
            ),
            "a stale WAL leftover must not wedge the reindex, got {reply:?}"
        );
        assert!(
            !stale_wal.exists() && !stale_checkpoint.exists(),
            "the stale WAL family must be cleaned up, not left for the next run to inherit"
        );
    }

    /// Regression coverage for the final review's I2: the swap replaces the
    /// graph wholesale, so TESTED_BY edges the live graph held are destroyed.
    /// Nothing downstream restores them -- the CLI's own derivation is gated
    /// on `indexed_files > 0`, and after a full rebuild every file hash
    /// matches, so the next ordinary index skips it too. The daemon must
    /// therefore re-derive them itself before swapping.
    #[test]
    fn full_reindex_rederives_tested_by_edges_the_swap_would_otherwise_destroy() {
        use crate::graph::GraphBackend;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // `test_`-prefixed function in a `test_`-named file is what marks a
        // symbol as kind `Test` (see `extract::entities`), and TESTED_BY is
        // derived from a Test symbol's CALLS edges.
        fs::write(root.join("lib.py"), "def target():\n    pass\n").unwrap();
        fs::write(
            root.join("test_lib.py"),
            "def test_target():\n    target()\n",
        )
        .unwrap();

        let prism = open_project(root);
        prism.index().unwrap();

        let queue = std::sync::Arc::new(std::sync::Mutex::new(
            crate::daemon::queue::IndexWorkQueue::new(),
        ));
        let mut held: Option<std::sync::Arc<crate::Infigraph>> = None;
        let make_registry = || Ok(python_registry());

        let request_path = root.join(".infigraph").join("fullreindex.request");
        fs::write(
            &request_path,
            serde_json::to_string(&crate::daemon_protocol::WriteRequest::FullReindex).unwrap(),
        )
        .unwrap();

        drive_full_reindex_sync(root, &request_path, &queue, &make_registry, &mut held);

        let verify =
            crate::graph::KuzuBackend::open_read_only(&root.join(".infigraph").join("graph"))
                .unwrap();
        let rows = verify
            .raw_query("MATCH (s:Symbol)-[:TESTED_BY]->(t:Symbol) RETURN s.name, t.name")
            .unwrap();
        assert!(
            !rows.is_empty(),
            "the rebuilt graph must carry TESTED_BY edges; without re-derivation \
             the swap silently drops them for good"
        );
    }

    /// Regression coverage for the registry-build failure branch of
    /// `try_start_full_reindex` (the `make_registry()` early return in
    /// `watch/mod.rs`): a `FullReindex` request whose language
    /// registry fails to build must reply with an explicit error and leave
    /// the live graph completely untouched -- no partial rebuild, no
    /// `graph.rebuilding` left behind. This is the property build-fresh-
    /// then-swap exists to guarantee; the two tests above only exercise the
    /// success path, so without this test that guarantee was unverified
    /// anywhere in the codebase.
    #[test]
    fn full_reindex_replies_with_an_error_and_leaves_the_old_graph_untouched_when_the_registry_build_fails(
    ) {
        use crate::graph::GraphBackend;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("old.py"), "def old_symbol():\n    pass\n").unwrap();

        let prism = open_project(root);
        prism.index().unwrap();

        let live_graph_path = root.join(".infigraph").join("graph");
        assert!(live_graph_path.exists());

        let queue = std::sync::Arc::new(std::sync::Mutex::new(
            crate::daemon::queue::IndexWorkQueue::new(),
        ));
        let mut held: Option<std::sync::Arc<crate::Infigraph>> = None;
        let make_registry = || Err(anyhow::anyhow!("injected registry failure"));

        let request_path = root.join(".infigraph").join("fullreindex.request");
        fs::write(
            &request_path,
            serde_json::to_string(&crate::daemon_protocol::WriteRequest::FullReindex).unwrap(),
        )
        .unwrap();

        drive_full_reindex_sync(root, &request_path, &queue, &make_registry, &mut held);

        let reply_path = request_path.with_extension("result");
        let reply: crate::daemon_protocol::WriteResult =
            serde_json::from_str(&fs::read_to_string(&reply_path).unwrap()).unwrap();
        match reply {
            crate::daemon_protocol::WriteResult::Err { message } => {
                assert!(
                    message.contains("full reindex failed"),
                    "expected a full-reindex failure message, got: {message}"
                );
            }
            other => panic!("expected WriteResult::Err, got {other:?}"),
        }

        // Verify against a completely fresh read connection, matching the
        // success-path test's own approach: the original content must
        // still be there, untouched by the failed attempt.
        let verify = crate::graph::KuzuBackend::open_read_only(&live_graph_path).unwrap();
        let rows = verify.raw_query("MATCH (s:Symbol) RETURN s.name").unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert!(
            names.contains(&"old_symbol"),
            "a registry-build failure must leave the live graph's original content untouched"
        );

        let rebuilding_path = root.join(".infigraph").join("graph.rebuilding");
        assert!(
            !rebuilding_path.exists(),
            "a registry-build failure must not leave graph.rebuilding behind"
        );
    }

    /// Regression coverage for the "superseded waiters" property of
    /// `try_start_full_reindex`: a request only *queued* (not yet
    /// executing) when `FullReindex` arrives is genuinely moot -- the full
    /// reindex is about to re-scan every file from disk regardless of what
    /// was pending -- but its waiter must still be answered, not left
    /// hanging until its own timeout. Without this test the drain-and-reply
    /// loop that implements this could regress (e.g. an early return before
    /// answering waiters, or draining without replying) and nothing would
    /// fail except a client silently hanging in production.
    #[test]
    fn full_reindex_answers_a_superseded_waiter_with_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def a():\n    pass\n").unwrap();

        let prism = open_project(root);
        prism.index().unwrap();

        let queue = std::sync::Arc::new(std::sync::Mutex::new(
            crate::daemon::queue::IndexWorkQueue::new(),
        ));
        // A request queued but not yet executing when FullReindex arrives.
        let superseded_reply = root.join(".infigraph").join("superseded.result");
        queue.lock().unwrap().add_waiter(Waiter {
            kind: WaiterKind::Index,
            use_learned: false,
            reply_path: superseded_reply.clone(),
            paths: None,
        });

        let mut held: Option<std::sync::Arc<crate::Infigraph>> = None;
        let make_registry = || Ok(python_registry());

        let request_path = root.join(".infigraph").join("fullreindex.request");
        fs::write(
            &request_path,
            serde_json::to_string(&crate::daemon_protocol::WriteRequest::FullReindex).unwrap(),
        )
        .unwrap();

        drive_full_reindex_sync(root, &request_path, &queue, &make_registry, &mut held);

        let reply: crate::daemon_protocol::WriteResult =
            serde_json::from_str(&fs::read_to_string(&superseded_reply).unwrap()).unwrap();
        match reply {
            crate::daemon_protocol::WriteResult::Err { message } => {
                assert!(
                    message.contains("superseded"),
                    "a superseded waiter's reply must explain why, got: {message}"
                );
            }
            other => panic!("expected WriteResult::Err, got {other:?}"),
        }
    }

    /// Regression test for a final-review Critical finding: the watch loop's
    /// `WatchEventKind::Removed` handling used to call both `remove_file`
    /// (the exact path) and `remove_files_by_prefix` (anything nested under
    /// it, for directory removal) directly and unlocked. Coalescing removal
    /// into the queue (`add_removal`) kept only the exact-path removal --
    /// deleting a whole directory left every file that had been under it
    /// stranded in the graph forever, since the real daemon runs with no
    /// periodic whole-project scan to eventually notice the drift.
    /// `add_watch_removal` (used only for genuine filesystem removal events)
    /// restores the directory-prefix scan/removal alongside the exact-path
    /// removal.
    #[test]
    fn a_watch_removal_of_a_directory_removes_every_file_that_was_under_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.py"), "def a():\n    pass\n").unwrap();
        fs::write(root.join("sub/b.py"), "def b():\n    pass\n").unwrap();
        fs::write(root.join("top.py"), "def top():\n    pass\n").unwrap();

        let prism = open_project(root);
        prism.index().unwrap();

        let backend = prism.backend().unwrap();
        let hashes_before = backend.get_file_hashes().unwrap();
        assert!(hashes_before.contains_key("sub/a.py"));
        assert!(hashes_before.contains_key("sub/b.py"));
        assert!(hashes_before.contains_key("top.py"));

        // The directory is already gone from disk by the time a real
        // filesystem removal event would fire -- nothing left to inspect to
        // tell it apart from a single-file removal.
        fs::remove_dir_all(root.join("sub")).unwrap();

        let mut queue = IndexWorkQueue::new();
        queue.add_watch_removal("sub".to_string());
        let drained = queue.drain();
        let outcome = execute_drain(&prism, drained).unwrap();
        assert!(outcome.extractions.is_empty());

        let hashes_after = backend.get_file_hashes().unwrap();
        assert!(
            !hashes_after.contains_key("sub/a.py"),
            "a file under the deleted directory must be removed from the graph"
        );
        assert!(
            !hashes_after.contains_key("sub/b.py"),
            "a file under the deleted directory must be removed from the graph"
        );
        assert!(
            hashes_after.contains_key("top.py"),
            "a file outside the deleted directory must survive"
        );
    }

    /// Regression test for a final-review Important finding: every
    /// Index/UpsertFilesBulk waiter folded into a drain used to be told
    /// `extractions.len()` for the WHOLE merged batch, even when its own
    /// request named only one file -- so a targeted single-file `Index`
    /// request could report having indexed hundreds of files that were
    /// actually someone else's concurrent work riding along in the same
    /// drain. `Waiter::paths` lets a waiter's reply be scoped to what it
    /// actually asked for.
    #[test]
    fn a_targeted_waiter_reports_a_count_scoped_to_its_own_request_not_the_whole_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.py"), "def a():\n    pass\n").unwrap();
        fs::write(root.join("b.py"), "def b():\n    pass\n").unwrap();
        let prism = open_project(root);
        prism.index().unwrap();

        // Both files changed since the last index -- both will be
        // re-extracted by this one combined drain.
        fs::write(
            root.join("a.py"),
            "def a():\n    pass\n\ndef a2():\n    pass\n",
        )
        .unwrap();
        fs::write(
            root.join("b.py"),
            "def b():\n    pass\n\ndef b2():\n    pass\n",
        )
        .unwrap();

        let mut queue = IndexWorkQueue::new();
        queue.add_raw("a.py".to_string());
        queue.add_raw("b.py".to_string());

        // A targeted waiter that only ever asked about a.py.
        let targeted_reply = tmp.path().join("targeted.result");
        queue.add_waiter(Waiter {
            kind: WaiterKind::Index,
            use_learned: false,
            reply_path: targeted_reply.clone(),
            paths: Some(vec!["a.py".to_string()]),
        });

        // A whole-project waiter, riding along in the same drain.
        let whole_project_reply = tmp.path().join("whole-project.result");
        queue.add_waiter(Waiter {
            kind: WaiterKind::Index,
            use_learned: false,
            reply_path: whole_project_reply.clone(),
            paths: None,
        });

        let drained = queue.drain();
        let outcome = execute_drain(&prism, drained).unwrap();
        assert_eq!(
            outcome.extractions.len(),
            2,
            "test setup is wrong: both a.py and b.py should have been re-extracted"
        );

        let targeted: WriteResult =
            serde_json::from_str(&fs::read_to_string(&targeted_reply).unwrap()).unwrap();
        match targeted {
            WriteResult::Ok { indexed_files, .. } => assert_eq!(
                indexed_files, 1,
                "a waiter that only asked about a.py must not be told about b.py's \
                 concurrent, unrelated work"
            ),
            other => panic!("expected WriteResult::Ok, got {other:?}"),
        }

        let whole_project: WriteResult =
            serde_json::from_str(&fs::read_to_string(&whole_project_reply).unwrap()).unwrap();
        match whole_project {
            WriteResult::Ok { indexed_files, .. } => assert_eq!(
                indexed_files, 2,
                "a paths: None waiter is inherently whole-batch scoped"
            ),
            other => panic!("expected WriteResult::Ok, got {other:?}"),
        }
    }
}
