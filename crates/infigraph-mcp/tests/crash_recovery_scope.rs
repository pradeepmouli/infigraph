//! #20 (R5.5): a crashed MCP worker must not cost any project its graph.
//!
//! The supervisor used to answer a worker SIGSEGV by wiping and reindexing
//! the startup directory, every registered repo and every group (I-14).
//! Since #159 the worker does not open code graphs -- reads route to each
//! project's daemon -- so a worker crash says nothing about any graph, and
//! the wipe only ever destroyed healthy ones. Graph damage is the daemon's
//! to detect and repair (R3.1.4).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const GRAPH_BYTES: &[u8] = b"a graph the crash must not touch";

struct Fixture {
    _tmp: tempfile::TempDir,
    startup: PathBuf,
    bystander: PathBuf,
    instances: PathBuf,
    log: PathBuf,
    child: Child,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A supervisor launched in `startup`, with `bystander` registered.
fn start() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let startup = root.join("startup");
    let bystander = root.join("bystander");
    for project in [&startup, &bystander] {
        std::fs::create_dir_all(project.join(".infigraph")).unwrap();
        std::fs::write(project.join(".infigraph").join("graph"), GRAPH_BYTES).unwrap();
    }
    let registry_home = root.join("registry-home");
    std::fs::create_dir_all(&registry_home).unwrap();
    std::fs::write(
        registry_home.join("registry.json"),
        serde_json::json!({
            "repos": {"bystander": {
                "name": "bystander",
                "path": bystander,
                "languages": [],
                "symbol_count": 0,
                "module_count": 0
            }},
            "groups": {}
        })
        .to_string(),
    )
    .unwrap();
    let instances = root.join("instances");
    std::fs::create_dir_all(&instances).unwrap();
    let log = root.join("mcp.log");

    let child = Command::new(env!("CARGO_BIN_EXE_infigraph-mcp"))
        .arg("--mcp")
        .current_dir(&startup)
        .env("HOME", root.join("home"))
        .env("INFIGRAPH_REGISTRY_HOME", &registry_home)
        .env("INFIGRAPH_MCP_LOCK_PATH", root.join("mcp.lock"))
        .env("INFIGRAPH_REGISTRY_INSTANCES_DIR", &instances)
        .env("INFIGRAPH_MCP_LOG_PATH", &log)
        .env("CI", "true")
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn infigraph-mcp supervisor");

    Fixture {
        _tmp: tmp,
        startup,
        bystander,
        instances,
        log,
        child,
    }
}

/// A registered worker pid other than `not`, if one is registered now.
fn registered_worker(instances: &Path, not: Option<u32>) -> Option<u32> {
    std::fs::read_dir(instances)
        .ok()?
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .filter_map(|v| v["pid"].as_u64().map(|p| p as u32))
        .find(|p| Some(*p) != not)
}

/// Wait for the next worker to register (`Ok`) or the supervisor to exit
/// (`Err`), whichever comes first.
fn next_worker(f: &mut Fixture, not: Option<u32>) -> Result<u32, ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(pid) = registered_worker(&f.instances, not) {
            return Ok(pid);
        }
        if let Some(status) = f.child.try_wait().unwrap() {
            return Err(status);
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "neither a new worker nor a supervisor exit within 60s:\n{}",
            std::fs::read_to_string(&f.log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Kill `pid` with SIGSEGV, as a real memory fault would.
///
/// One signal is not enough. Rust's std installs a SIGSEGV handler to catch
/// stack overflows; for a fault outside a guard page it restores the default
/// disposition and returns, relying on the faulting instruction to fault
/// again. A signal from `kill(2)` has no faulting instruction, so the first
/// one is swallowed and only the next one kills.
fn segfault(pid: u32) {
    for _ in 0..5 {
        // SAFETY: plain kill(2) on a worker this test's own supervisor spawned.
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGSEGV) } != 0 {
            return; // gone
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    panic!("worker {pid} survived repeated SIGSEGV");
}

/// Whether `project`'s graph is still exactly the fixture's bytes.
fn graph_untouched(project: &Path) -> bool {
    std::fs::read(project.join(".infigraph").join("graph"))
        .ok()
        .as_deref()
        == Some(GRAPH_BYTES)
}

#[test]
fn a_worker_crash_leaves_every_projects_graph_alone_and_respawns_the_worker() {
    let mut f = start();
    let first = next_worker(&mut f, None).expect("the first worker");

    segfault(first);
    let respawned = next_worker(&mut f, Some(first));

    assert!(
        respawned.is_ok(),
        "one crash must be answered with a new worker: {respawned:?}"
    );
    assert!(
        graph_untouched(&f.startup),
        "the crash wiped the startup project's graph"
    );
    assert!(
        graph_untouched(&f.bystander),
        "the crash wiped a registered bystander's graph"
    );
    let log = std::fs::read_to_string(&f.log).unwrap_or_default();
    assert!(
        log.contains("worker crashed"),
        "the crash must still be logged: {log}"
    );
}

#[test]
fn a_crash_looping_worker_stops_the_supervisor_instead_of_respawning_forever() {
    let mut f = start();
    let mut pid = next_worker(&mut f, None).expect("the first worker");
    let mut crashes = 0;
    let status = loop {
        segfault(pid);
        crashes += 1;
        match next_worker(&mut f, Some(pid)) {
            Ok(next) => pid = next,
            Err(status) => break status,
        }
        assert!(
            crashes < 10,
            "the supervisor kept respawning a crash-looping worker"
        );
    };

    assert!(
        !status.success(),
        "giving up must be a failure exit: {status:?}"
    );
    let log = std::fs::read_to_string(&f.log).unwrap_or_default();
    assert!(log.contains("crash loop"), "say why it stopped: {log}");
    assert!(
        graph_untouched(&f.startup),
        "the crash loop touched startup's graph"
    );
    assert!(
        graph_untouched(&f.bystander),
        "the crash loop touched bystander's graph"
    );
}
