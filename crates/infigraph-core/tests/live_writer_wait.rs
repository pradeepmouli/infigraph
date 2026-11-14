//! A live, identified writer of the graph is a wait for the daemon, not a
//! fault. Each test runs the real `infigraph daemon` (its watcher and
//! producer included) against a graph that a child process holds as its
//! writer, and that names itself in `index.lock` the way a `scip-enrich`
//! does. The holder is this test binary run again, with `--exact`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use infigraph_core::daemon::writes::{submit, WriteOpts};
use infigraph_core::daemon_protocol::WriteRequest;
use infigraph_core::graph::store::GraphStore;
use infigraph_core::lockfile;

const HOLDER_ROOT: &str = "INFIGRAPH_TEST_LIVE_WRITER_ROOT";
const HOLDER_READY: &str = "live-writer.ready";
const HOLDER_RELEASE: &str = "live-writer.release";
const DAEMON_BACKEND: &str = "daemon";

/// The child the tests run. It is not a test of its own: it does nothing
/// unless `HOLDER_ROOT` is set. When it runs, it opens the graph as its writer,
/// names itself in `index.lock`, and holds both until the test writes
/// `HOLDER_RELEASE`.
#[test]
fn live_writer_holder_process() {
    let Some(root) = std::env::var_os(HOLDER_ROOT) else {
        return;
    };
    let root = PathBuf::from(root);
    let infigraph_dir = root.join(".infigraph");
    let _writer = GraphStore::open(&infigraph_dir.join(infigraph_core::graph::names::STEM))
        .expect("the holder opens the graph as its writer");
    let _identity = lockfile::try_acquire(&infigraph_dir.join("index.lock"), "scip-enrich")
        .expect("index.lock can be taken")
        .expect("index.lock is free");
    std::fs::write(root.join(HOLDER_READY), b"").unwrap();
    wait_for(&root.join(HOLDER_RELEASE), Duration::from_secs(120));
}

mod common;
use common::cli::cli_command;

/// Waits for `path` to appear, failing once `budget` is spent. A handshake
/// with a child process, not a guess at how long it takes.
fn wait_for(path: &Path, budget: Duration) {
    let deadline = Instant::now() + budget;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_holder(root: &Path) -> Child {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "live_writer_holder_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HOLDER_ROOT, root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the holder");
    wait_for(&root.join(HOLDER_READY), Duration::from_secs(120));
    child
}

fn release(root: &Path, holder: &mut Child) {
    std::fs::write(root.join(HOLDER_RELEASE), b"").unwrap();
    let status = holder.wait().unwrap();
    assert!(status.success(), "the holder exits cleanly once released");
}

/// A project indexed once, before anyone else opens its graph.
fn indexed_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.py"), "def main(): pass\n").unwrap();
    let out = cli_command()
        .arg("index")
        .current_dir(dir.path())
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

/// A real `infigraph daemon` for `root`, killed when dropped.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The daemon's stderr, where its degraded-mode lines land.
fn daemon_stderr(root: &Path) -> PathBuf {
    root.join("daemon-stderr.log")
}

fn start_daemon(root: &Path) -> Daemon {
    let log = std::fs::File::create(daemon_stderr(root)).unwrap();
    let child = cli_command()
        .arg("daemon")
        .current_dir(root)
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let daemon = Daemon(child);
    let lock = root.join(".infigraph").join("watch.lock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(lock.exists() && std::fs::metadata(&lock).unwrap().len() > 0) {
        assert!(
            Instant::now() < deadline,
            "the daemon never acquired watch.lock"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon
}

/// Waits until the daemon's stderr says `needle`, failing after `budget`.
fn wait_for_daemon_log(root: &Path, needle: &str, budget: Duration) {
    let log = daemon_stderr(root);
    let deadline = Instant::now() + budget;
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never said {needle:?}; its stderr so far:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn fault_latched(root: &Path) -> bool {
    infigraph_core::daemon::fault::fault_path(&root.join(".infigraph")).exists()
}

fn finds(root: &Path, symbol: &str) -> bool {
    let out = cli_command()
        .args(["search", symbol])
        .current_dir(root)
        .env(infigraph_core::BACKEND_ENV, DAEMON_BACKEND)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains(symbol)
}

/// A write that cannot be served while the holder holds the graph times out
/// with an error that names the holder, from the daemon's own wording.
#[test]
fn a_write_that_times_out_while_a_live_writer_holds_the_graph_names_it() {
    let dir = indexed_project();
    let mut holder = spawn_holder(dir.path());
    let _daemon = start_daemon(dir.path());
    let who = format!("scip-enrich (PID {})", holder.id());
    wait_for_daemon_log(dir.path(), &who, Duration::from_secs(60));

    let err = submit(
        dir.path(),
        &WriteRequest::RemoveFiles { files: Vec::new() },
        WriteOpts {
            timeout: Duration::from_secs(3),
            cancel: None,
            on_slot_wait: None,
        },
    )
    .expect_err("the write cannot be served while the holder holds the graph");
    let text = format!("{err:#}");
    assert!(text.contains(&who), "the timeout names the holder: {text}");
    assert!(
        !fault_latched(dir.path()),
        "the timed-out write latched no fault"
    );
    release(dir.path(), &mut holder);
}

/// A file added while a live identified writer holds the graph is indexed
/// once the holder lets go, with no latched fault at any point. This is the
/// claim the wait rests on: the wait converges without a restart.
#[test]
fn a_file_added_while_a_live_writer_holds_the_graph_is_indexed_once_it_lets_go() {
    let dir = indexed_project();
    let mut holder = spawn_holder(dir.path());
    let _daemon = start_daemon(dir.path());
    std::fs::write(
        dir.path().join("drifted.py"),
        "def drifted_during_downtime(): return 'late'\n",
    )
    .unwrap();
    let who = format!("scip-enrich (PID {})", holder.id());
    wait_for_daemon_log(dir.path(), &who, Duration::from_secs(60));
    assert!(
        !fault_latched(dir.path()),
        "a live writer must not latch a fault while it holds the graph"
    );

    release(dir.path(), &mut holder);

    let deadline = Instant::now() + Duration::from_secs(180);
    while !finds(dir.path(), "drifted_during_downtime") {
        assert!(
            Instant::now() < deadline,
            "the file never reached the graph after the holder let go"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        !fault_latched(dir.path()),
        "no fault is latched by the time the file is indexed"
    );
}
