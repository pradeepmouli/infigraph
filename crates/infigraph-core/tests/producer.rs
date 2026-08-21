//! Integration coverage for the extracted fsevent-watching producer
//! (crates/infigraph-core/src/watch/producer.rs). Verifies it correctly
//! feeds IndexWorkQueue on real filesystem events and stops cleanly on
//! cancellation, WITHOUT any coordinator/drain logic running alongside it
//! -- that's the whole point of the split.

use infigraph_core::watch::queue::IndexWorkQueue;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn producer_feeds_the_queue_on_a_real_file_change() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::fs::write(root.join("main.py"), "def main(): pass").unwrap();

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();

    let queue_clone = Arc::clone(&queue);
    let root_clone = root.clone();
    let token_clone = token.clone();
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(
            root_clone,
            queue_clone,
            |_evt| {},
            token_clone,
        )
        .await;
    });

    // Give the watcher time to register, then trigger a real fsevent.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    std::fs::write(root.join("main.py"), "def main(): return 1").unwrap();

    // Poll for the queue to reflect the change. The producer's own debounce
    // window (ChangeBatch, 1s) plus its flush cadence means this is not
    // instant, and raw fsevent delivery latency varies by platform and load
    // -- hence a generous budget rather than one fixed sleep.
    let mut saw_queued_work = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if !queue.lock().unwrap().is_empty() {
            saw_queued_work = true;
            break;
        }
    }
    assert!(
        saw_queued_work,
        "producer should have marked main.py dirty and queued it"
    );

    token.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn producer_exits_promptly_on_cancellation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();

    let queue_clone = Arc::clone(&queue);
    let root_clone = root.clone();
    let token_clone = token.clone();
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(
            root_clone,
            queue_clone,
            |_evt| {},
            token_clone,
        )
        .await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let start = std::time::Instant::now();
    token.cancel();
    handle.await.unwrap();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "cancellation should be near-instant (event-driven select!, not a poll interval)"
    );
}
