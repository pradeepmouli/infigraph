//! Integration coverage for the extracted fsevent-watching producer
//! (crates/infigraph-core/src/watch/producer.rs). Verifies it correctly
//! feeds IndexWorkQueue on real filesystem events and stops cleanly on
//! cancellation, WITHOUT any coordinator/drain logic running alongside it
//! -- that's the whole point of the split.

use infigraph_core::daemon::queue::IndexWorkQueue;
use infigraph_core::watch::producer::ProducerConfig;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// The test stall hook's file name (`INFIGRAPH_TEST_WATCH_REGISTER_STALL_FILE`).
/// Root-relative, so a producer in another test never stalls, and one value
/// for every test here because the variable is process-wide.
const STALL: &str = "register-stall";

fn config(root: PathBuf) -> ProducerConfig {
    ProducerConfig {
        root,
        registry: Arc::new(infigraph_languages::bundled_registry().unwrap()),
        debounce_ms: 50,
        ignore_rebuild_secs: 300,
    }
}

/// Wait until the producer is actually watching `root`, then clear what
/// proving it left behind, so the test starts from an empty queue and dirty
/// set. A fixed sleep used to be enough only because registration ran on
/// the producer's own task and so blocked these `current_thread` tests'
/// only thread; it now runs on a thread of its own (so a cancel need not
/// wait for it), and on a loaded machine `fseventsd` takes longer than any
/// fixed sleep.
async fn wait_until_watching(root: &Path, queue: &Arc<Mutex<IndexWorkQueue>>) {
    const PROBE: &str = "watch_probe.py";
    for i in 0..600 {
        if i % 10 == 0 {
            std::fs::write(root.join(PROBE), format!("def probe(): return {i}")).unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if !queue.lock().unwrap().is_empty() {
            // Past the 1s debounce and a flush tick, so no late probe event
            // lands after the reset below.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            *queue.lock().unwrap() = IndexWorkQueue::new();
            infigraph_core::dirty::clear_dirty(&root.join(".infigraph"), &[PROBE.to_string()])
                .unwrap();
            return;
        }
    }
    panic!("the producer never started watching {}", root.display());
}

/// A dependency lockfile must not be queued *even though a language pack
/// genuinely claims it*. `pnpm-lock.yaml` is matched by the bundled YAML
/// pack, so `registry.for_file` does NOT stop it -- it sailed through and got
/// indexed, which is how sittir accumulated 3,498 symbols from one lockfile.
///
/// Contrast `producer_ignores_files_no_language_pack_claims` below: that is
/// the registry filter doing its job for an extension nothing claims. This is
/// the case the registry filter cannot reach, and therefore the one that
/// needed `store_util::is_lockfile` beside it at the same call site.
#[tokio::test]
async fn producer_ignores_dependency_lockfiles_a_language_pack_does_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();

    let queue_clone = Arc::clone(&queue);
    let cfg = config(root.clone());
    let token_clone = token.clone();
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(cfg, queue_clone, |_evt| {}, token_clone)
            .await;
    });

    wait_until_watching(&root, &queue).await;
    // Real, parseable YAML: if this is skipped it is because it is a
    // lockfile, not because nothing could claim or parse it.
    std::fs::write(
        root.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      left-pad:\n        version: 1.3.0\n",
    )
    .unwrap();

    // Same budget as the sibling test: the 1s debounce plus several flush
    // ticks, so a missing filter would have queued the path well inside it.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(
        queue.lock().unwrap().is_empty(),
        "a dependency lockfile must not be queued even though the YAML pack claims it"
    );

    let dirty = infigraph_core::dirty::pending_dirty(&root.join(".infigraph")).unwrap();
    assert!(
        dirty.is_empty(),
        "nor may it be persisted as dirty, got: {dirty:?}"
    );

    token.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn producer_feeds_the_queue_on_a_real_file_change() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::fs::write(root.join("main.py"), "def main(): pass").unwrap();

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();

    let queue_clone = Arc::clone(&queue);
    let cfg = config(root.clone());
    let token_clone = token.clone();
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(cfg, queue_clone, |_evt| {}, token_clone)
            .await;
    });

    wait_until_watching(&root, &queue).await;
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

