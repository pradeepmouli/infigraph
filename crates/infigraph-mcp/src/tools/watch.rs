use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::Value;

use infigraph_core::daemon_protocol::{WatchAction, WatchRole};
use infigraph_core::watch::WatchEventKind;
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

static WATCHERS_DISABLED: AtomicBool = AtomicBool::new(false);

pub fn disable_watchers() {
    WATCHERS_DISABLED.store(true, Ordering::Relaxed);
}

pub fn watchers_disabled() -> bool {
    WATCHERS_DISABLED.load(Ordering::Relaxed)
}

/// Monotonic tie-breaker for `generate_watcher_id`. A millisecond timestamp
/// alone collides whenever two watchers start in the same millisecond --
/// routine when e.g. `tool_index_project` fires back-to-back for multiple
/// repos in a tight loop -- and a collision silently overwrites the earlier
/// watcher's map entry, dropping its `stop_tx` and leaking that watcher
/// (never reachable by ID again, so `stop_all_watchers`-style cleanup can
/// never signal it).
static WATCHER_ID_SEQ: AtomicU64 = AtomicU64::new(0);

/// Build a collision-proof watcher ID: `{prefix}-{millis}-{seq}`. Shared by
/// code watchers (`tool_watch_project`) and doc watchers (`tool_watch_docs`)
/// so both get the same uniqueness guarantee from one place.
pub(crate) fn generate_watcher_id(prefix: &str) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let seq = WATCHER_ID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{millis}-{seq}")
}

fn watch_log(level: &str, msg: &str) {
    use std::io::Write;
    let path = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .or_else(dirs_next::home_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".infigraph")
        .join("mcp.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {level}: {msg}");
    }
}

pub struct WatcherEntry {
    pub stop_tx: mpsc::Sender<()>,
    pub path: String,
    pub pending_reindex: Arc<Mutex<Vec<String>>>,
}

pub static WATCHERS: Mutex<Option<HashMap<String, WatcherEntry>>> = Mutex::new(None);

pub fn get_watchers() -> std::sync::MutexGuard<'static, Option<HashMap<String, WatcherEntry>>> {
    WATCHERS.lock().unwrap()
}

pub fn init_watchers() {
    let mut guard = WATCHERS.lock().unwrap();
    if guard.is_none() {
        *guard = Some(HashMap::new());
    }
}

pub fn is_watching(path: &str) -> bool {
    let guard = WATCHERS.lock().unwrap();
    guard
        .as_ref()
        .is_some_and(|map| map.values().any(|e| e.path == path))
}

/// True when a code watcher is running for `root` — either in this process
/// (WATCHERS map) or in another process (CLI `infigraph watch` or another
/// MCP worker holding `.infigraph/watch.lock`). Probe-only: opens the lock
/// file and immediately releases the trial flock; never creates the file.
pub fn watcher_running(root: &std::path::Path) -> bool {
    let root_str = root.to_string_lossy().replace('\\', "/");
    if is_watching(&root_str) {
        return true;
    }
    use fs2::FileExt;
    let lock_path = root.join(".infigraph").join("watch.lock");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&lock_path)
        .ok()
        .map(|f| {
            let locked = f.try_lock_exclusive().is_err();
            let _ = fs2::FileExt::unlock(&f);
            locked
        })
        .unwrap_or(false)
}

pub fn auto_start_watch(path: &str) -> Option<String> {
    auto_start_watch_inner(path, false)
}

pub fn auto_start_watch_opportunistic(path: &str) -> Option<String> {
    auto_start_watch_inner(path, true)
}

