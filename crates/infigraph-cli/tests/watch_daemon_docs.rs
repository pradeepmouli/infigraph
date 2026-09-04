use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Polls `still_alive` until it reports false, or the budget runs out.
/// Returns how long the wait took, or `None` if it never went away.
///
/// The budget is 30s, not the 5s these three waits used to allow. A debug
/// build of the daemon spends seconds building its 62-language registry
/// before its coordinator loop starts -- and therefore before it can notice a
/// stop sentinel at all -- so on a loaded runner a 5s budget expires while
/// the daemon is still starting up, not because it ignored the stop. That is
/// the same reasoning, and the same 30s, as the watch-loop reply budget noted
/// in CLAUDE.md. This first failed on Linux CI, where the whole suite runs
/// under more contention than a dev machine.
///
/// Prints when a wait exceeds the old 5s budget, so a CI log distinguishes
/// "needed more time" from "was never going to stop".
fn wait_until_gone(what: &str, mut still_alive: impl FnMut() -> bool) -> Option<Duration> {
    const BUDGET: Duration = Duration::from_secs(30);
    const STEP: Duration = Duration::from_millis(100);
    let start = Instant::now();
    loop {
        if !still_alive() {
            let waited = start.elapsed();
            if waited > Duration::from_secs(5) {
                eprintln!("[test] {what} took {waited:?} to go away (over the old 5s budget)");
            }
            return Some(waited);
        }
        if start.elapsed() >= BUDGET {
            return None;
        }
        std::thread::sleep(STEP);
    }
}

fn cli_binary() -> std::path::PathBuf {
    // Mirrors infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of's
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

/// RAII guard that kills and reaps a spawned child process on drop, so a
/// panic anywhere in the test (not just the happy path) can't leave a real
/// `infigraph daemon` process running forever against an abandoned tempdir.
/// `std::process::Child` does not do this itself -- Drop just closes the
/// parent's handle, the child keeps running independently.
struct KillOnDrop(std::process::Child);

impl std::ops::Deref for KillOnDrop {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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

    let mut child = KillOnDrop(
        Command::new(&bin)
            .arg("daemon")
            .arg("--debounce")
            .arg("50")
            // Fast daemon attach-poll so this test doesn't wait through the
            // production default (1000ms) to notice docs.kuzu appearing.
            .env("INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS", "50")
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn infigraph watch"),
    );

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

    // Kill the daemon now rather than waiting for `child` to drop at end of
    // scope, so it can't still be writing reindex lines during the
    // post-kill assertions below. `KillOnDrop` still guards every other
    // exit path (early return, panic).
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

/// Spawns `infigraph daemon` as a genuine detached process against `root`
/// and waits for it to acquire `watch.lock`, mirroring
/// `cmd_watch_daemon_also_indexes_docs_without_restart`'s harness shape.
/// Callers get back the `KillOnDrop`-guarded child plus its lock path so
/// they can drive it with further real CLI subprocess invocations.
fn spawn_ready_daemon(
    bin: &std::path::Path,
    root: &std::path::Path,
) -> (KillOnDrop, std::path::PathBuf) {
    let lock_path = root.join(".infigraph").join("watch.lock");

    let mut daemon = KillOnDrop(
        Command::new(bin)
            .arg("daemon")
            .arg("--debounce")
            .arg("50")
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn infigraph daemon"),
    );

    let stdout = daemon.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            eprintln!("[daemon stdout] {line}");
        }
    });
    let stderr = daemon.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[daemon stderr] {line}");
        }
    });

    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(
            &lock_path,
            Duration::from_secs(10)
        ),
        "daemon never acquired watch.lock after spawn"
    );

    (daemon, lock_path)
}

