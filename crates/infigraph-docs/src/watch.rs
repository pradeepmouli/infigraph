use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::{Config, RecursiveMode, Watcher};

use crate::{is_document_file, DocIndex};

pub fn watch_docs(
    root: &Path,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    log_prefix: &str,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let config = Config::default().with_poll_interval(Duration::from_millis(debounce_ms));
    let mut watcher = notify::RecommendedWatcher::new(tx, config)?;
    watcher.watch(root, RecursiveMode::Recursive)?;

    // notify's recursive watch has no directory exclusion of its own -- it
    // subscribes to the entire tree, `.infigraph`/`target`/`node_modules`
    // included. `is_document_file` alone isn't enough of a filter: a build
    // tool continuously regenerating e.g. `target/doc/*.html` or coverage
    // `*.xml`/`*.svg` output matches it on every write, setting `pending`
    // forever even though `collect_doc_files` (which respects the same
    // ignore rules) will never actually index anything under `target/` --
    // observed live as an infinite "reindexed: 0 files, 0 chunks" loop.
    let ignore_matcher = infigraph_core::ignore_rules::IgnoreMatcher::build(root);

    let debounce = Duration::from_millis(debounce_ms);
    let mut last_reindex = Instant::now();
    let mut pending = false;

    loop {
        if stop_rx.try_recv().is_ok() {
            eprintln!("[{log_prefix}] stopped");
            break;
        }

        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(event)) => {
                if event
                    .paths
                    .iter()
                    .any(|p| is_document_file(p) && !ignore_matcher.is_ignored(p, false))
                {
                    pending = true;
                }
            }
            Ok(Err(e)) => eprintln!("[{log_prefix}] watch error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if pending && last_reindex.elapsed() >= debounce {
            eprintln!("[{log_prefix}] document change detected, reindexing...");
            let mut idx = match DocIndex::open(root) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("[{log_prefix}] open error: {e}");
                    pending = false;
                    continue;
                }
            };
            if let Err(e) = idx.init() {
                eprintln!("[{log_prefix}] init error: {e}");
            } else {
                let indexed = match idx.index() {
                    Ok(r) => {
                        eprintln!(
                            "[{log_prefix}] reindexed: {} files, {} chunks",
                            r.indexed_files, r.total_chunks
                        );
                        true
                    }
                    Err(e) => {
                        eprintln!("[{log_prefix}] index error: {e}");
                        false
                    }
                };
                drop(idx);
                if indexed {
                    match crate::combined::schedule_group_doc_refresh(root) {
                        Ok(count) if count > 0 => {
                            eprintln!(
                                "[{log_prefix}] refreshing {count} combined document group(s)"
                            )
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("[{log_prefix}] combined refresh error: {e}"),
                    }
                }
            }
            pending = false;
            last_reindex = Instant::now();
        }
    }

    Ok(())
}

/// How often the daemon loop polls for `.infigraph/docs.kuzu`'s existence
/// and the per-handler stop sentinel while deciding whether to attach or
/// detach a `watch_docs` session. Overridable via
/// `INFIGRAPH_DOC_DAEMON_POLL_MS` so tests don't wait through a real 1s tick.
fn attach_poll_interval() -> Duration {
    std::env::var("INFIGRAPH_DOC_DAEMON_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(1000))
}

/// Drive doc-watching for `root` as part of a merged code+doc watch daemon
/// (see `infigraph_core::watch::daemon`). Dynamically attaches (starts a
/// `watch_docs` session) once `.infigraph/docs.kuzu` exists, detaches
/// (stops it) if that file disappears (e.g. after `clean_docs`) -- eligible
/// to re-attach once it reappears -- or if `.infigraph/watch.stop.docs` is
/// found -- NOT eligible to re-attach until docs.kuzu disappears and
/// reappears, since that sentinel represents an explicit stop request, not
/// an index-lifecycle event. Exits once `shutdown` is observed true.
/// Blocks until then.
pub fn watch_docs_daemon_loop(
    root: &Path,
    debounce_ms: u64,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let docs_kuzu = root.join(".infigraph").join("docs.kuzu");
    let stop_sentinel = root.join(".infigraph").join("watch.stop.docs");
    let poll = attach_poll_interval();

    let mut suppressed_until_absent = false;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }

        let exists = docs_kuzu.exists();

        if suppressed_until_absent {
            if !exists {
                suppressed_until_absent = false;
            }
            std::thread::sleep(poll);
            continue;
        }

        if !exists {
            std::thread::sleep(poll);
            continue;
        }

        let root_owned = root.to_path_buf();
        eprintln!(
            "[doc-watch-daemon] attaching doc watcher for {}",
            root.display()
        );
        suppressed_until_absent = run_attached_cycle(
            &docs_kuzu,
            &stop_sentinel,
            &shutdown,
            poll,
            move |stop_rx| watch_docs(&root_owned, debounce_ms, stop_rx, "doc-watch-daemon"),
        );
    }
}

