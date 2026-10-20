//! `WriteRequest::IndexDocs` over the socket to a real write coordinator
//! (docs opt-in). The executor lives in `infigraph-docs`; the coordinator
//! reaches it through the `DocsHandle` the daemon's owner supplies, so a
//! canned handle stands in for it here.

mod common;
use common::daemon::{start, start_with_docs, stop, ENV_LOCK};

use std::sync::Arc;
use std::time::{Duration, Instant};

use infigraph_core::daemon::read_protocol::WatchAction;
use infigraph_core::daemon::writes::{submit, WriteOpts};
use infigraph_core::daemon_protocol::{DocAck, DocIndexStats, WriteRequest, WriteResult};

/// Answers `index_docs` after `delay` with a count that shows `full`.
struct CannedIndexer {
    delay: Duration,
}

impl infigraph_core::daemon::DocsHandle for CannedIndexer {
    fn control(&self, _action: WatchAction) -> Result<(), String> {
        Ok(())
    }
    fn is_running(&self) -> bool {
        false
    }
    fn is_busy(&self) -> bool {
        false
    }
    fn index_docs(&self, full: bool) -> Result<DocIndexStats, String> {
        std::thread::sleep(self.delay);
        Ok(DocIndexStats {
            files_indexed: if full { 2 } else { 1 },
            ..Default::default()
        })
    }
    fn index_doc_paths(&self, upserts: &[String], _removals: &[String]) -> Result<bool, String> {
        std::thread::sleep(self.delay);
        Ok(!upserts.is_empty())
    }
    fn ack_docs(&self, acks: &[DocAck], drop: &[String]) -> Result<(usize, usize), String> {
        std::thread::sleep(self.delay);
        Ok((acks.len(), drop.len()))
    }
}

fn an_ack() -> WriteRequest {
    WriteRequest::AckDocEntries {
        acks: vec![DocAck {
            target: "src/a.py::f".to_string(),
            commit: "abc".to_string(),
            commit_time: 1_700_000_000,
            acked_at: 1_800_000_000,
        }],
        drop: vec!["src/a.py::g".to_string(), "src/a.py::h".to_string()],
    }
}

fn opts() -> WriteOpts<'static> {
    WriteOpts {
        timeout: Duration::from_secs(120),
        cancel: None,
        on_slot_wait: None,
    }
}

#[test]
fn index_docs_is_answered_with_the_indexers_stats() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start_with_docs(
        dir.path(),
        Some(Arc::new(CannedIndexer {
            delay: Duration::from_millis(100),
        })),
    );
    let reply = submit(dir.path(), &WriteRequest::IndexDocs { full: true }, opts()).unwrap();
    assert_eq!(
        reply,
        WriteResult::DocsIndexed(DocIndexStats {
            files_indexed: 2,
            ..Default::default()
        })
    );
    stop(d);
}

#[test]
fn an_ack_is_answered_with_what_the_docs_handle_stored_and_dropped() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start_with_docs(
        dir.path(),
        Some(Arc::new(CannedIndexer {
            delay: Duration::from_millis(50),
        })),
    );
    let reply = submit(dir.path(), &an_ack(), opts()).unwrap();
    assert_eq!(
        reply,
        WriteResult::DocsAcked {
            stored: 1,
            dropped: 2
        }
    );
    stop(d);
}

#[test]
fn a_daemon_without_a_docs_handle_refuses_an_ack_with_a_reason() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    let reply = submit(dir.path(), &an_ack(), opts()).unwrap();
    assert_eq!(
        reply,
        WriteResult::Err {
            message: infigraph_core::daemon::NO_DOCS_INDEXER.to_string()
        }
    );
    stop(d);
}

/// One docs write at a time, of either kind: an ack asked for while an index
/// runs waits for it and is then served.
#[test]
fn an_ack_waits_for_the_docs_index_that_is_running() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start_with_docs(
        dir.path(),
        Some(Arc::new(CannedIndexer {
            delay: Duration::from_millis(800),
        })),
    );
    let root = dir.path().to_path_buf();
    let indexing =
        std::thread::spawn(move || submit(&root, &WriteRequest::IndexDocs { full: false }, opts()));
    std::thread::sleep(Duration::from_millis(200));

    let t = Instant::now();
    let reply = submit(dir.path(), &an_ack(), opts()).unwrap();

    assert!(
        t.elapsed() >= Duration::from_millis(400),
        "the ack ran alongside the index ({:?})",
        t.elapsed()
    );
    assert!(matches!(reply, WriteResult::DocsAcked { .. }), "{reply:?}");
    assert!(matches!(
        indexing.join().unwrap().unwrap(),
        WriteResult::DocsIndexed(_)
    ));
    stop(d);
}

#[test]
fn a_daemon_without_a_docs_handle_refuses_index_docs_with_a_reason() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    let reply = submit(dir.path(), &WriteRequest::IndexDocs { full: false }, opts()).unwrap();
    assert_eq!(
        reply,
        WriteResult::Err {
            message: infigraph_core::daemon::NO_DOCS_INDEXER.to_string()
        }
    );
    stop(d);
}

/// Spec: a long first index never blocks code-graph writes. It runs on a
/// background task, not the coordinator thread.
#[test]
fn code_writes_are_served_while_documents_index() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start_with_docs(
        dir.path(),
        Some(Arc::new(CannedIndexer {
            delay: Duration::from_secs(5),
        })),
    );
    // Warm: the graph is open before anything is timed.
    submit(dir.path(), &WriteRequest::Index { paths: None }, opts()).unwrap();

    let root = dir.path().to_path_buf();
    let indexing =
        std::thread::spawn(move || submit(&root, &WriteRequest::IndexDocs { full: false }, opts()));
    std::thread::sleep(Duration::from_millis(300));

    let t = Instant::now();
    let code = submit(
        dir.path(),
        &WriteRequest::UpsertRepo {
            namespace: "n".into(),
        },
        opts(),
    )
    .unwrap();
    assert!(matches!(code, WriteResult::Ok { .. }), "{code:?}");
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "a code write waited {:?} behind a docs index",
        t.elapsed()
    );
    assert!(matches!(
        indexing.join().unwrap().unwrap(),
        WriteResult::DocsIndexed(_)
    ));
    stop(d);
}
