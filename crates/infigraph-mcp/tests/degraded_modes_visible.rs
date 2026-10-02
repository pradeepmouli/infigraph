//! #75, end to end: a fallback taken inside the daemon is visible to the
//! person, in `infigraph doctor`, in the MCP `get_stats` tool and in a tool
//! footer, and stops being reported once its cause is gone. Before this the
//! only trace was a line in `.infigraph/daemon.log`.
//!
//! Real processes throughout: the `infigraph` CLI as daemon and as `doctor`,
//! and an `infigraph-mcp` supervisor and worker reading through that daemon.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

mod support;

use support::{start_supervisor, Server};

/// Debug builds construct the language registry on first use.
const BUDGET: Duration = Duration::from_secs(120);

/// The words every surface must show: the mode's own, from core.
fn doc_reads_unavailable() -> String {
    let message = infigraph_core::degraded::DegradedMode::DocReadsUnavailable {
        reason: String::new(),
    }
    .message();
    message.trim_end_matches([':', ' ']).to_string()
}

/// This build's `infigraph` CLI, or `None` when the target dir has none
/// (infigraph-mcp does not depend on infigraph-cli, so cargo does not build
/// it for this crate's tests alone). One from another build is a failure,
/// not a skip: its daemon would be testing different code (#141).
fn cli() -> Option<PathBuf> {
    let cli = infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    )
    .ok()?;
    let cli_hash = infigraph_core::daemon::installed_build_hash_of(&cli)
        .unwrap_or_else(|| panic!("{} did not report a build hash", cli.display()));
    assert_eq!(
        cli_hash,
        infigraph_core::build_hash(),
        "stale `infigraph` CLI at {}: run `cargo build -p infigraph-cli` (same profile) first",
        cli.display()
    );
    Some(cli)
}

fn command(cli: &Path, root: &Path, home: &Path, backend: &str) -> Command {
    let mut cmd = Command::new(cli);
    cmd.current_dir(root)
        .env("HOME", home)
        .env("INFIGRAPH_REGISTRY_HOME", home)
        .env(infigraph_core::BACKEND_ENV, backend)
        .env(infigraph_core::scip_switch::enabled_env_name(), "0")
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_DOCS_ENABLED")
        .env_remove("INFIGRAPH_WATCH_DAEMON");
    cmd
}

fn run(cli: &Path, root: &Path, home: &Path, backend: &str, args: &[&str]) -> Output {
    command(cli, root, home, backend)
        .args(args)
        .output()
        .unwrap()
}

/// Kills and reaps the daemon this test started, on every exit path.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_daemon(cli: &Path, root: &Path, home: &Path) -> Daemon {
    let daemon = Daemon(
        command(cli, root, home, infigraph_core::LOCAL_BACKEND)
            .args(["daemon", "--debounce", "50"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(
            &root.join(".infigraph").join("watch.lock"),
            BUDGET
        ),
        "the daemon never took watch.lock"
    );
    daemon
}

fn doctor(cli: &Path, root: &Path, home: &Path) -> String {
    let out = run(cli, root, home, "daemon", &["doctor"]);
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn tool_text(server: &mut Server, id: i64, tool: &str, arguments: Value) -> String {
    server.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                       "params": {"name": tool, "arguments": arguments}}));
    let reply = server.reply(id, BUDGET);
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{tool} did not answer with text: {reply}"))
        .to_string()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

#[test]
fn a_fallback_inside_the_daemon_is_visible_in_doctor_get_stats_and_the_footer() {
    let Some(cli) = cli() else {
        eprintln!("skipping: infigraph CLI binary not built in this target dir");
        return;
    };
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let home = home.path();
    std::fs::write(root.join("hello.py"), "def hello():\n    pass\n").unwrap();
    let index = run(
        &cli,
        &root,
        home,
        infigraph_core::LOCAL_BACKEND,
        &["index", "--no-embed"],
    );
    assert!(index.status.success(), "index failed: {index:?}");

    // A document store that exists and will not open: the same on every
    // platform (it is refused by size before the database sees it). With
    // documents switched off nothing rebuilds it behind the test's back; the
    // doc watcher would otherwise wipe and recreate it within a poll.
    let infigraph_dir = root.join(".infigraph");
    std::fs::write(
        infigraph_dir.join("config.toml"),
        "[docs]\nenabled = false\n",
    )
    .unwrap();
    let docs_store = infigraph_dir.join("docs.kuzu");
    std::fs::write(&docs_store, b"not a database").unwrap();

    let daemon = start_daemon(&cli, &root, home);
    let wanted = doc_reads_unavailable();
    let key = infigraph_core::degraded::DOC_READS_UNAVAILABLE;

    let report = doctor(&cli, &root, home);
    assert!(
        report.contains(key) && report.contains(&wanted),
        "doctor does not show the daemon's fallback:\n{report}"
    );

    let mut server = start_supervisor(&[(infigraph_core::BACKEND_ENV, "daemon")]);
    let path = json!({"path": root.to_string_lossy()});
    let stats = tool_text(&mut server, 1, "get_stats", path.clone());
    assert!(
        stats.contains("Degraded modes:") && stats.contains(&wanted),
        "get_stats does not show the daemon's fallback:\n{stats}"
    );
    let listing = tool_text(&mut server, 2, "list_files", path.clone());
    assert!(
        listing.contains(&format!("⚠ {wanted}")),
        "the tool footer does not show the daemon's fallback:\n{listing}"
    );

    // The footer says it once; `get_stats`, which was asked, every time.
    let again = tool_text(&mut server, 4, "list_files", path.clone());
    assert!(
        !again.contains(&wanted),
        "the footer repeats a lasting mode on the next call:\n{again}"
    );
    let stats = tool_text(&mut server, 5, "get_stats", path.clone());
    assert!(
        stats.contains(&wanted),
        "get_stats stopped listing it:\n{stats}"
    );

    // What the footer's lookup costs a tool call, against this healthy
    // daemon: asked afresh (what `doctor` and `get_stats` do), and through
    // the cache the footer uses. Printed, and bounded loosely enough for a
    // loaded machine: the point is that neither is a wait.
    let time = |f: &dyn Fn()| {
        median(
            (0..50)
                .map(|_| {
                    let started = Instant::now();
                    f();
                    started.elapsed()
                })
                .collect(),
        )
    };
    let fresh = time(&|| drop(infigraph_core::degraded::gather(&root)));
    let cached = time(&|| drop(infigraph_core::degraded::gather_cached(&root)));
    eprintln!("gather: {fresh:?} per call asked afresh, {cached:?} through the footer's cache");
    assert!(
        cached < Duration::from_millis(50),
        "the footer's lookup costs {cached:?} per tool call"
    );

    // The cause goes away: the store is removed and the daemon restarted.
    // Every surface stops reporting it.
    drop(daemon);
    std::fs::remove_file(&docs_store).unwrap();
    let _daemon = start_daemon(&cli, &root, home);

    let report = doctor(&cli, &root, home);
    assert!(
        !report.contains(key),
        "doctor still reports a fallback whose cause is gone:\n{report}"
    );
    let stats = tool_text(&mut server, 3, "get_stats", path);
    assert!(
        !stats.contains(&wanted),
        "get_stats still reports a fallback whose cause is gone:\n{stats}"
    );
}
