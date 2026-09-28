//! Shared teardown and isolation for infigraph-mcp integration tests (#136).
//!
//! `tool_index_project` shells out to `infigraph index`, whose auto-watch
//! spawns a *detached* `infigraph daemon` for the project and registers the
//! project in the global registry. Neither goes away with the test on its
//! own: the daemon keeps writing into `.infigraph/` while the `TempDir` is
//! being removed, resurrects the directory through `create_dir_all`, and
//! then watches a directory nobody owns forever (415 such tempdirs and a
//! dozen daemons were found on one dev machine), and the registry fills with
//! `.tmpXXXXXX` entries that `infigraph doctor` reports as removed worktrees.
//!
//! Two levers, pick per test binary:
//! - [`disable_background_watchers`] for binaries whose subject is not
//!   watching at all (process-global opt-out, so a binary that calls it must
//!   not also test auto-start).
//! - [`TestProject`] (or [`stop_daemon_for`]) for the watcher tests, where a
//!   daemon is the point and has to be stopped before its root is removed.
//!
//! Both isolate the registry ([`isolate_registry`]).

#![allow(dead_code)] // each test binary uses the subset it needs

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Opt this whole test process out of background watchers. The CLI's
/// auto-watch and the MCP's own auto-start both honour `INFIGRAPH_NO_WATCH`
/// (through `daemon::lifecycle::is_ci_env`), and the variable is inherited
/// by every `infigraph index` child this process spawns.
pub fn disable_background_watchers() {
    std::env::set_var("INFIGRAPH_NO_WATCH", "1");
    isolate_registry();
}

/// Point `INFIGRAPH_REGISTRY_HOME` at a per-process scratch directory so
/// neither this process nor the CLI children it spawns write test projects
/// into `~/.infigraph/registry.json`. Idempotent; returns the scratch home.
///
/// The scratch directories are named by pid and swept when older than an
/// hour, so the residue this keeps out of the registry does not simply move
/// to the temp dir.
pub fn isolate_registry() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let base = std::env::temp_dir();
        sweep_stale_registry_homes(&base);
        let home = base.join(format!("{REGISTRY_HOME_PREFIX}{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("registry scratch dir");
        std::env::set_var("INFIGRAPH_REGISTRY_HOME", &home);
        // In-process tools log their failures (index timeouts, a clone's
        // failed edges) through `mcp_log`; keep those out of the real
        // `mcp.log` too (#201). Only the log: the lock stays per-test, via
        // `McpStateScope` or `isolated_mcp_command`.
        std::env::set_var("INFIGRAPH_MCP_LOG_PATH", home.join("mcp.log"));
        home
    })
}

const REGISTRY_HOME_PREFIX: &str = "infigraph-test-registry-";
const REGISTRY_HOME_STALE_AFTER: Duration = Duration::from_secs(60 * 60);

