use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::Infigraph;

/// A single file-change event emitted by the watcher.
#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub kind: WatchEventKind,
    pub path: PathBuf,
    /// True if this file has cross-file CALLS edges — full reindex needed to re-resolve them.
    pub has_cross_file_calls: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEventKind {
    Modified,
    Created,
    Removed,
}

impl std::fmt::Display for WatchEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            WatchEventKind::Modified => "modified",
            WatchEventKind::Created => "created",
            WatchEventKind::Removed => "removed",
        };
        if self.has_cross_file_calls {
            write!(f, "{kind}: {} [cross-file calls detected — full reindex recommended]", self.path.display())
        } else {
            write!(f, "{kind}: {}", self.path.display())
        }
    }
}

/// Watch a project directory and auto-reindex on file changes.
///
/// After reindexing a changed file, checks if it has cross-file CALLS edges.
/// If so, sets `WatchEvent.has_cross_file_calls = true` so the caller can
/// prompt the user to run a full reindex (to re-resolve dangling call targets).
///
/// Blocks until `stop_rx` receives a signal.
pub fn watch_project(
    prism: &Infigraph,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    on_event: impl Fn(WatchEvent) + Send + 'static,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

    let config = Config::default()
        .with_poll_interval(Duration::from_millis(debounce_ms));

    let mut watcher = RecommendedWatcher::new(tx, config)?;
    watcher.watch(prism.root(), RecursiveMode::Recursive)?;

    let ignore_dirs = [
        ".infigraph", ".git", "node_modules", "__pycache__",
        ".venv", "venv", "target", "build", "dist", ".tox",
    ];

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(event)) => {
                let watch_kind = match event.kind {
                    EventKind::Create(_) => WatchEventKind::Created,
                    EventKind::Modify(_) => WatchEventKind::Modified,
                    EventKind::Remove(_) => WatchEventKind::Removed,
                    _ => continue,
                };

                for path in event.paths {
                    if should_ignore(&path, &ignore_dirs) {
                        continue;
                    }

                    let rel = match path.strip_prefix(prism.root()) {
                        Ok(r) => r.to_string_lossy().replace('\\', "/"),
                        Err(_) => continue,
                    };

                    match watch_kind {
                        WatchEventKind::Removed => {
                            let _ = prism.remove_file(&path);
                            on_event(WatchEvent {
                                kind: watch_kind.clone(),
                                path,
                                has_cross_file_calls: false,
                            });
                        }
                        WatchEventKind::Created | WatchEventKind::Modified => {
                            if prism.registry().for_file(&rel).is_some() {
                                match prism.index_file(&path) {
                                    Ok(_) => {
                                        if let Some(store) = prism.store() {
                                            let changed = [rel.as_str()];
                                            if let Err(e) = crate::embed::update_embeddings(store, prism.root(), &changed) {
                                                eprintln!("watch: embedding update failed for {rel}: {e}");
                                            }
                                        }
                                        let cross = has_cross_file_calls(prism, &rel);
                                        on_event(WatchEvent {
                                            kind: watch_kind.clone(),
                                            path,
                                            has_cross_file_calls: cross,
                                        });
                                    }
                                    Err(e) => eprintln!("watch: reindex failed for {rel}: {e}"),
                                }
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => eprintln!("watch error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

/// Like `watch_project` but automatically runs a full reindex when cross-file call edges
/// are affected by a change, keeping call resolution accurate without user intervention.
/// `make_registry` is a factory fn (e.g. `infigraph_languages::bundled_registry`) to
/// create a fresh LanguageRegistry for the auto-reindex pass.
pub fn watch_project_auto_resolve(
    prism: &Infigraph,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    log_prefix: &str,
    make_registry: impl Fn() -> anyhow::Result<crate::lang::LanguageRegistry> + Send + 'static,
) -> Result<()> {
    let root = prism.root().to_path_buf();
    watch_project(prism, debounce_ms, stop_rx, {
        let prefix = log_prefix.to_string();
        move |evt: WatchEvent| {
            if evt.has_cross_file_calls {
                eprintln!("[watch {prefix}] {evt}");
                if let Ok(reg) = make_registry() {
                    if let Ok(mut p) = Infigraph::open(&root, reg) {
                        if p.init().is_ok() {
                            match p.index() {
                                Ok(r) => {
                                    eprintln!("[watch {prefix}] auto full reindex: {}/{} files", r.indexed_files, r.total_files);
                                    if let Some(store) = p.store() {
                                        let changed: Vec<&str> = r.extractions.iter().map(|e| e.file.as_str()).collect();
                                        match crate::embed::update_embeddings(store, &root, &changed) {
                                            Ok(n) => eprintln!("[watch {prefix}] updated {n} embeddings"),
                                            Err(e) => eprintln!("[watch {prefix}] embedding update failed: {e}"),
                                        }
                                    }
                                }
                                Err(e) => eprintln!("[watch {prefix}] auto reindex failed: {e}"),
                            }
                        }
                    }
                }
            } else {
                eprintln!("[watch {prefix}] {evt}");
            }
        }
    })
}

/// Returns true if the file has any resolved CALLS edges to/from symbols in other files.
/// These edges become stale when the file changes — a full reindex is needed to re-resolve.
fn has_cross_file_calls(prism: &Infigraph, rel_path: &str) -> bool {
    let store = match prism.store() {
        Some(s) => s,
        None => return false,
    };
    let conn = match store.connection() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let escaped = rel_path.replace('\'', "\\'");
    // Check outgoing cross-file calls (this file calls symbols in other files)
    let q = format!(
        "MATCH (a:Symbol)-[:CALLS]->(b:Symbol) WHERE a.file = '{escaped}' AND b.file <> '{escaped}' RETURN count(*) LIMIT 1"
    );
    if let Ok(mut result) = conn.query(&q) {
        if let Some(row) = result.next() {
            if let Some(val) = row.first() {
                if val.to_string().parse::<u64>().unwrap_or(0) > 0 {
                    return true;
                }
            }
        }
    }
    // Check incoming cross-file calls (other files call symbols in this file)
    let q2 = format!(
        "MATCH (a:Symbol)-[:CALLS]->(b:Symbol) WHERE b.file = '{escaped}' AND a.file <> '{escaped}' RETURN count(*) LIMIT 1"
    );
    if let Ok(mut result) = conn.query(&q2) {
        if let Some(row) = result.next() {
            if let Some(val) = row.first() {
                return val.to_string().parse::<u64>().unwrap_or(0) > 0;
            }
        }
    }
    false
}

fn should_ignore(path: &Path, ignore_dirs: &[&str]) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        ignore_dirs.contains(&s.as_ref()) || s.starts_with('.')
    })
}