fn auto_start_watch_inner(path: &str, skip_disabled_check: bool) -> Option<String> {
    if is_remote_mode() {
        return None;
    }
    if !skip_disabled_check && watchers_disabled() {
        return None;
    }
    let root = std::path::PathBuf::from(path).canonicalize().ok()?;
    let root_str = root.to_string_lossy().replace('\\', "/");

    if is_watching(&root_str) {
        return None;
    }

    if infigraph_core::daemon::lifecycle::watch_daemon_mode_enabled() {
        return match ensure_daemon_watcher(&root, "watch") {
            Ok(infigraph_core::daemon::lifecycle::DaemonStartOutcome::Spawned) => {
                eprintln!("[auto-watch] Started daemon watcher for {root_str}");
                Some(format!("Daemon watcher started for {root_str}"))
            }
            Ok(infigraph_core::daemon::lifecycle::DaemonStartOutcome::AlreadyRunning) => None,
            Ok(infigraph_core::daemon::lifecycle::DaemonStartOutcome::Failed(e)) => {
                eprintln!("[auto-watch] Failed to start daemon watcher: {e}");
                None
            }
            Err(e) => {
                eprintln!("[auto-watch] could not locate infigraph CLI binary: {e}");
                None
            }
        };
    }

    let args = serde_json::json!({
        "path": path,
        "auto_resolve": true,
        "debounce_ms": 500
    });
    match tool_watch_project(&args) {
        Ok(msg) => {
            eprintln!("[auto-watch] Started watcher for {root_str}");
            Some(msg)
        }
        Err(e) => {
            eprintln!("[auto-watch] Failed to start watcher: {e}");
            None
        }
    }
}

/// Resolve the CLI binary next to this MCP binary and ask the shared daemon
/// primitive to ensure a detached watcher is running for `root`. Shared by
/// both code-watch (`tool_watch_project`'s explicit daemon-mode branch,
/// `auto_start_watch_inner`) and doc-watch (`tool_watch_docs`,
/// `auto_start_doc_watch_inner`) callers so CLI-binary resolution isn't
/// duplicated between them — each caller still formats its own message/log
/// from the returned outcome, since a silent auto-start and an explicit tool
/// call want different things reported.
///
/// `section` ("watch" or "watch_docs") is caller-supplied rather than
/// inferred, since this one function backs both policies: hardcoding either
/// section here would make the persisted disable policy for one section
/// incorrectly suppress daemon spawns for the other (they share this same
/// underlying daemon-process primitive, but the policy is per-section).
/// Checked first, before any binary resolution or lock probing, so a
/// disabled policy is a true no-op rather than doing work it then discards.
pub(crate) fn ensure_daemon_watcher(
    root: &std::path::Path,
    section: &str,
) -> Result<infigraph_core::daemon::lifecycle::DaemonStartOutcome> {
    if !infigraph_core::watch::config::watch_enabled_at(root, section) {
        return Ok(infigraph_core::daemon::lifecycle::DaemonStartOutcome::AlreadyRunning);
    }
    let mcp_exe = std::env::current_exe().context("could not resolve current executable")?;
    let cli_binary = infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(&mcp_exe)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(infigraph_core::daemon::lifecycle::ensure_daemon_running(
        root,
        &cli_binary,
    ))
}

