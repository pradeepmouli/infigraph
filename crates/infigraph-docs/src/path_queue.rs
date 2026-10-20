//! The document index as a [`PathQueue`]: how the watcher's settled changes
//! get in line for the daemon's docs writer, which is the execute side
//! ([`PathIndexer for DocIndex`](crate::path_indexer)).
//!
//! The sink never opens `docs.kuzu`. It submits `IndexDocPaths` (or, for a
//! whole-project pass, `IndexDocs`) to the daemon's coordinator as a client
//! would, and waits for the answer on its own worker thread. The coordinator
//! runs one docs write at a time and coalesces the ones waiting, under the
//! exclusive docs lock with the `docs_enabled` switch re-read inside it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use infigraph_core::daemon_protocol::{WriteRequest, WriteResult};
use infigraph_core::pending_paths::{PendingPaths, Taken};
use infigraph_core::watch::queued::PathQueue;

use crate::{is_document_file, DocIndex};

/// Where a docs write goes. In the daemon it is the coordinator's port; a
/// test passes a function that runs the executor directly.
pub trait DocSubmit: Send + Sync {
    fn submit(&self, request: WriteRequest) -> anyhow::Result<WriteResult>;
}

impl<F> DocSubmit for F
where
    F: Fn(WriteRequest) -> anyhow::Result<WriteResult> + Send + Sync,
{
    fn submit(&self, request: WriteRequest) -> anyhow::Result<WriteResult> {
        self(request)
    }
}

/// What enqueuing a document change needs.
pub struct DocQueue {
    root: PathBuf,
    submit: Arc<dyn DocSubmit>,
}

impl DocQueue {
    pub fn new(root: &Path, submit: Arc<dyn DocSubmit>) -> Self {
        Self {
            root: root.to_path_buf(),
            submit,
        }
    }
}

impl PathQueue for DocIndex {
    type Handle = DocQueue;

    /// A created or modified path is the document index's business only if the
    /// index would read it.
    fn wants(_handle: &DocQueue, rel: &str) -> bool {
        is_document_file(Path::new(rel))
    }

    fn enqueue(handle: &DocQueue, pending: PendingPaths) {
        let Taken {
            upserts,
            removals,
            whole,
            ..
        } = pending.take();
        let request = if whole {
            // The whole-tree run finds the changes and prunes the removals too.
            // Not `IndexDocs`: that opts a project in, and a watcher never does.
            WriteRequest::IndexDocPaths {
                upserts: Vec::new(),
                removals: Vec::new(),
                whole: true,
            }
        } else if upserts.is_empty() && removals.is_empty() {
            return;
        } else {
            let mut upserts: Vec<String> = upserts.into_keys().collect();
            let mut removals: Vec<String> = removals.into_iter().collect();
            upserts.sort();
            removals.sort();
            WriteRequest::IndexDocPaths {
                upserts,
                removals,
                whole: false,
            }
        };
        match handle.submit.submit(request) {
            Ok(WriteResult::Err { message }) => eprintln!(
                "[doc-watch-daemon] {} the docs write for {} failed: {message}",
                infigraph_core::watch::log_stamp(),
                handle.root.display()
            ),
            Ok(_) => {}
            Err(e) => eprintln!(
                "[doc-watch-daemon] {} could not submit a docs write for {}: {e:#}",
                infigraph_core::watch::log_stamp(),
                handle.root.display()
            ),
        }
    }
}
