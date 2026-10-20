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

/// Edits made while no watcher ran produce no event for a new one to see, so
/// a producer that has finished registering queues one whole-project pass --
/// the hash-diffing scan `infigraph index` does -- with no change made at
/// all. Queued after registration, so nothing can fall between the two.
#[tokio::test]
async fn producer_queues_a_true_up_once_it_is_watching() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::fs::write(root.join("main.py"), "def main(): pass").unwrap();

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();
    let (queue_clone, cfg, token_clone) = (Arc::clone(&queue), config(root.clone()), token.clone());
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(cfg, queue_clone, |_evt| {}, token_clone)
            .await;
    });

    let mut queued = false;
    for _ in 0..300 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if queue.lock().unwrap().wants_whole_project() {
            queued = true;
            break;
        }
    }
    assert!(
        queued,
        "a watcher that finished registering queued no true-up"
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
///
/// Not on macOS: there the whole tree is one recursive subscription, so a
/// created directory is never registered and has no call to stall.
#[cfg(not(target_os = "macos"))]
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

/// A sink that only records the batches it is given.
struct Recorder(Arc<Mutex<Vec<infigraph_core::watch::sink::WatchBatch>>>);

impl infigraph_core::watch::sink::WatchSink for Recorder {
    fn name(&self) -> &'static str {
        "recorder"
    }
    fn domain(&self) -> infigraph_core::ignore_rules::Domain {
        infigraph_core::ignore_rules::Domain::Docs
    }
    fn accept(&mut self, batch: infigraph_core::watch::sink::WatchBatch) {
        self.0.lock().unwrap().push(batch);
    }
    fn health(&self) -> infigraph_core::watch::sink::SinkHealth {
        Default::default()
    }
}

/// The canary is the producer's, not the code sink's: a deaf subscription is
/// rebuilt once and every attached sink, whatever its domain, is told events
/// were lost.
#[tokio::test(flavor = "current_thread")]
async fn a_deaf_subscription_tells_every_attached_sink_to_rescan() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::env::set_var("INFIGRAPH_TEST_WATCH_CANARY_MS", "300,600");
    std::env::set_var("INFIGRAPH_TEST_WATCH_CANARY_DEAF", "1");
    let seen = Arc::new(Mutex::new(Vec::new()));
    infigraph_core::watch::sink::request_attach(&root, Box::new(Recorder(Arc::clone(&seen))));

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();
    let (cfg, queue_clone, token_clone) = (config(root.clone()), Arc::clone(&queue), token.clone());
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(cfg, queue_clone, |_evt| {}, token_clone)
            .await;
    });

    // One rescan for attaching, then one for each miss.
    let mut rescans = 0;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        rescans = seen.lock().unwrap().iter().filter(|b| b.rescan).count();
        if rescans >= 2 {
            break;
        }
    }
    token.cancel();
    handle.await.unwrap();
    std::env::remove_var("INFIGRAPH_TEST_WATCH_CANARY_MS");
    std::env::remove_var("INFIGRAPH_TEST_WATCH_CANARY_DEAF");
    assert!(
        rescans >= 2,
        "the attach and the canary miss each tell the sink to rescan, saw {rescans}"
    );
}

/// A watcher that cannot see its own probe write is deaf, and says so: the
/// project is degraded (`code-watcher-deaf`) and the subscription rebuilt.
/// The test hook makes the producer ignore the probe's event, which is what
/// a stream that delivers nothing looks like from inside.
#[tokio::test]
async fn a_watcher_that_never_sees_its_probe_degrades_the_project() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::env::set_var("INFIGRAPH_TEST_WATCH_CANARY_MS", "300,600");
    std::env::set_var("INFIGRAPH_TEST_WATCH_CANARY_DEAF", "1");

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();
    let (cfg, queue_clone, token_clone) = (config(root.clone()), Arc::clone(&queue), token.clone());
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(cfg, queue_clone, |_evt| {}, token_clone)
            .await;
    });

    let mut degraded = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        degraded = infigraph_core::degraded::live::for_root(&root)
            .iter()
            .any(|n| n.key == infigraph_core::degraded::CODE_WATCHER_DEAF);
        if degraded {
            break;
        }
    }
    token.cancel();
    handle.await.unwrap();
    std::env::remove_var("INFIGRAPH_TEST_WATCH_CANARY_MS");
    std::env::remove_var("INFIGRAPH_TEST_WATCH_CANARY_DEAF");
    assert!(
        degraded,
        "a watcher blind to its own probe must be reported as degraded"
    );
}

/// The healthy case: probes are seen, nothing is degraded, and the probes
/// cost no extra true-up (the startup one is the only whole-project pass).
#[tokio::test]
async fn a_healthy_watcher_sees_its_probes_and_queues_no_true_up_for_them() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::env::set_var("INFIGRAPH_TEST_WATCH_CANARY_MS", "300,3000");

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();
    let before = infigraph_core::watch::producer::true_ups_queued();
    let (cfg, queue_clone, token_clone) = (config(root.clone()), Arc::clone(&queue), token.clone());
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(cfg, queue_clone, |_evt| {}, token_clone)
            .await;
    });

    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    let degraded = infigraph_core::degraded::live::for_root(&root);
    let after = infigraph_core::watch::producer::true_ups_queued();
    token.cancel();
    handle.await.unwrap();
    std::env::remove_var("INFIGRAPH_TEST_WATCH_CANARY_MS");
    assert!(
        degraded.is_empty(),
        "a healthy watcher is not degraded: {degraded:?}"
    );
    assert_eq!(after - before, 1, "only the startup true-up");
}

