use std::path::Path;
use std::sync::mpsc;
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
