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
                if event.paths.iter().any(|p| is_document_file(p)) {
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

        let (inner_stop_tx, inner_stop_rx) = mpsc::channel::<()>();
        let shutdown_for_detacher = Arc::clone(&shutdown);
        let docs_kuzu_for_detacher = docs_kuzu.clone();
        let stop_sentinel_for_detacher = stop_sentinel.clone();

        // Runs concurrently with the blocking watch_docs call below, and
        // signals it to return once any detach/shutdown condition is met.
        let detacher = std::thread::spawn(move || -> bool {
            loop {
                if shutdown_for_detacher.load(Ordering::Relaxed) {
                    let _ = inner_stop_tx.send(());
                    return false;
                }
                if stop_sentinel_for_detacher.exists() {
                    let _ = std::fs::remove_file(&stop_sentinel_for_detacher);
                    let _ = inner_stop_tx.send(());
                    return true;
                }
                if !docs_kuzu_for_detacher.exists() {
                    let _ = inner_stop_tx.send(());
                    return false;
                }
                std::thread::sleep(poll);
            }
        });

        eprintln!(
            "[doc-watch-daemon] attaching doc watcher for {}",
            root.display()
        );
        if let Err(e) = watch_docs(root, debounce_ms, inner_stop_rx, "doc-watch-daemon") {
            eprintln!("[doc-watch-daemon] watch_docs error: {e}");
        }

        suppressed_until_absent = detacher.join().unwrap_or(false);
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
}