/// Drives one attach cycle: runs `watch_fn` (normally a `watch_docs` call) on
/// its own thread and polls, in the CALLING thread, for whichever trips
/// first: `watch_fn` finishing on its own (unrequested), `shutdown`, the
/// stop sentinel, or `docs_kuzu` disappearing.
///
/// `handle.is_finished()` is checked before any of the other conditions on
/// every tick specifically so this function can never block forever: those
/// other conditions are things `watch_fn` reacts to via `stop_rx`, so once
/// `watch_fn` has already returned (e.g. `notify`'s sender was dropped, or
/// the watcher failed to start), none of them will necessarily ever become
/// true, and this function must notice that exit directly instead of
/// waiting on a stop signal nothing will act on.
///
/// Returns whether this was a "sticky" detach (explicit stop sentinel,
/// which suppresses re-attachment until `docs_kuzu` disappears and
/// reappears) as opposed to any other exit reason.
fn run_attached_cycle<F>(
    docs_kuzu: &Path,
    stop_sentinel: &Path,
    shutdown: &Arc<AtomicBool>,
    poll: Duration,
    watch_fn: F,
) -> bool
where
    F: FnOnce(mpsc::Receiver<()>) -> Result<()> + Send + 'static,
{
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || watch_fn(stop_rx));

    let log_join_result = |res: std::thread::Result<Result<()>>| match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("[doc-watch-daemon] watch_docs error: {e}"),
        Err(_) => eprintln!("[doc-watch-daemon] watch_docs thread panicked"),
    };

    loop {
        if handle.is_finished() {
            log_join_result(handle.join());
            eprintln!("[doc-watch-daemon] watch_docs exited unexpectedly, retrying");
            return false;
        }

        if shutdown.load(Ordering::Relaxed) {
            let _ = stop_tx.send(());
            log_join_result(handle.join());
            return false;
        }

        if stop_sentinel.exists() {
            let _ = std::fs::remove_file(stop_sentinel);
            let _ = stop_tx.send(());
            log_join_result(handle.join());
            return true;
        }

        if !docs_kuzu.exists() {
            let _ = stop_tx.send(());
            log_join_result(handle.join());
            return false;
        }

        std::thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    fn set_fast_poll() {
        std::env::set_var("INFIGRAPH_DOC_DAEMON_POLL_MS", "20");
    }

    fn clear_fast_poll() {
        std::env::remove_var("INFIGRAPH_DOC_DAEMON_POLL_MS");
    }

    /// `DocIndex` has no `chunk_count()` accessor; the real path is
    /// `DocIndex::store()` (populated only after `init()`) -> `DocBackend::stats()`
    /// -> `DocStoreStats::chunk_count`. Any failure along that path (e.g.
    /// transient contention with a concurrently-writing daemon thread) is
    /// treated as 0, matching what a hypothetical fallible `chunk_count()`
    /// accessor would collapse to.
    ///
    /// This helper opens a second, independent `DocIndex`/`DocStore` handle
    /// against the same `docs.kuzu` while the daemon thread under test may
    /// concurrently be mid-`open`/`init`/`index`/`drop` on its own handle.
    /// That's safe: `DocStore::open` (see `store.rs`) takes a process-wide
    /// `static DB_LOCK: Mutex<()>` and holds the guard for the `DocStore`'s
    /// whole lifetime, so two `DocStore::open` calls in this test binary
    /// cannot run concurrently -- the second simply blocks on `Mutex::lock()`
    /// until the first is dropped, it does not error out. Since
    /// `DocIndex::init()`'s wipe-and-rebuild-on-failure path only triggers on
    /// an actual `Err` from `DocStore::open`, ordinary lock contention here
    /// cannot spuriously trip it.
    fn chunk_count(root: &Path) -> usize {
        let mut idx = match crate::DocIndex::open(root) {
            Ok(i) => i,
            Err(_) => return 0,
        };
        if idx.init().is_err() {
            return 0;
        }
        idx.store()
            .and_then(|s| s.stats().ok())
            .map(|s| s.chunk_count)
            .unwrap_or(0)
    }

    #[test]
    fn returns_immediately_when_shutdown_already_set() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let shutdown = Arc::new(AtomicBool::new(true));
        watch_docs_daemon_loop(&root, 50, shutdown).unwrap();
        // No assertion beyond "returned" -- this test times out (fails) if
        // the loop doesn't check shutdown before ever attaching.
    }

    #[test]
    fn does_not_attach_without_docs_kuzu() {
        set_fast_poll();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || watch_docs_daemon_loop(&root, 50, shutdown_clone));

        std::thread::sleep(Duration::from_millis(150));
        // No docs.kuzu ever appeared -- the loop must still be polling, not
        // stuck in an attached watch_docs call. Shutting down must return
        // promptly (proves it was in the poll loop, not blocked inside
        // watch_docs's own internal loop, which only checks its stop_rx on
        // its own ~500ms cadence and would still return promptly here too --
        // the real proof is the next test, which asserts actual indexing).
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.join().unwrap().unwrap();
        clear_fast_poll();
    }

    #[test]
    fn attaches_and_indexes_once_docs_kuzu_appears() {
        set_fast_poll();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let root_clone = root.clone();
        let handle =
            std::thread::spawn(move || watch_docs_daemon_loop(&root_clone, 50, shutdown_clone));

        // Not indexed yet -- give the poll loop a couple of ticks doing
        // nothing, then create a real (empty) doc index so docs.kuzu exists.
        std::thread::sleep(Duration::from_millis(60));
        crate::DocIndex::open(&root).unwrap().init().unwrap();
        assert!(root.join(".infigraph").join("docs.kuzu").exists());

        // Give the daemon loop time to notice and attach, then write a doc.
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(root.join("readme.md"), "# hello\n\nsome content").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut chunks = 0;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            chunks = chunk_count(&root);
            if chunks > 0 {
                break;
            }
        }
        assert!(
            chunks > 0,
            "doc daemon loop must have attached and indexed readme.md"
        );

        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.join().unwrap().unwrap();
        clear_fast_poll();
    }

    /// Regression test for a real infinite-loop bug (#52): `notify`'s
    /// recursive watch has no directory exclusion of its own, so a
    /// document-extension file written under a safety-excluded directory
    /// (e.g. `target/doc/*.html` from `cargo doc`, coverage `*.xml`/`*.svg`
    /// output, etc.) used to satisfy `is_document_file` alone and set
    /// `pending` just like a real doc change -- except `collect_doc_files`
    /// (which respects the same ignore rules) never actually indexes
    /// anything under `target/`, so the reindex always finds "0 files, 0
    /// chunks" and the next write under `target/` sets `pending` again,
    /// forever. Tests the exact combined predicate `watch_docs`'s event
    /// filter uses directly, rather than through the full threaded
    /// daemon+FSEvents path -- a build artifact under `target/` is excluded
    /// from indexing either way, so observing `chunk_count` alone can't
    /// distinguish "reindex was wrongly attempted" from "correctly
    /// skipped."
    #[test]
    fn document_change_filter_excludes_safety_listed_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("target").join("doc")).unwrap();
        std::fs::write(root.join("readme.md"), "# hello\n").unwrap();

        let ignore_matcher = infigraph_core::ignore_rules::IgnoreMatcher::build(&root);
        let real_doc = root.join("readme.md");
        let build_artifact = root.join("target").join("doc").join("index.html");

        assert!(
            is_document_file(&real_doc) && !ignore_matcher.is_ignored(&real_doc, false),
            "a real doc outside any excluded directory must still set pending"
        );
        assert!(
            is_document_file(&build_artifact),
            "sanity check: the build artifact's extension alone looks document-shaped"
        );
        assert!(
            ignore_matcher.is_ignored(&build_artifact, false),
            "a file under target/ must be recognized as safety-excluded"
        );
        assert!(
            !(is_document_file(&build_artifact)
                && !ignore_matcher.is_ignored(&build_artifact, false)),
            "watch_docs's combined filter must reject a build artifact under target/, \
             not just check its extension"
        );
    }

    #[test]
    fn detaches_on_stop_sentinel_and_does_not_immediately_reattach() {
        set_fast_poll();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        crate::DocIndex::open(&root).unwrap().init().unwrap();

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let root_clone = root.clone();
        let handle =
            std::thread::spawn(move || watch_docs_daemon_loop(&root_clone, 50, shutdown_clone));

        // Let it attach.
        std::thread::sleep(Duration::from_millis(100));

        // Request an explicit detach.
        std::fs::write(root.join(".infigraph").join("watch.stop.docs"), b"").unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !root.join(".infigraph").join("watch.stop.docs").exists(),
            "sentinel must be consumed (removed) once acted on"
        );

        // Write a NEW doc while suppressed -- must NOT be indexed, proving
        // the loop stayed detached instead of immediately re-attaching
        // (docs.kuzu still exists, so a naive re-poll would re-attach).
        std::fs::write(root.join("after-stop.md"), "# should not be indexed yet").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let chunks_while_suppressed = chunk_count(&root);
        assert_eq!(
            chunks_while_suppressed, 0,
            "must stay detached after an explicit stop until docs.kuzu disappears and reappears"
        );

        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.join().unwrap().unwrap();
        clear_fast_poll();
    }

    #[test]
    fn run_attached_cycle_reports_non_sticky_when_watch_fn_exits_unrequested() {
        // Proves the fix for Finding 1: if the watch invocation returns on
        // its own (e.g. `watch_docs` seeing its internal channel disconnect,
        // or erroring out before its loop ever starts) -- none of
        // shutdown/stop_sentinel/docs_kuzu-absent -- `run_attached_cycle`
        // must still return promptly instead of blocking on a stop signal
        // the exited thread can no longer act on.
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let docs_kuzu = root.join("docs.kuzu");
            let stop_sentinel = root.join("watch.stop.docs");
            let shutdown = Arc::new(AtomicBool::new(false));

            let sticky = run_attached_cycle(
                &docs_kuzu,
                &stop_sentinel,
                &shutdown,
                Duration::from_millis(10),
                |_stop_rx: mpsc::Receiver<()>| -> Result<()> { Ok(()) },
            );
            let _ = result_tx.send(sticky);
        });

        let sticky = result_rx.recv_timeout(Duration::from_secs(2)).expect(
            "run_attached_cycle must return promptly when watch_fn exits unrequested, \
             not block waiting on a stop condition nothing will ever trip",
        );
        assert!(
            !sticky,
            "an unrequested watch_fn exit must not be reported as a sticky (explicit-stop) detach"
        );
    }
}