/// Regression test for the language-registry filter. A file no language
/// pack claims can never produce a `FileExtraction`, and `clear_dirty` only
/// clears what a drain's extractions reported -- so marking one dirty makes
/// it dirty forever. Without the filter every README, lockfile and image
/// touched under the root accumulates in a dirty set that never drains.
#[tokio::test]
async fn producer_ignores_files_no_language_pack_claims() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();

    let queue_clone = Arc::clone(&queue);
    let cfg = config(root.clone());
    let token_clone = token.clone();
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(cfg, queue_clone, |_evt| {}, token_clone)
            .await;
    });

    wait_until_watching(&root, &queue).await;
    std::fs::write(root.join("notes.unclaimedext"), "not source of any kind").unwrap();

    // Long enough to cover the 1s debounce window plus several flush ticks:
    // if the filter is missing, the path is queued well inside this budget.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(
        queue.lock().unwrap().is_empty(),
        "a file with no language pack behind it must not be queued -- it would \
         stay in the persistent dirty set forever"
    );

    let dirty = infigraph_core::dirty::pending_dirty(&root.join(".infigraph")).unwrap();
    assert!(
        dirty.is_empty(),
        "nor may it be persisted as dirty, got: {dirty:?}"
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
    let cfg = config(root.clone());
    let token_clone = token.clone();
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(cfg, queue_clone, |_evt| {}, token_clone)
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

/// A cancel must end the producer even while watcher registration is
/// stalled -- on macOS that is an RPC to a backed-up `fseventsd`, seen
/// taking over a minute on a loaded dev machine, during which a stopped
/// in-process MCP watcher kept `watch.lock` and the next `watch_project`
/// on the root failed with "another watcher is already running".
#[tokio::test]
async fn producer_exits_promptly_on_cancellation_while_registration_is_stalled() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::env::set_var("INFIGRAPH_TEST_WATCH_REGISTER_STALL_FILE", STALL);
    std::fs::write(root.join(STALL), "").unwrap();

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();
    let cfg = config(root.clone());
    let token_clone = token.clone();
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(cfg, queue, |_evt| {}, token_clone).await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let start = std::time::Instant::now();
    token.cancel();
    let exited = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    // Let the registration thread finish and drop its watcher either way.
    std::fs::remove_file(root.join(STALL)).unwrap();
    exited
        .expect("a cancel must not wait out a stalled watcher registration")
        .unwrap();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "cancellation took {:?} with registration stalled",
        start.elapsed()
    );
}

/// The same for a directory created while watching: it gets its own
/// subscription, registered from inside the event loop through the same
/// blocking `fseventsd` call as startup's. Multi-threaded so that, were the
/// loop to block its worker, the test itself could still time the cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn producer_exits_promptly_on_cancellation_while_a_new_directory_registration_is_stalled() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::env::set_var("INFIGRAPH_TEST_WATCH_REGISTER_STALL_FILE", STALL);

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();
    let cfg = config(root.clone());
    let queue_clone = Arc::clone(&queue);
    let token_clone = token.clone();
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(cfg, queue_clone, |_evt| {}, token_clone)
            .await;
    });
    wait_until_watching(&root, &queue).await;

    std::fs::write(root.join(STALL), "").unwrap();
    std::fs::create_dir(root.join("new_dir")).unwrap();
    let stalled = root.join(format!("{STALL}.stalled"));
    for _ in 0..600 {
        if stalled.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        stalled.exists(),
        "the producer never started registering new_dir"
    );
    // A blocked producer must still end, so the failure is an assertion and
    // not a hung test.
    let stall = root.join(STALL);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(10));
        let _ = std::fs::remove_file(stall);
    });

    let start = std::time::Instant::now();
    token.cancel();
    let exited = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    let _ = std::fs::remove_file(root.join(STALL));
    exited
        .expect("a cancel must not wait out a stalled new-directory registration")
        .unwrap();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "cancellation took {:?} with a new-directory registration stalled",
        start.elapsed()
    );
}