fn sweep_stale_registry_homes(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(REGISTRY_HOME_PREFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > REGISTRY_HOME_STALE_AFTER);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Remove `path` when the test process exits. For the `OnceLock`-held shared
/// fixtures some binaries build once for all their tests: a static is never
/// dropped, so its `TempDir` would survive the run.
pub fn remove_at_exit(path: &Path) {
    use std::sync::Mutex;
    static PATHS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    if let Ok(mut paths) = PATHS.lock() {
        paths.push(path.to_path_buf());
    }

    // `libc` is a unix-only dependency of this crate (see Cargo.toml's
    // `[target.'cfg(unix)'.dependencies]`), so this whole registration --
    // the handler and the OnceLock guarding it included -- is unix-only.
    // Windows leaves the temp directories to the OS: this hook is a
    // tidiness measure for long-lived `OnceLock` fixtures, not a
    // correctness guarantee any test depends on.
    #[cfg(unix)]
    {
        static REGISTERED: OnceLock<()> = OnceLock::new();

        extern "C" fn cleanup() {
            if let Ok(paths) = PATHS.lock() {
                for p in paths.iter() {
                    stop_daemon_for(p);
                    let _ = std::fs::remove_dir_all(p);
                }
            }
        }

        REGISTERED.get_or_init(|| {
            // SAFETY: `cleanup` is a plain `extern "C" fn()` with no arguments,
            // exactly what atexit expects; it runs once, at process exit.
            unsafe {
                libc::atexit(cleanup);
            }
        });
    }
}

/// A project directory whose daemon is stopped before the directory is
/// removed. Drop-in for the bare `tempfile::TempDir` the fixtures used to
/// hand back: `path()` is the same call.
pub struct TestProject {
    dir: Option<tempfile::TempDir>,
    root: PathBuf,
}

impl TestProject {
    pub fn new() -> Self {
        isolate_registry();
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let root = dir.path().to_path_buf();
        Self {
            dir: Some(dir),
            root,
        }
    }

    /// `new()` plus `(relative path, contents)` files written under it.
    pub fn with_files(files: &[(&str, &str)]) -> Self {
        let project = Self::new();
        for (name, content) in files {
            let p = project.root.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }
        project
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn path_string(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }
}

impl Default for TestProject {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestProject {
    fn drop(&mut self) {
        let had_in_process_watcher = stop_in_process_watchers_for(&self.root);
        stop_daemon_for(&self.root);
        remove_tree(&self.root);
        // An in-process watcher that was auto-started at the very end of a
        // test (search does that) may not have taken `watch.lock` yet when
        // it was stopped above, and its startup then reopens the graph under
        // the removed root. Give it a moment and take the tree down again.
        if had_in_process_watcher {
            for _ in 0..4 {
                std::thread::sleep(Duration::from_millis(250));
                if !self.root.exists() {
                    break;
                }
                stop_daemon_for(&self.root);
                remove_tree(&self.root);
            }
        }
        drop(self.dir.take());
    }
}

/// `remove_dir_all` with a short retry: a watcher thread mid-write can make
/// one attempt fail even after it has been told to stop.
fn remove_tree(root: &Path) {
    for _ in 0..10 {
        if std::fs::remove_dir_all(root).is_ok() || !root.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How long a stopped in-process code watcher may take to release
/// `watch.lock`. Its teardown measured ~2s on an idle dev machine and
/// ~3.5s at load 15. The per-binary copies of this wait used 5s and then
/// gave up silently, so the next `watch_project` on the root failed with
/// "another watcher is already running" -- held by this very process's
/// own stopping watcher. Generous, because running out is a failure,
/// never a reason to carry on.
pub const WATCHER_STOP_BUDGET: Duration = Duration::from_secs(60);

/// How long to wait for a freshly started watcher's first reaction to a
/// change. It covers registering with the OS first: on a loaded macOS
/// machine that is an RPC to a backed-up `fseventsd`, and a doc watcher
/// was seen still inside it after 15s, the old budget, before it had even
/// run its catch-up reindex. A poll returns as soon as the change lands,
/// so a generous budget costs nothing when the machine is quiet.
pub const WATCH_EVENT_BUDGET: Duration = Duration::from_secs(60);

/// Stop every in-process code watcher (the WATCHERS map: what
/// `tool_watch_project` / `auto_start_watch` run on a thread of this
/// process) and wait for each root's `watch.lock` to be released, so a
/// test that watches the same root next is not racing the shutdown.
pub fn stop_all_watchers() {
    let roots: Vec<String> = {
        let mut guard = infigraph_mcp::tools::watch::get_watchers();
        guard
            .as_mut()
            .map(|map| {
                map.drain()
                    .map(|(_, entry)| {
                        let _ = entry.stop_tx.send(());
                        entry.path
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    for root in roots {
        await_watch_lock_released(Path::new(&root));
    }
}

/// Wait for a watcher that was told to stop to release `root`'s
/// `watch.lock`. Panics naming the holder if it outlives
/// [`WATCHER_STOP_BUDGET`] (only reports it when already unwinding, e.g.
/// from a cleanup guard, where a second panic would abort).
pub fn await_watch_lock_released(root: &Path) {
    let lock = root.join(".infigraph").join("watch.lock");
    if wait_until_released(&lock, WATCHER_STOP_BUDGET) {
        return;
    }
    let msg = format!(
        "a stopped watcher still holds {} after {WATCHER_STOP_BUDGET:?}: {:?}",
        lock.display(),
        infigraph_core::lockfile::read_holder(&lock).map(|h| (h.role, h.pid))
    );
    if std::thread::panicking() {
        eprintln!("[support] {msg}");
    } else {
        panic!("{msg}");
    }
}

/// Stop every in-process doc watcher. Doc watchers do not hold
/// `watch.lock` (only code watchers do), so there is nothing to wait for.
pub fn stop_all_doc_watchers() {
    let mut guard = infigraph_mcp::tools::docs::get_doc_watchers();
    if let Some(map) = guard.as_mut() {
        for (_, entry) in map.drain() {
            let _ = entry.stop_tx.send(());
        }
    }
}

/// Stop every in-process MCP watcher registered for `root` (the ones
/// `tool_watch_project` / `auto_start_watch` run on a thread of this
/// process) and wait for `watch.lock` to be released. Returns whether any
/// was registered.
pub fn stop_in_process_watchers_for(root: &Path) -> bool {
    let wanted = root.to_string_lossy().into_owned();
    let canonical = root
        .canonicalize()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| wanted.clone());
    let mut found = false;
    {
        let mut guard = infigraph_mcp::tools::watch::get_watchers();
        if let Some(map) = guard.as_mut() {
            let ids: Vec<String> = map
                .iter()
                .filter(|(_, e)| e.path == wanted || e.path == canonical)
                .map(|(id, _)| id.clone())
                .collect();
            for id in ids {
                if let Some(entry) = map.remove(&id) {
                    let _ = entry.stop_tx.send(());
                    found = true;
                }
            }
        }
    }
    if found {
        wait_until_released(
            &root.join(".infigraph").join("watch.lock"),
            WATCHER_STOP_BUDGET,
        );
    }
    found
}

/// Stop whatever holds `<root>/.infigraph/watch.lock`: the `watch.stop`
/// sentinel first (the coordinator's cooperative exit, honoured by detached
/// daemons and in-process watchers alike), then SIGTERM to the recorded
/// holder if the lock is still held, then SIGKILL. Never signals this
/// process itself (an in-process watcher's holder pid is our own).
pub fn stop_daemon_for(root: &Path) {
    use infigraph_core::daemon::lifecycle::daemon_is_alive;

    let infigraph_dir = root.join(".infigraph");
    let lock = infigraph_dir.join("watch.lock");
    if !daemon_is_alive(&lock) {
        return;
    }
    let _ = std::fs::write(infigraph_dir.join("watch.stop"), "");
    if wait_until_released(&lock, Duration::from_secs(10)) {
        return;
    }
    let Some(holder) = infigraph_core::lockfile::read_holder(&lock) else {
        return;
    };
    if holder.pid == std::process::id() {
        return;
    }
    eprintln!(
        "[support] daemon {} for {} ignored watch.stop -- terminating it",
        holder.pid,
        root.display()
    );
    let _ = infigraph_core::ps::kill_infigraph_process(holder.pid, false);
    if wait_until_released(&lock, Duration::from_secs(5)) {
        return;
    }
    let _ = infigraph_core::ps::kill_infigraph_process(holder.pid, true);
    wait_until_released(&lock, Duration::from_secs(5));
}

fn wait_until_released(lock: &Path, budget: Duration) -> bool {
    use infigraph_core::daemon::lifecycle::daemon_is_alive;
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if !daemon_is_alive(lock) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !daemon_is_alive(lock)
}

/// The env overrides that move infigraph-mcp's per-user files --
/// `~/.infigraph/mcp.lock` and `~/.infigraph/mcp.log` -- under `dir`. The
/// one list every isolation path draws from, spawned or in-process, so a
/// test can never isolate the lock and still write into the developer's
/// real log (#201): fabricated takeover/handover lines there sit beside the
/// real ones and mislead the next lock investigation.
pub fn mcp_state_env(dir: &Path) -> [(&'static str, PathBuf); 2] {
    [
        ("INFIGRAPH_MCP_LOCK_PATH", dir.join("mcp.lock")),
        ("INFIGRAPH_MCP_LOG_PATH", dir.join("mcp.log")),
    ]
}

/// [`mcp_state_env`] under a fresh directory for code running inside the
/// test process: sets every override on construction and restores the
/// previous values on drop. The variables are process-global, so callers
/// serialize on their own env lock.
pub struct McpStateScope {
    dir: tempfile::TempDir,
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl McpStateScope {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir for mcp state");
        let previous = mcp_state_env(dir.path())
            .into_iter()
            .map(|(key, value)| {
                let old = std::env::var_os(key);
                std::env::set_var(key, value);
                (key, old)
            })
            .collect();
        Self { dir, previous }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for McpStateScope {
    fn drop(&mut self) {
        for (key, old) in &self.previous {
            match old {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// The real `infigraph-mcp` binary, unable to touch the developer's own
/// server (#197). Without its own `INFIGRAPH_MCP_LOCK_PATH` a spawned
/// worker competes for the live `~/.infigraph/mcp.lock`, and because a test
/// build's hash never matches the installed one, it asks the live server to
/// hand over -- which then exits -- and burns its own startup budget
/// waiting on the takeover. Everything a worker writes outside its project
/// goes under `scratch`: lock, log, instance registry, registry home and
/// `HOME`; background watchers are off. Callers may override any of it.
pub fn isolated_mcp_command(scratch: &Path) -> std::process::Command {
    let instances = scratch.join("instances");
    let registry = scratch.join("registry-home");
    let home = scratch.join("home");
    for dir in [&instances, &registry, &home] {
        std::fs::create_dir_all(dir).expect("scratch dir for an isolated infigraph-mcp");
    }
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_infigraph-mcp"));
    cmd.envs(mcp_state_env(scratch))
        .env("INFIGRAPH_REGISTRY_INSTANCES_DIR", &instances)
        .env("INFIGRAPH_REGISTRY_HOME", &registry)
        .env("HOME", &home)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_WATCH_DAEMON");
    cmd
}