/// Real end-to-end test: the deprecated `Commands::WatchStop` alias
/// (`infigraph watch-stop`, kebab-cased from its single-variant name --
/// confirmed via the built binary's `--help` output, distinct from the
/// nested `infigraph watch stop` subcommand covered below) still writes the
/// raw `.infigraph/watch.stop` sentinel directly rather than going through
/// the `WatchControl` protocol. `run_write_coordinator`'s single top-level
/// loop (crates/infigraph-core/src/watch/mod.rs) treats that sentinel's
/// mere existence as a full-loop `break` -- the SAME loop that also serves
/// `.infigraph/requests/` write requests, code-watch, and doc-watch. So,
/// unlike the newer per-role `WatchControl` protocol, this legacy path has
/// no way to stop only code-watching: it brings the WHOLE daemon process
/// down, exactly like `infigraph daemon-stop` does, just via a cruder
/// mechanism -- which is exactly why its own doc comment says "Prefer
/// `daemon stop`". `docs/DESIGN-hardening.md`'s R2.4.6 description confirms
/// this was the known limitation of the pre-split design that this whole
/// plan exists to work around for the new commands (not this legacy one).
/// This test proves the deprecated sentinel path is still genuinely wired
/// up (not dead code) and has the expected process-killing parity with
/// `daemon-stop`, rather than silently no-op'ing.
#[test]
fn legacy_watch_stop_alias_brings_the_whole_daemon_down() {
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

    let (mut daemon, lock_path) = spawn_ready_daemon(&bin, &root);

    let watch_stop_status = Command::new(&bin)
        .arg("watch-stop")
        .current_dir(&root)
        .stdin(Stdio::null())
        .status()
        .expect("failed to run infigraph watch-stop");
    assert!(
        watch_stop_status.success(),
        "infigraph watch-stop exited non-zero: {watch_stop_status:?}"
    );

    let stopped = wait_until_gone("the daemon", || {
        infigraph_core::daemon::lifecycle::daemon_is_alive(&lock_path)
    })
    .is_some();
    assert!(
        stopped,
        "the legacy infigraph watch-stop alias should still bring the whole daemon process \
         down (parity with infigraph daemon-stop) -- if this now fails, either the sentinel \
         is no longer honored by run_write_coordinator, or its scope was deliberately \
         narrowed and this test's premise needs updating alongside that change"
    );

    // The process has already exited on its own; reap it so `wait()` inside
    // `KillOnDrop::drop` doesn't block, and so we don't send a redundant
    // kill signal to an already-dead pid.
    let _ = daemon.wait();
}

/// Real end-to-end test: the NEW nested `infigraph watch stop` subcommand
/// (`Commands::Watch { action: WatchCliAction::Stop }`, dispatched as
/// `cmd_watch_control(root, WatchRole::Code, Stop)` -- see main.rs's
/// `Commands::Watch { action } =>` arm) goes through the `WatchControl`
/// write-request protocol instead of the legacy sentinel file. This is the
/// command actually designed to decouple code-watching from the daemon
/// process (R2.4.4/R2.4.6 in docs/DESIGN-hardening.md) -- unlike the legacy
/// `watch-stop` alias covered by
/// `legacy_watch_stop_alias_brings_the_whole_daemon_down` above, this one
/// must leave the daemon's PID alive. `infigraph daemon-stop` is then used
/// to confirm the daemon can still be brought down cleanly afterward.
///
/// `crates/infigraph-core/tests/watch_control.rs` (a separate task in this
/// same plan) proves the same underlying guarantee by submitting the
/// `WatchControl` request directly in-process; this test complements it by
/// proving the guarantee holds through real clap argument parsing and
/// `main()` dispatch for the actual `infigraph watch stop` / `infigraph
/// daemon-stop` CLI invocations, not just the internal handler functions.
#[test]
fn watch_action_stop_leaves_the_daemon_process_alive() {
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

    let (mut daemon, lock_path) = spawn_ready_daemon(&bin, &root);
    let daemon_pid = daemon.id();

    let watch_stop_status = Command::new(&bin)
        .arg("watch")
        .arg("stop")
        .current_dir(&root)
        .stdin(Stdio::null())
        .status()
        .expect("failed to run infigraph watch stop");
    assert!(
        watch_stop_status.success(),
        "infigraph watch stop exited non-zero: {watch_stop_status:?}"
    );

    assert!(
        infigraph_core::daemon::lifecycle::daemon_is_alive(&lock_path),
        "infigraph watch stop must not bring the daemon process down -- \
         it only signals code-watching to stop, not the whole daemon"
    );
    assert!(
        infigraph_mcp::lifecycle::process_alive(daemon_pid),
        "daemon PID {daemon_pid} should still be alive after infigraph watch stop"
    );

    let daemon_stop_status = Command::new(&bin)
        .arg("daemon-stop")
        .current_dir(&root)
        .stdin(Stdio::null())
        .status()
        .expect("failed to run infigraph daemon-stop");
    assert!(
        daemon_stop_status.success(),
        "infigraph daemon-stop exited non-zero: {daemon_stop_status:?}"
    );

    let stopped = wait_until_gone("the daemon", || {
        infigraph_core::daemon::lifecycle::daemon_is_alive(&lock_path)
    })
    .is_some();
    assert!(
        stopped,
        "infigraph daemon-stop should have brought the daemon process down"
    );

    // The process has already exited on its own; reap it so `wait()` inside
    // `KillOnDrop::drop` doesn't block, and so we don't send a redundant
    // kill signal to an already-dead pid.
    let _ = daemon.wait();
}

