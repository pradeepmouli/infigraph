//! Crash / corrupt-index recovery helpers (code graph + document store).

use std::path::Path;
use std::time::Duration;

/// The supervisor's record of recent worker crashes (#20, R5.5).
///
/// A crash is answered by restarting the worker and nothing else. It used to
/// wipe and reindex the startup directory, every registered repo and every
/// group (I-14) -- but since #159 the worker opens no code graph, so its crash
/// says nothing about any of them, and the wipe only ever destroyed healthy
/// graphs. Graph damage is the owning daemon's to detect and repair (R3.1.4).
///
/// Without the wipe's delay between restarts, a worker that crashes on
/// startup would respawn in a tight loop forever. So a crash that is the
/// `WORKER_CRASH_LOOP_LIMIT`-th within `WORKER_CRASH_LOOP_WINDOW` stops the
/// supervisor instead, and the MCP client sees the server exit.
#[derive(Debug, Default)]
pub struct WorkerCrashes {
    recent: std::collections::VecDeque<std::time::Instant>,
}

pub const WORKER_CRASH_LOOP_LIMIT: usize = 3;
pub const WORKER_CRASH_LOOP_WINDOW: Duration = Duration::from_secs(60);

impl WorkerCrashes {
    /// Record a crash at `now`. `true`: restart the worker. `false`: this is
    /// a crash loop, give up.
    pub fn record(&mut self, now: std::time::Instant) -> bool {
        self.recent
            .retain(|at| now.saturating_duration_since(*at) < WORKER_CRASH_LOOP_WINDOW);
        self.recent.push_back(now);
        self.recent.len() < WORKER_CRASH_LOOP_LIMIT
    }
}

/// Proactively starts watching the directory this MCP server was launched
/// in, rather than only ever starting a watcher reactively after some write
/// happens to touch it. Only takes effect when both daemon mode
/// (`INFIGRAPH_BACKEND=daemon`) and the `[watch].auto_start_on_boot`
/// config toggle (env override: `INFIGRAPH_WATCH_AUTO_START_ON_BOOT`) are on. Runs
/// a true-up reindex first (see the "True-up" comment inline below) so the
/// watcher starts from a caught-up baseline instead of only ever reacting
/// to changes from this point forward.
///
/// Scoped to just `startup_dir` -- an earlier version swept the whole
/// project registry plus the groups dir, but that meant every MCP server startup spun up a daemon for
/// every registered project regardless of whether this server instance was
/// actually serving it: stale temp-test directories, a groups config dir
/// that isn't really a "project", and other repos not even open in this
/// session. Only the repo this server was launched in is this server's
/// concern.
///
/// Deliberately synchronous: `main.rs::run()` wraps this
/// call in its own thread so this doesn't delay MCP server readiness, but
/// keeping the logic itself synchronous means tests can observe its effect
/// deterministically. Lives in the library crate rather than as a
/// `main.rs`-only function, specifically so integration tests in `tests/`
/// can reach it -- `main.rs` compiles to a separate `[[bin]]` target with no
/// unit-test history of its own.
pub fn start_daemon_watcher_for_startup_dir(startup_dir: Option<&Path>) {
    if !infigraph_core::daemon_backend_selected() {
        return;
    }

    let Some(dir) = startup_dir else {
        return;
    };
    if !infigraph_core::watch::config::watch_enabled_at(dir, "watch") {
        return;
    }
    // Must be a directory that's actually been indexed -- mirrors
    // `collect_reindex_targets`'s own guard, for the same reason: a fresh,
    // never-indexed cwd has no `.infigraph/watch.lock` home for a daemon to
    // coordinate through, and there's nothing to watch yet regardless.
    if !dir.join(".infigraph").is_dir() {
        return;
    }

    // Unconditional, independent of `auto_start_on_boot` below: if a daemon
    // is already watching this root on a stale build, replace it with a
    // fresh one. This is correctness upkeep on something already running,
    // not new background activity, so it shouldn't share the gate that
    // controls spawning a daemon from nothing -- see `prune_stale_daemon`'s
    // doc comment. A pruned daemon leaves the root unwatched only until the
    // `auto_start_watch` call below (if enabled) or the next opportunistic
    // trigger (e.g. a `search` call) spawns a fresh one.
    // Judge staleness against the CLI binary a fresh daemon would be spawned
    // from, never against this MCP process's own build (#135): an MCP started
    // before an install would otherwise SIGTERM every daemon on the new
    // build. `None` (binary not resolvable) never prunes on hash grounds.
    let installed = std::env::current_exe()
        .ok()
        .and_then(|exe| infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(&exe).ok())
        .and_then(|cli| infigraph_core::daemon::installed_build_hash_of(&cli));
    infigraph_core::daemon::lifecycle::prune_stale_daemon(
        &dir.join(".infigraph/watch.lock"),
        installed.as_deref(),
    );

    if !crate::session_context::auto_start_watch_on_boot_enabled(dir) {
        return;
    }

    let path_str = dir.to_string_lossy().to_string();

    // True-up: catch drift accumulated while nothing was watching this
    // project (MCP was down, or a prior watcher exceeded its restart
    // budget and gave up) before starting to watch -- the watcher itself
    // only reacts to *future* filesystem events, so without this a file
    // added or changed during that gap stays invisible until something
    // else happens to touch it again. Reuses the same tool a client would
    // call to reindex by hand: incremental (mtime/hash based) under the
    // hood, so this is near-free when nothing drifted and does real work
    // only when it did. Errors are logged, not propagated -- a failed
    // true-up must not block starting the watcher below.
    match crate::tools::index::tool_index_project(&serde_json::json!({ "path": path_str })) {
        Ok(msg) => crate::mcp_log("INFO", &format!("Startup true-up index: {path_str}: {msg}")),
        Err(e) => crate::mcp_log(
            "WARN",
            &format!("Startup true-up index failed for {path_str}: {e}"),
        ),
    }

    if let Some(msg) = crate::tools::watch::auto_start_watch(&path_str) {
        crate::mcp_log("INFO", &format!("Startup watch: {path_str}: {msg}"));
    }
    crate::tools::docs::auto_start_doc_watch(&path_str);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn crashes_below_the_limit_restart_the_worker() {
        let mut crashes = WorkerCrashes::default();
        let t = Instant::now();
        for i in 0..WORKER_CRASH_LOOP_LIMIT - 1 {
            assert!(crashes.record(t + Duration::from_secs(i as u64)));
        }
    }

    #[test]
    fn the_limit_th_crash_inside_the_window_is_a_crash_loop() {
        let mut crashes = WorkerCrashes::default();
        let t = Instant::now();
        for _ in 0..WORKER_CRASH_LOOP_LIMIT - 1 {
            assert!(crashes.record(t));
        }
        assert!(!crashes.record(t + WORKER_CRASH_LOOP_WINDOW / 2));
    }

    #[test]
    fn crashes_older_than_the_window_no_longer_count() {
        let mut crashes = WorkerCrashes::default();
        let t = Instant::now();
        for _ in 0..WORKER_CRASH_LOOP_LIMIT - 1 {
            assert!(crashes.record(t));
        }
        assert!(crashes.record(t + WORKER_CRASH_LOOP_WINDOW));
    }
}