/// What the storm tests do for 30s while a producer watches a 1000-directory
/// tree: write under a gitignored `target/`, and/or create and `rm -rf`
/// directories under the watched tree, in a tight loop.
#[derive(Clone, Copy)]
struct Storm {
    ignored_writes: bool,
    directory_churn: bool,
}

/// Runs `storm` for 30s, then rewrites one source file every 5s until an
/// event for it reaches the queue (an empty queue to begin with, so nothing
/// else can fill it). Returns how long that took (`None`: 90s) and how many
/// whole-project passes the producer queued meanwhile.
async fn storm_then_edit(storm: Storm) -> (Option<std::time::Duration>, u64, u64) {
    const CHURN: std::time::Duration = std::time::Duration::from_secs(30);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    for i in 0..1000 {
        let dir = root.join(format!("src/d{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m.py"), "def f(): pass\n").unwrap();
    }

    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let token = CancellationToken::new();
    let (cfg, queue_clone, token_clone) = (config(root.clone()), Arc::clone(&queue), token.clone());
    let handle = tokio::task::spawn(async move {
        infigraph_core::watch::producer::run_producer(cfg, queue_clone, |_evt| {}, token_clone)
            .await;
    });
    wait_until_watching(&root, &queue).await;
    let before = infigraph_core::watch::producer::true_ups_queued();

    let storm_root = root.clone();
    let churn = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let mut n = 0u64;
        while start.elapsed() < CHURN {
            if storm.ignored_writes {
                // 20k distinct files under the ignored target/, spread over
                // the run (a build writes them over minutes; unthrottled this
                // loop does 20k a second and backs the channel up for 8s).
                for _ in 0..50 {
                    let f = storm_root.join(format!("target/debug/f{}.o", n % 20_000));
                    let _ = std::fs::write(f, b"x");
                    n += 1;
                }
                std::thread::sleep(std::time::Duration::from_millis(75));
            }
            if storm.directory_churn {
                let d = storm_root.join("src/churn/a/b");
                let _ = std::fs::create_dir_all(&d);
                let _ = std::fs::write(d.join("c.py"), "def c(): pass\n");
                let _ = std::fs::remove_dir_all(storm_root.join("src/churn"));
            }
        }
        n
    });
    let written = tokio::task::spawn_blocking(move || churn.join().unwrap())
        .await
        .unwrap();

    *queue.lock().unwrap() = IndexWorkQueue::new();
    let edited_at = std::time::Instant::now();
    let mut seen_after = None;
    // Rewritten every 5s, not continuously: a write inside the batch's 1s
    // window restarts it.
    'wait: for i in 0..18 {
        std::fs::write(
            root.join("src/d7/edit.py"),
            format!("def edited(): return {i}\n"),
        )
        .unwrap();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if queue.lock().unwrap().holds_path("src/d7/edit.py") {
                seen_after = Some(edited_at.elapsed());
                break 'wait;
            }
        }
    }
    let true_ups = infigraph_core::watch::producer::true_ups_queued() - before;
    token.cancel();
    handle.await.unwrap();
    eprintln!(
        "storm: {written} ignored writes in {CHURN:?}; edit seen after {seen_after:?}; \
         {true_ups} true-up(s)"
    );
    (seen_after, true_ups, CHURN.as_secs())
}

/// Writes under an ignored path, as a Rust tree under `cargo build` makes on
/// macOS, where one recursive subscription delivers `target/` too: the
/// flood is filtered in process, a real edit is still seen promptly, and the
/// whole-project passes it asks for stay bounded by the debounce. Long, so
/// run on request: `cargo test -p infigraph-core --test producer -- --ignored storm`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a minute of file churn; run with --ignored"]
async fn storm_of_ignored_writes_leaves_the_watcher_responsive() {
    let (seen, true_ups, secs) = storm_then_edit(Storm {
        ignored_writes: true,
        directory_churn: false,
    })
    .await;
    assert!(
        seen.is_some_and(|t| t < std::time::Duration::from_secs(5)),
        "an edit after the storm was seen after {seen:?}, not within 5s"
    );
    // One per debounce window, plus the one in flight at each end.
    assert!(true_ups <= secs / 30 + 2, "{true_ups} true-ups in {secs}s");
}

/// Directory churn (create, write, `rm -rf` in a tight loop for 30s) is what
/// made the watcher on a recursive macOS subscription go deaf: no event
/// arrived for any file until the subscription was rebuilt. The canary finds
/// that, so the edit is seen within its period plus its wait (plus slack),
/// and the bound is the canary's, not 5s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a minute of file churn; run with --ignored"]
async fn storm_of_directory_churn_is_healed_by_the_canary() {
    let (seen, true_ups, secs) = storm_then_edit(Storm {
        ignored_writes: false,
        directory_churn: true,
    })
    .await;
    assert!(
        seen.is_some_and(|t| t < std::time::Duration::from_secs(45)),
        "an edit after the churn was seen after {seen:?}, not within the canary's bound"
    );
    assert!(true_ups <= secs / 30 + 3, "{true_ups} true-ups in {secs}s");
}