/// Acquire the per-project watch lock (`.infigraph/watch.lock`), the same
/// lock the CLI's `infigraph watch` holds for its lifetime. Retries briefly:
/// a watcher that was just told to stop may take up to its poll interval
/// (~200ms) to actually exit and release this lock, and without a retry
/// window a start attempt landing in that gap would spuriously fail.
fn acquire_project_watch_lock(
    lock_path: &std::path::Path,
) -> Result<infigraph_core::lockfile::LockFile> {
    const ATTEMPTS: u32 = 10;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
    let mut last_err = None;
    for _ in 0..ATTEMPTS {
        match infigraph_core::lockfile::try_acquire(lock_path, "mcp-watch") {
            Ok(Some(guard)) => return Ok(guard),
            Ok(None) => {
                last_err = Some(anyhow::anyhow!("lock held"));
                std::thread::sleep(RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap())
}

fn is_remote_mode() -> bool {
    std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false)
}

pub fn tool_watch_project(args: &Value) -> Result<String> {
    if is_remote_mode() {
        return Ok(
            "File watching is not supported in remote mode (Neo4j backend). \
                    Reindexing is triggered via webhooks instead."
                .to_string(),
        );
    }

    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let debounce_ms = args
        .get("debounce_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(500);
    let auto_resolve = args
        .get("auto_resolve")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let root = std::path::PathBuf::from(path)
        .canonicalize()
        .context("invalid path")?;
    let root_str = root.to_string_lossy().replace('\\', "/");

    // Explicit calls must respect the same coordination auto-start already
    // does — without these checks, a direct watch_project call bypasses the
    // persisted enable/disable policy, the daemon-mode toggle, and
    // primary/secondary gating, letting a competing in-process watcher race
    // a properly daemon-spawned one for the same repo's index.lock (or start
    // one the user explicitly asked infigraph to keep off).
    if !infigraph_core::watch::config::watch_enabled_at(&root, "watch") {
        return Ok(format!(
            "Not starting a watcher for {root_str}: code-watching is disabled \
             (infigraph watch enable to turn it back on)."
        ));
    }

    if watchers_disabled() {
        return Ok(format!(
            "Not starting a watcher for {root_str}: this MCP instance is not primary \
             (another instance holds mcp.lock and owns watchers for this machine). \
             Use get_watch_status to check the active watcher."
        ));
    }

    if infigraph_core::daemon::lifecycle::watch_daemon_mode_enabled() {
        return match ensure_daemon_watcher(&root, "watch")? {
            infigraph_core::daemon::lifecycle::DaemonStartOutcome::Spawned => {
                Ok(format!("Daemon watcher started for {root_str}"))
            }
            infigraph_core::daemon::lifecycle::DaemonStartOutcome::AlreadyRunning => {
                Ok(format!("Daemon watcher already running for {root_str}"))
            }
            infigraph_core::daemon::lifecycle::DaemonStartOutcome::Failed(e) => {
                Err(anyhow::anyhow!("Failed to start daemon watcher: {e}"))
            }
        };
    }

    init_watchers();

    let lock_path = root.join(".infigraph").join("watch.lock");
    let watch_lock = acquire_project_watch_lock(&lock_path).map_err(|_| {
        anyhow::anyhow!(
            "another watcher is already running for {root_str} \
             (CLI `infigraph watch` or another MCP worker) — \
             use get_watch_status to check, or stop it first"
        )
    })?;

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let watcher_id = generate_watcher_id("watch");

    let pending_reindex: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let pending_clone = Arc::clone(&pending_reindex);

    {
        let mut guard = get_watchers();
        if let Some(map) = guard.as_mut() {
            map.insert(
                watcher_id.clone(),
                WatcherEntry {
                    stop_tx,
                    path: root_str.clone(),
                    pending_reindex,
                },
            );
        }
    }

    let watcher_id_clone = watcher_id.clone();
    std::thread::spawn(move || {
        // Held for the watcher's lifetime; released when this thread exits.
        let _watch_lock = watch_lock;
        let id_short = watcher_id_clone[..12.min(watcher_id_clone.len())].to_string();
        if auto_resolve {
            if let Err(e) = infigraph_core::watch::watch_project_auto_resolve(
                &root,
                bundled_registry,
                debounce_ms,
                stop_rx,
                &id_short,
            ) {
                let msg = format!("watcher {id_short} auto_resolve error: {e}");
                eprintln!("[watch] {msg}");
                watch_log("ERROR", &msg);
            }
        } else {
            let on_event = {
                let id_short = id_short.clone();
                move |evt: infigraph_core::watch::WatchEvent| match evt.kind {
                    WatchEventKind::WatcherRestarted => {
                        let msg = format!("watcher {id_short} restarted after internal failure");
                        eprintln!("[watch {id_short}] {msg}");
                        watch_log("WARN", &msg);
                    }
                    WatchEventKind::WatcherDied => {
                        let msg = format!(
                            "watcher {id_short} died permanently for {}",
                            evt.path.display()
                        );
                        eprintln!("[watch {id_short}] {msg}");
                        watch_log("ERROR", &msg);
                    }
                    _ if evt.has_cross_file_calls => {
                        let file = evt.path.to_string_lossy().replace('\\', "/");
                        eprintln!("[watch {id_short}] {evt}");
                        let mut pending = pending_clone.lock().unwrap();
                        if !pending.contains(&file) {
                            pending.push(file);
                        }
                        eprintln!("[watch {id_short}] ⚠ cross-file calls affected — call index_project to re-resolve (or use auto_resolve=true)");
                    }
                    _ => {
                        eprintln!("[watch {id_short}] {evt}");
                    }
                }
            };
            if let Err(e) = infigraph_core::watch::watch_project(
                &root,
                bundled_registry,
                debounce_ms,
                stop_rx,
                on_event,
            ) {
                let msg = format!("watcher {id_short} error: {e}");
                eprintln!("[watch] {msg}");
                watch_log("ERROR", &msg);
            }
        }
        let mut guard = WATCHERS.lock().unwrap();
        if let Some(map) = guard.as_mut() {
            map.remove(&watcher_id_clone);
        }
    });

    let auto_note = if auto_resolve {
        "\nauto_resolve: ON — full reindex runs automatically when cross-file calls are affected"
    } else {
        "\nauto_resolve: OFF — call index_project when notified of cross-file call changes, or use get_watch_status to check"
    };

    Ok(format!(
        "Watcher started.\nID: {watcher_id}\nPath: {root_str}\nDebounce: {debounce_ms}ms{auto_note}\nUse stop_watch to stop."
    ))
}

pub fn tool_stop_watch(args: &Value) -> Result<String> {
    if let Some(watcher_id) = args.get("watcher_id").and_then(|v| v.as_str()) {
        let mut guard = get_watchers();
        if let Some(map) = guard.as_mut() {
            if let Some(entry) = map.remove(watcher_id) {
                let _ = entry.stop_tx.send(());
                return Ok(format!("Watcher {watcher_id} stopped."));
            }
        }
        return Ok(format!("No watcher found with ID: {watcher_id}"));
    }

    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        let root = std::path::PathBuf::from(path)
            .canonicalize()
            .context("invalid path")?;
        let sentinel = root.join(".infigraph").join("watch.stop");
        let lock_path = root.join(".infigraph").join("watch.lock");
        if !lock_path.exists() {
            return Ok("No watcher running.".to_string());
        }
        let alive = infigraph_core::lockfile::try_acquire(&lock_path, "watch-liveness-probe")
            .ok()
            .flatten()
            .is_none();
        if !alive {
            return Ok("No watcher running.".to_string());
        }
        std::fs::write(&sentinel, b"")?;
        return Ok("Stop signal sent. Watcher will exit within ~1 second.".to_string());
    }

    anyhow::bail!("missing 'watcher_id' or 'path'")
}

/// How long to wait for a running daemon to reply to a `WatchControl`
/// request before giving up. Mirrors `infigraph-cli::info_commands::
/// WATCH_CONTROL_TIMEOUT` (30s) -- that constant is private to the CLI
/// crate, so this is a deliberate duplicate of the value, not the item.
const WATCH_CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub fn enable_watch(args: &Value) -> Result<String> {
    watch_control(args, WatchRole::Code, WatchAction::Enable)
}

pub fn disable_watch(args: &Value) -> Result<String> {
    watch_control(args, WatchRole::Code, WatchAction::Disable)
}

pub fn restart_watch(args: &Value) -> Result<String> {
    watch_control(args, WatchRole::Code, WatchAction::Restart)
}

/// Shared, role-parameterized implementation behind enable_watch/
/// disable_watch/restart_watch and their _docs counterparts (Task 16).
/// Mirrors `tool_watch_project`'s three cases: not-primary decline,
/// daemon-mode delegation (through the real `WatchControl` request bridge,
/// not the `tool_stop_watch` sentinel -- that sentinel brings down the
/// whole daemon process, not just this role's watch activity), and
/// in-process (WATCHERS map / local producer state). Also mirrors the CLI's
/// `cmd_watch_control` (`crates/infigraph-cli/src/info_commands.rs`): policy
/// writes happen unconditionally so they survive a restart even with no
/// daemon currently up, and a `daemon_is_alive` liveness probe runs before
/// submitting so the everyday "no daemon running" case returns promptly
/// instead of blocking for the full `WATCH_CONTROL_TIMEOUT`.
pub(crate) fn watch_control(args: &Value, role: WatchRole, action: WatchAction) -> Result<String> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path'")?;
    let root = std::path::PathBuf::from(path)
        .canonicalize()
        .context("invalid path")?;
    let root_str = root.to_string_lossy().replace('\\', "/");

    if watchers_disabled() {
        return Ok(format!(
            "Not controlling watch for {root_str}: this MCP instance is not primary \
             (another instance holds mcp.lock and owns watchers for this machine)."
        ));
    }

    if matches!(action, WatchAction::Enable | WatchAction::Disable) {
        infigraph_core::watch::config::write_watch_policy(
            &root,
            role,
            action == WatchAction::Enable,
        )?;
    }

    if infigraph_core::daemon::lifecycle::watch_daemon_mode_enabled() {
        let lock_path = root.join(".infigraph").join("watch.lock");
        if !infigraph_core::daemon::lifecycle::daemon_is_alive(&lock_path) {
            return Ok(
                if matches!(action, WatchAction::Enable | WatchAction::Disable) {
                    format!(
                    "{role:?}: policy persisted for {root_str}; no daemon currently running to notify."
                )
                } else {
                    format!("No daemon running for {root_str}.")
                },
            );
        }
        let registry = bundled_registry()?;
        let prism = Infigraph::open(&root, registry)?;
        prism.submit_watch_control_and_await(role, action, WATCH_CONTROL_TIMEOUT)?;
        return Ok(format!("{role:?}: {action:?} sent for {root_str}."));
    }

    match (role, action) {
        (WatchRole::Code, WatchAction::Disable) => {
            tool_stop_watch(&serde_json::json!({ "path": path }))?;
            Ok(format!("Code watching disabled for {root_str}"))
        }
        (WatchRole::Code, WatchAction::Enable) => {
            if !is_watching(&root_str) {
                tool_watch_project(&serde_json::json!({ "path": path }))?;
            }
            Ok(format!("Code watching enabled for {root_str}"))
        }
        (WatchRole::Code, WatchAction::Restart) => {
            let _ = tool_stop_watch(&serde_json::json!({ "path": path }));
            tool_watch_project(&serde_json::json!({ "path": path }))?;
            Ok(format!("Code watching restarted for {root_str}"))
        }
        (WatchRole::Docs, WatchAction::Disable) => {
            super::docs::tool_stop_watch_docs(&serde_json::json!({ "path": path }))?;
            Ok(format!("Doc watching disabled for {root_str}"))
        }
        (WatchRole::Docs, WatchAction::Enable) => {
            if !super::docs::is_doc_watching(&root_str) {
                super::docs::tool_watch_docs(&serde_json::json!({ "path": path }))?;
            }
            Ok(format!("Doc watching enabled for {root_str}"))
        }
        (WatchRole::Docs, WatchAction::Restart) => {
            let _ = super::docs::tool_stop_watch_docs(&serde_json::json!({ "path": path }));
            super::docs::tool_watch_docs(&serde_json::json!({ "path": path }))?;
            Ok(format!("Doc watching restarted for {root_str}"))
        }
        _ => anyhow::bail!(
            "unsupported role/action combination for the in-process case: {role:?}/{action:?}"
        ),
    }
}

pub fn tool_get_watch_status(args: &Value) -> Result<String> {
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        let root = std::path::PathBuf::from(path)
            .canonicalize()
            .context("invalid path")?;
        let lock_path = root.join(".infigraph").join("watch.lock");
        if !lock_path.exists() {
            return Ok(format!("No watcher running for {path}."));
        }
        let alive = infigraph_core::lockfile::try_acquire(&lock_path, "watch-liveness-probe")
            .ok()
            .flatten()
            .is_none();
        if !alive {
            return Ok(format!("No watcher running for {path}."));
        }
        return Ok(match infigraph_core::lockfile::read_holder(&lock_path) {
            Some(info) => format!(
                "Watcher active for {path}\nHeld by PID {} (role: {}) since epoch {}\n\
                 Note: pending-reindex tracking is not available across processes in \
                 daemon mode — use index_project if unsure whether a reindex is needed.",
                info.pid, info.role, info.acquired_at
            ),
            None => format!(
                "Watcher active for {path} (holder identity unavailable)\n\
                 Note: pending-reindex tracking is not available across processes in \
                 daemon mode — use index_project if unsure whether a reindex is needed."
            ),
        });
    }

    let watcher_id = args.get("watcher_id").and_then(|v| v.as_str());

    if let Some(id) = watcher_id {
        // Check code watchers
        {
            let guard = get_watchers();
            if let Some(map) = guard.as_ref() {
                if let Some(entry) = map.get(id) {
                    let pending = entry.pending_reindex.lock().unwrap();
                    let mut out = format!("Watcher: {id}\nType: code\nPath: {}\n", entry.path);
                    if pending.is_empty() {
                        out.push_str("Status: OK — no pending reindex needed\n");
                    } else {
                        out.push_str(&format!(
                            "⚠ {} file(s) changed with cross-file calls — run index_project to re-resolve:\n",
                            pending.len()
                        ));
                        for f in pending.iter() {
                            out.push_str(&format!("  - {f}\n"));
                        }
                    }
                    return Ok(out);
                }
            }
        }
        // Check doc watchers
        {
            let guard = super::docs::get_doc_watchers();
            if let Some(map) = guard.as_ref() {
                if let Some(entry) = map.get(id) {
                    return Ok(format!(
                        "Watcher: {id}\nType: docs\nPath: {}\nStatus: active\n",
                        entry.path
                    ));
                }
            }
        }
        return Ok(format!("No watcher found with ID: {id}"));
    }

    // List all watchers from both registries
    let mut total = 0usize;
    let mut out = String::new();

    {
        let guard = get_watchers();
        if let Some(map) = guard.as_ref() {
            for (id, entry) in map.iter() {
                total += 1;
                let pending_count = entry.pending_reindex.lock().unwrap().len();
                let warn = if pending_count > 0 {
                    format!(" ⚠ {pending_count} pending reindex")
                } else {
                    String::new()
                };
                out.push_str(&format!("  {id} — [code] {}{warn}\n", entry.path));
            }
        }
    }

    {
        let guard = super::docs::get_doc_watchers();
        if let Some(map) = guard.as_ref() {
            for (id, entry) in map.iter() {
                total += 1;
                out.push_str(&format!("  {id} — [docs] {}\n", entry.path));
            }
        }
    }

    if total == 0 {
        return Ok("No watchers running.".to_string());
    }

    Ok(format!("{total} watcher(s) running:\n{out}"))
}

#[cfg(test)]
mod watcher_id_tests {
    use super::*;

    #[test]
    fn generate_watcher_id_is_unique_within_the_same_millisecond() {
        // Regression test: two watchers starting back-to-back (e.g.
        // tool_index_project firing for two repos in a tight loop) used to
        // get the identical ID when both calls landed in the same
        // millisecond, silently overwriting one watcher's map entry and
        // leaking it (unreachable for stop/cleanup by ID ever again).
        let ids: Vec<String> = (0..500).map(|_| generate_watcher_id("watch")).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "generate_watcher_id produced a duplicate within a tight loop"
        );
    }

    #[test]
    fn generate_watcher_id_uses_the_given_prefix() {
        assert!(generate_watcher_id("docwatch").starts_with("docwatch-"));
        assert!(generate_watcher_id("watch").starts_with("watch-"));
    }
}