/// Regression test for `KillOnDrop` itself: a panic while a spawned child is
/// wrapped in the guard must still kill and reap the child. This is the
/// scenario that used to leak real `infigraph daemon` processes above --
/// an `.expect()` panicking before the old explicit `child.kill()` call ran
/// left the spawned process orphaned forever, reparented to PID 1, holding
/// a real `.infigraph/watch.lock`. Proving Drop fires on the unwind path
/// (not just the normal return path) is the whole point of the fix.
#[cfg(unix)]
#[test]
fn kill_on_drop_kills_child_on_panic() {
    let child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("failed to spawn sleep");
    let pid = child.id();
    assert!(
        infigraph_mcp::lifecycle::process_alive(pid),
        "sleep process should be alive right after spawn"
    );

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = KillOnDrop(child);
        panic!("simulated panic while the guard is alive");
    }));
    assert!(unwound.is_err(), "expected the closure to panic");

    // Give the OS a brief moment to finish tearing the process down after
    // the guard's Drop sends the kill signal during unwind.
    let stopped = wait_until_gone("the child process", || {
        infigraph_mcp::lifecycle::process_alive(pid)
    })
    .is_some();
    assert!(
        stopped,
        "KillOnDrop must kill child pid {pid} even when dropped during a panic unwind"
    );
}

/// `infigraph daemon` had no panic hook at all before R3.1.4's exit-reason
/// logging pass: a panic anywhere in the process unwound silently, leaving
/// the same blank trail an uncatchable SIGKILL does. `INFIGRAPH_TEST_DAEMON_PANIC`
/// forces a deterministic panic right after the hook installs (mirroring
/// infigraph-mcp's `FORCE_COMPRESS_PANIC`/`FORCE_DEDUP_PANIC`) so this test
/// doesn't depend on finding a real internal panic to trigger.
#[test]
fn a_panic_in_the_daemon_process_is_logged_before_it_exits() {
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

    let mut daemon = KillOnDrop(
        Command::new(&bin)
            .arg("daemon")
            .current_dir(&root)
            .env("INFIGRAPH_TEST_DAEMON_PANIC", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn infigraph daemon"),
    );

    let stderr = daemon.stderr.take().unwrap();
    let captured = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        BufReader::new(stderr).read_to_string(&mut buf).ok();
        buf
    });

    let status = daemon.wait().expect("failed to wait on daemon process");
    assert!(
        !status.success(),
        "a forced panic must not exit as if nothing happened, got {status:?}"
    );

    let captured = captured.join().expect("stderr reader thread panicked");
    assert!(
        captured.contains("[daemon] PANIC:")
            && captured.contains("INFIGRAPH_TEST_DAEMON_PANIC forced panic"),
        "expected the panic hook's log line on stderr, got: {captured}"
    );
}

