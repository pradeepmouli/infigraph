use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn cli_binary() -> std::path::PathBuf {
    // Mirrors infigraph_core::watch::daemon::resolve_cli_binary_sibling_of's
    // grandparent fallback: integration-test binaries live one level below
    // the real build output directory.
    let exe = std::env::current_exe().unwrap();
    let deps_dir = exe.parent().unwrap();
    let candidate = deps_dir.join("infigraph");
    if candidate.exists() {
        return candidate;
    }
    deps_dir.parent().unwrap().join("infigraph")
}

/// Real end-to-end test: spawns `infigraph watch <root>` as a genuine
/// detached child process, indexes docs for the same root partway through,
/// and confirms the daemon's doc thread picked it up without restarting
/// the watch process -- proving cmd_watch's doc-thread wiring works, not
/// just watch_docs_daemon_loop in isolation (Task 1 already covers that).
///
/// Two synchronization points matter here, both driven off the daemon
/// subprocess's own stderr rather than fixed sleeps or polling:
///
/// 1. `watch_docs` (the daemon's per-attach worker, see infigraph-docs's
///    watch.rs) only reacts to filesystem events observed *after* its
///    `notify` watcher registers -- there's no initial full scan on attach.
///    readme.md must not be written until the daemon has actually attached,
///    or the file-create event fires before anything is listening and is
///    silently missed. We wait for the "attaching doc watcher" log line.
///
/// 2. Verifying the reindex happened is done by watching for the daemon's
///    "reindexed: N files, M chunks" line, NOT by opening `docs.kuzu` a
///    second time from the test process. A second concurrent
///    `DocIndex::open` would race the daemon's own open: `DocStore`'s
///    `DB_LOCK` (store.rs) is a process-local `static Mutex` -- it protects
///    concurrent opens *within* one process (as in Task 1's in-process
///    test) but gives zero protection across the process boundary here,
///    where the daemon runs as a real child process. `DocIndex::init()`'s
///    any-open-failure-wipes-and-rebuilds recovery then turns that lock
///    contention into active destruction of the daemon's live index. This
///    was empirically confirmed to be the actual cause of this test's
///    failures (not a bug in `cmd_watch`'s doc-thread wiring, which a
///    manual run confirmed works correctly): the daemon's stderr showed a
///    genuine "reindexed: 1 files, 1 chunks" immediately followed by a
///    spurious detach/reattach cycle once a racing test-side open wiped
///    the file out from under it.
#[test]
fn cmd_watch_daemon_also_indexes_docs_without_restart() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "skipping: infigraph CLI binary not found at {} (needs a full `cargo build`/`cargo test --workspace` first)",
            bin.display()
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();

    let mut child = Command::new(&bin)
        .arg("daemon")
        .arg("--debounce")
        .arg("50")
        // Fast daemon attach-poll so this test doesn't wait through the
        // production default (1000ms) to notice docs.kuzu appearing.
        .env("INFIGRAPH_DOC_DAEMON_POLL_MS", "50")
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn infigraph watch");

    let (attach_tx, attach_rx) = mpsc::channel::<()>();
    let (reindexed_tx, reindexed_rx) = mpsc::channel::<String>();

    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            eprintln!("[watch stdout] {line}");
        }
    });
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[watch stderr] {line}");
            if line.contains("attaching doc watcher") {
                let _ = attach_tx.send(());
            }
            if line.contains("reindexed:") {
                let _ = reindexed_tx.send(line);
            }
        }
    });

    // Give the watcher a moment to acquire watch.lock and start its loops.
    std::thread::sleep(Duration::from_millis(300));

    // Index docs for the same root WITHOUT stopping the watch process --
    // this is what makes docs.kuzu appear mid-run, the exact scenario the
    // daemon's doc thread must notice on its own. This is a ONE-TIME setup
    // open before the daemon has ever attached, so it can't race the
    // daemon's own open the way a repeated polling open would.
    infigraph_docs::DocIndex::open(&root)
        .unwrap()
        .init()
        .unwrap();

    attach_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("daemon's doc thread never attached after docs.kuzu appeared");
    // The attach log fires before `notify::Watcher::watch()` returns inside
    // `watch_docs`; give it a brief moment to finish registering so the
    // upcoming write is guaranteed to land after the watch is live.
    std::thread::sleep(Duration::from_millis(200));

    std::fs::write(root.join("readme.md"), "# hello\n\nsome content").unwrap();

    let reindex_line = reindexed_rx.recv_timeout(Duration::from_secs(10));

    let _ = child.kill();
    let _ = child.wait();

    let reindex_line = reindex_line.expect(
        "the running watch daemon's doc thread must have logged a reindex of readme.md \
         without a restart (timed out waiting for a 'reindexed:' line on its stderr)",
    );
    let indexed_files: u32 = reindex_line
        .split_whitespace()
        .find_map(|tok| tok.parse::<u32>().ok())
        .unwrap_or(0);
    assert!(
        indexed_files > 0,
        "expected a real reindex of readme.md, got: {reindex_line}"
    );
}