/// Real end-to-end regression test for the "doc-watcher can't recover from
/// the `watch.stop.docs` sentinel" bug: the MCP `stop_watch_docs` tool
/// (`tool_stop_watch_docs` in `infigraph-mcp`) stops daemon-mode doc
/// watching by writing `.infigraph/watch.stop.docs` directly, rather than
/// going through the `WatchControl` protocol `infigraph watch-docs stop`
/// uses. That sentinel is consumed by `watch_docs_daemon_loop`
/// (`infigraph-docs/src/watch.rs`) without ever exiting its thread -- it
/// just sets an internal `suppressed_until_absent` flag and keeps polling.
/// Before the fix, `DocWatchThread::start()` (`infigraph-cli`) returned
/// early whenever `self.handle.is_some()`, which stays true the whole time
/// (the thread never actually exits), so `infigraph watch-docs start` /
/// `enable_watch_docs` was a permanent silent no-op after this sentinel
/// path. This test writes that exact sentinel directly (there is no CLI
/// command that does -- only the MCP tool does, but the on-disk contract is
/// identical and this avoids spinning up a whole MCP server) against a real
/// daemon subprocess, then proves `infigraph watch-docs start` genuinely
/// resumes doc-watching afterward.
#[test]
fn watch_docs_start_resumes_after_a_sentinel_triggered_stop() {
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
    // Pre-create docs.kuzu so the daemon's doc thread attaches on its very
    // first poll tick, rather than needing a second synchronization point
    // for "docs.kuzu appeared mid-run" (already covered by
    // cmd_watch_daemon_also_indexes_docs_without_restart above).
    infigraph_docs::DocIndex::open(&root)
        .unwrap()
        .init()
        .unwrap();

    let mut daemon = KillOnDrop(
        Command::new(&bin)
            .arg("daemon")
            .arg("--debounce")
            .arg("50")
            .env("INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS", "50")
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn infigraph daemon"),
    );

    let (attach_tx, attach_rx) = mpsc::channel::<()>();
    let (reindexed_tx, reindexed_rx) = mpsc::channel::<String>();

    let stdout = daemon.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            eprintln!("[daemon stdout] {line}");
        }
    });
    let stderr = daemon.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[daemon stderr] {line}");
            if line.contains("attaching doc watcher") {
                let _ = attach_tx.send(());
            }
            if line.contains("reindexed:") {
                let _ = reindexed_tx.send(line);
            }
        }
    });

    let lock_path = root.join(".infigraph").join("watch.lock");
    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(
            &lock_path,
            Duration::from_secs(10)
        ),
        "daemon never acquired watch.lock after spawn"
    );

    attach_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("daemon's doc thread never attached on startup (docs.kuzu pre-existed)");
    std::thread::sleep(Duration::from_millis(200));

    // Baseline: prove doc-watching genuinely works before touching the
    // sentinel at all.
    std::fs::write(root.join("readme.md"), "# hello\n\nsome content").unwrap();
    reindexed_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("baseline reindex before any stop must succeed");

    // Simulate tool_stop_watch_docs's daemon-mode write directly.
    std::fs::write(root.join(".infigraph").join("watch.stop.docs"), b"").unwrap();
    let sentinel = root.join(".infigraph").join("watch.stop.docs");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while sentinel.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !sentinel.exists(),
        "sentinel must be consumed by the running daemon"
    );

    // Confirm it's genuinely suppressed: a doc written now must NOT be
    // reindexed within a bounded wait (docs.kuzu never disappeared, so this
    // is not a false negative from a detach/reattach race).
    std::fs::write(root.join("while-stopped.md"), "# should not be indexed").unwrap();
    assert!(
        reindexed_rx.recv_timeout(Duration::from_secs(2)).is_err(),
        "doc watching must stay stopped after the sentinel -- got an unexpected reindex"
    );

    // The actual regression check: infigraph watch-docs start must resume
    // doc-watching despite the daemon's doc thread never having exited.
    let start_status = Command::new(&bin)
        .arg("watch-docs")
        .arg("start")
        .current_dir(&root)
        .stdin(Stdio::null())
        .status()
        .expect("failed to run infigraph watch-docs start");
    assert!(
        start_status.success(),
        "infigraph watch-docs start exited non-zero: {start_status:?}"
    );

    attach_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("doc watcher must re-attach after infigraph watch-docs start");
    std::thread::sleep(Duration::from_millis(200));

    std::fs::write(root.join("after-restart.md"), "# should be indexed now").unwrap();
    let resumed = reindexed_rx.recv_timeout(Duration::from_secs(10));

    let _ = daemon.kill();
    let _ = daemon.wait();

    assert!(
        resumed.is_ok(),
        "infigraph watch-docs start must actually resume doc-watching after a prior \
         sentinel-triggered stop, not silently no-op"
    );
}
