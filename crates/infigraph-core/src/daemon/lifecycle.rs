//! Cross-process "ensure a watcher daemon is running for this repo"
//! primitive, shared by the CLI's `infigraph watch` auto-start and MCP's
//! opportunistic/bootstrap watch-start paths (toggle-gated — see
//! `watch_daemon_mode_enabled`). Generalizes what used to be CLI-only
//! (`spawn_watcher`/`ensure_watcher_running` in `infigraph-cli`), which
//! assumed the calling process *was* the binary to re-exec
//! (`std::env::current_exe()`) — that assumption breaks when the caller is
//! `infigraph-mcp`, which has no `watch` subcommand of its own. Callers now
//! pass the target binary path explicitly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Result;

const CI_ENV_VARS: &[&str] = &[
    "CI",
    "GITHUB_ACTIONS",
    "JENKINS_URL",
    "BUILDKITE",
    "GITLAB_CI",
    "INFIGRAPH_NO_WATCH",
];

/// True under CI or when watching has been explicitly opted out of via
/// `INFIGRAPH_NO_WATCH`. Single source of truth — previously duplicated
/// verbatim as `infigraph-cli::index::is_ci()`.
pub fn is_ci_env() -> bool {
    CI_ENV_VARS.iter().any(|v| std::env::var_os(v).is_some())
}

/// Whether the active backend is remote (Neo4j/Postgres) rather than the
/// default local Kùzu. Single source of truth — previously duplicated as
/// `is_neo4j_backend()` (infigraph-cli) and `is_remote_mode()`
/// (infigraph-mcp), both checking the same `INFIGRAPH_BACKEND` env var.
/// File watching is meaningless under remote mode: reindexing there is
/// driven by webhooks, not local file-change events.
pub fn is_remote_backend() -> bool {
    crate::selected_backend() == "neo4j"
}

/// Whether daemon-mode watching is active. Aliases `daemon_backend_selected`
/// (`INFIGRAPH_BACKEND=daemon`) rather than reading its own env var --
/// `INFIGRAPH_WATCH_DAEMON` used to be an independent toggle, but setting it
/// without also selecting the daemon backend left a real hazard: the
/// watcher would become a real daemon process holding its own `Database`
/// handle on the live graph, while the calling process -- still on the
/// local Kuzu backend -- opened a second, independent `Database` object on
/// the same path for its own writes. One env var now controls both.
pub fn watch_daemon_mode_enabled() -> bool {
    crate::daemon_backend_selected()
}

/// Outcome of an `ensure_daemon_running` call.
#[derive(Debug, PartialEq, Eq)]
pub enum DaemonStartOutcome {
    /// A daemon is already alive for this repo (lock held), watching is
    /// inapplicable (CI / remote backend), or the project simply hasn't been
    /// indexed yet (no `.infigraph` at `root`) — no-op either way. The
    /// "not indexed yet" case is a benign precondition-not-met state (e.g.
    /// the very first `infigraph index` on a fresh project, before
    /// `.infigraph` exists), not an actionable failure, so it's folded into
    /// this silent variant rather than `Failed`.
    AlreadyRunning,
    /// This call won the lock race and spawned a new daemon process.
    Spawned,
    /// Spawn was attempted but failed (e.g. binary not found, OS-level spawn
    /// error, lock-probe I/O error).
    Failed(String),
}

/// Ensure a detached `infigraph watch` daemon is running for `root`,
/// re-exec'ing `watch_binary` (the CLI binary path — the CLI passes its own
/// `current_exe()`; MCP passes a resolved sibling binary path, since
/// `infigraph-mcp` has no `watch` subcommand). Coordinates through the same
/// `.infigraph/watch.lock` every watcher entry point already uses, so this
/// is safe to call redundantly from multiple processes — at most one spawn
/// wins. Note: there is a narrow, pre-existing race between the trial lock
/// probe below and the daemon's own lock acquisition at startup — this
/// mirrors the original CLI implementation's behavior exactly and is not
/// newly introduced here; closing it is out of scope for this plan.
///
/// Respects the CI/`INFIGRAPH_NO_WATCH` opt-out -- appropriate here, since
/// every caller of this function wants a daemon purely for the file-watching
/// *convenience*, which is fine to skip in CI/sandboxed environments. A
/// caller for whom a daemon is a hard correctness requirement instead (i.e.
/// `INFIGRAPH_BACKEND=daemon` was explicitly selected, so writes have
/// nowhere to go without one) must use [`ensure_daemon_running_required`],
/// which does not honor that opt-out.
pub fn ensure_daemon_running(root: &Path, watch_binary: &Path) -> DaemonStartOutcome {
    if is_ci_env() {
        return DaemonStartOutcome::AlreadyRunning;
    }
    ensure_daemon_running_required(root, watch_binary)
}

/// Like [`ensure_daemon_running`], but does not honor the CI/
/// `INFIGRAPH_NO_WATCH` convenience opt-out -- for callers where a daemon is
/// a hard requirement, not a nicety. `INFIGRAPH_BACKEND=daemon` is an
/// explicit request that every write route through a daemon; suppressing
/// the one thing that makes that possible because `INFIGRAPH_NO_WATCH`
/// happened to also be set (e.g. leaked from a shared shell profile) used to
/// mean every write silently blocked for its own multi-minute timeout
/// instead of either working or failing fast. Still respects
/// [`is_remote_backend`]: a daemon is meaningless under the Neo4j backend
/// regardless of which opt-out is in play.
pub fn ensure_daemon_running_required(root: &Path, watch_binary: &Path) -> DaemonStartOutcome {
    if is_remote_backend() {
        return DaemonStartOutcome::AlreadyRunning;
    }

    let tg_dir = root.join(".infigraph");
    if !tg_dir.exists() {
        // Not yet indexed (e.g. the very first `infigraph index` on a fresh
        // project, called before `.infigraph` is created). This is an
        // expected precondition-not-met state, not a failure — treat it the
        // same as "nothing to do" so callers don't surface a spurious
        // "Failed to start watcher" message on ordinary first-time use.
        return DaemonStartOutcome::AlreadyRunning;
    }

    let lock_path = tg_dir.join("watch.lock");
    match crate::lockfile::try_acquire(&lock_path, "watch-daemon-probe") {
        Ok(Some(guard)) => {
            // We won the trial lock — nobody else is watching. Release it
            // immediately (the daemon process re-acquires its own
            // long-lived hold on startup) and spawn.
            drop(guard);
            spawn_daemon(root, &tg_dir, watch_binary)
        }
        Ok(None) => {
            // Judge staleness against the binary we would spawn, not this
            // process's own build -- see `holder_is_stale_build`.
            let installed = crate::daemon::installed_build_hash_of(watch_binary);
            if !prune_stale_daemon(&lock_path, installed.as_deref()) {
                return DaemonStartOutcome::AlreadyRunning;
            }
            // The stale holder was pruned (or was already dead) and the
            // lock should now be free — retry once.
            match crate::lockfile::try_acquire(&lock_path, "watch-daemon-probe") {
                Ok(Some(guard)) => {
                    drop(guard);
                    spawn_daemon(root, &tg_dir, watch_binary)
                }
                Ok(None) => DaemonStartOutcome::AlreadyRunning,
                Err(e) => DaemonStartOutcome::Failed(e.to_string()),
            }
        }
        Err(e) => DaemonStartOutcome::Failed(e.to_string()),
    }
}

/// Whether a daemon currently holds `lock_path` (i.e. is genuinely running,
/// not just left a stale payload behind). The single canonical liveness
/// check for `watch.lock` -- every caller across the CLI, MCP, and core that
/// needs to know "is a daemon here" should use this rather than duplicating
/// the try-acquire-and-release probe.
pub fn daemon_is_alive(lock_path: &Path) -> bool {
    if !lock_path.exists() {
        return false;
    }
    crate::lockfile::try_acquire(lock_path, "daemon-liveness-probe")
        .ok()
        .flatten()
        .is_none()
}

/// Poll [`daemon_is_alive`] until it turns true or `budget` elapses. Exists
/// because daemon startup is asynchronous: `ensure_daemon_running`/
/// `ensure_daemon_running_required` return at spawn time, and the child
/// needs a moment to actually acquire `watch.lock`.
pub fn wait_for_daemon_ready(lock_path: &Path, budget: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if daemon_is_alive(lock_path) {
            return true;
        }
        if start.elapsed() >= budget {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// One live process this sweep judged to be an orphaned daemon: an
/// `infigraph daemon` still running against a root that no longer exists.
#[derive(Debug, Clone)]
pub struct OrphanedDaemon {
    pub pid: u32,
    pub cwd: PathBuf,
}

/// Find every live `infigraph daemon` process whose watched root has been
/// deleted, without touching any of them.
///
/// This exists as a backstop for the watch loop's own self-check (see
/// `run_write_coordinator`, which now shuts itself down once its root
/// disappears): that check only runs while the loop is actually ticking, so
/// a daemon that's wedged (stuck on a lock, a hung query) never reaches it.
///
/// A daemon's `watch.lock` -- the durable record `prune_stale_daemon` and
/// `infigraph doctor`/`infigraph ps` rely on -- lives *inside* the very
/// directory tree the daemon watches. Once that directory is `rm -rf`'d (a
/// deleted project, a test's tempdir), the lock file goes with it, so none
/// of those lock-file-driven mechanisms can discover the orphan anymore --
/// there is nothing left on disk pointing at it. The only remaining signal
/// is the live process itself: its OS-reported working directory, set once
/// at spawn by `build_daemon_command`'s `current_dir(root)`, still names
/// the now-gone path.
///
/// Three independent signals must all agree before a process counts as an
/// orphan (binary name, `daemon` in its own argv, and a genuinely missing
/// cwd) -- the same "don't trust a bare identity match" discipline
/// `prune_stale_daemon` documents for its own PID-reuse guard.
pub fn find_orphaned_daemons() -> Vec<OrphanedDaemon> {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let mut orphans = Vec::new();
    for (pid, proc) in sys.processes() {
        let cmd: Vec<String> = proc
            .cmd()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if is_orphaned_daemon(&proc.name().to_string_lossy(), &cmd, proc.cwd()) {
            orphans.push(OrphanedDaemon {
                pid: pid.as_u32(),
                // Checked non-None by `is_orphaned_daemon` just above.
                cwd: proc
                    .cwd()
                    .expect("cwd checked by is_orphaned_daemon")
                    .to_path_buf(),
            });
        }
    }
    orphans
}

/// The three-signal decision `find_orphaned_daemons` applies to each live
/// process: binary name, `daemon` present in its own argv, and a `cwd` that
/// genuinely no longer exists on disk. Factored out as a pure function of
/// already-observed facts (rather than inline in the `sysinfo` loop above)
/// so it's directly unit-testable without spawning a real process.
fn is_orphaned_daemon(proc_name: &str, cmd: &[String], cwd: Option<&Path>) -> bool {
    let looks_like_infigraph = {
        let lower = proc_name.to_ascii_lowercase();
        lower == "infigraph" || lower == "infigraph.exe"
    };
    if !looks_like_infigraph {
        return false;
    }
    if !cmd.iter().any(|a| a == "daemon") {
        return false;
    }
    match cwd {
        Some(cwd) => !cwd.exists(),
        None => false,
    }
}

/// Terminate an orphaned daemon found by [`find_orphaned_daemons`]. Sends
/// SIGTERM -- the same graceful path Ctrl-C/`infigraph kill` already use,
/// which the watch loop already releases its lock cleanly on receipt of --
/// never SIGKILL: a daemon that's merely slow to notice its root is gone
/// still deserves the chance to shut down cleanly.
pub fn kill_orphaned_daemon(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .output();
    }
}

/// If `lock_path`'s current holder is dead, or alive but running a
/// different build than this process, terminate it and wait briefly for
/// it to actually exit. Returns `true` if the holder was found to be
/// stale (already dead, or signaled and confirmed to have exited), `false`
/// if the holder is live and current, no holder identity could be read at
/// all (nothing actionable to prune), or a signaled holder is still alive
/// once the wait budget runs out.
///
/// That last case matters: the lock file's payload disappearing is not
/// proof the signaled process is gone (`Drop for LockFile` clears the
/// payload and releases the flock before the process finishes unwinding
/// and actually exits), so a caller that treats "lock content gone" as
/// license to spawn a fresh daemon can end up racing a new watcher's
/// startup against the old one's last few hundred ms of shutdown --
/// or, if the signal was never delivered/handled at all, spawning a
/// second permanent watcher alongside a first that never leaves. Waiting
/// on the PID itself catches both.
///
/// PID-reuse guard: `LockInfo` (unlike the MCP instance registry) carries
/// no OS-reported process-start-time to re-verify against, so a bare PID
/// match is not on its own proof this is the process the lock named (same
/// hazard `instances::classify_instances` guards against). Before sending
/// any signal, this checks that the live process at that PID still looks
/// like an infigraph binary — if the PID was recycled for something else
/// entirely, it's left alone.
///
/// `pub`: besides being `ensure_daemon_running_required`'s own internal
/// step, this is also the right thing for a caller to run unconditionally
/// whenever a daemon already exists for a root -- independent of whatever
/// gate controls spawning a *new* daemon from nothing (e.g. MCP's
/// `auto_start_on_boot` toggle). Replacing an already-running stale-build
/// daemon with a fresh one is correctness upkeep, not new background
/// activity, so it shouldn't share that toggle.
/// Pure decision: is a live holder recording `holder_build_hash` stale
/// relative to `installed_build_hash` -- the hash of the binary a fresh
/// daemon would be spawned from (see `daemon::installed_build_hash_of`)?
/// Deliberately NOT relative to `crate::build_hash()`: that is the judging
/// process's own build, which is exactly wrong when the judge is the
/// out-of-date one (#135 -- an `infigraph-mcp` started before an install
/// was SIGTERMing every daemon on the *new* build and respawning it from
/// the on-disk binary it would judge stale again). `None` (couldn't
/// determine what's installed) never justifies signalling a live process.
pub fn holder_is_stale_build(holder_build_hash: &str, installed_build_hash: Option<&str>) -> bool {
    match installed_build_hash {
        Some(installed) => holder_build_hash != installed,
        None => false,
    }
}

pub fn prune_stale_daemon(lock_path: &Path, installed_build_hash: Option<&str>) -> bool {
    let Some(holder) = crate::lockfile::read_holder(lock_path) else {
        return false;
    };

    let spid = sysinfo::Pid::from_u32(holder.pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[spid]), true);

    let Some(proc) = sys.process(spid) else {
        // PID isn't running at all -- the lock is simply stale (the holder
        // crashed or was killed without releasing it). Nothing to signal;
        // the caller's retry-acquire will pick up the now-free lock.
        return true;
    };

    if !holder_is_stale_build(&holder.build_hash, installed_build_hash) {
        return false; // live and current (relative to what's installed) -- nothing to prune
    }

    // Exact match against the real CLI binary names only -- a substring
    // check (e.g. "contains infigraph") would also match this very test
    // binary (`infigraph_core-<hash>`) and any other infigraph-* crate's
    // build/test artifacts, defeating the guard entirely.
    let proc_name = proc.name().to_string_lossy().to_ascii_lowercase();
    let looks_like_infigraph = proc_name == "infigraph" || proc_name == "infigraph.exe";
    if !looks_like_infigraph {
        // The PID was recycled for an unrelated process. Leave it alone --
        // the lock file is just stale metadata pointing at a dead holder;
        // nothing here to terminate.
        return false;
    }

    // Alive, current binary differs: ask it to exit. The watch loop already
    // releases watch.lock cleanly on SIGTERM (the same path Ctrl-C/`kill`
    // already trigger), so this is not a hard kill.
    #[cfg(unix)]
    unsafe {
        libc::kill(holder.pid as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &holder.pid.to_string()])
            .output();
    }

    const ATTEMPTS: u32 = 20;
    const DELAY: std::time::Duration = std::time::Duration::from_millis(100);
    wait_for_pid_exit(spid, &mut sys, ATTEMPTS, DELAY)
}

/// Poll `sys` for `pid` up to `attempts` times, `delay` apart. Returns
/// `true` as soon as the PID is no longer found running, `false` if it's
/// still alive after the last attempt. Split out from [`prune_stale_daemon`]
/// so the wait-then-confirm logic is testable against a real, deterministic
/// child process instead of a live SIGTERM/signal-handling race.
fn wait_for_pid_exit(
    pid: sysinfo::Pid,
    sys: &mut sysinfo::System,
    attempts: u32,
    delay: std::time::Duration,
) -> bool {
    for _ in 0..attempts {
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        if sys.process(pid).is_none() {
            return true;
        }
        std::thread::sleep(delay);
    }
    false
}

/// Build (without spawning) the `Command` used to launch a detached
/// `infigraph daemon` child for `root`. Exposed as `pub` — separate from
/// [`spawn_daemon`] — so integration tests can assert on the command's
/// configuration (e.g. its env mutations) directly, without needing to
/// actually spawn and observe a real child process.
pub fn build_daemon_command(root: &Path, tg_dir: &Path, watch_binary: &Path) -> Command {
    // Append, never truncate: multiple spawn attempts can race to acquire
    // watch.lock (see ensure_daemon_running's probe-then-spawn window), and
    // every attempt -- winner and losers alike -- opens this same file for
    // its child's stderr before the race is decided. O_APPEND makes each
    // write atomically seek-to-end, so a losing child's short "another
    // watcher is already running" message lands after the winner's output
    // instead of truncating it away.
    // Append, never truncate: multiple spawn attempts can race to acquire
    // watch.lock (see ensure_daemon_running's probe-then-spawn window), and
    // every attempt -- winner and losers alike -- opens this same file for
    // its child's stderr before the race is decided. O_APPEND makes each
    // write atomically seek-to-end, so a losing child's short "another
    // watcher is already running" message lands after the winner's output
    // instead of truncating it away.
    let log_path = tg_dir.join("daemon.log");
    // R7.3 (#83): cap before handing the file to the child as its stderr.
    // Rotate-at-spawn is the only safe point -- once the fd is inherited,
    // the child appends for its whole lifetime.
    crate::logrotate::rotate_if_over(&log_path, 10 * 1024 * 1024);
    let stderr_target = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => Stdio::from(f),
        Err(_) => Stdio::null(),
    };

    let mut cmd = Command::new(watch_binary);
    cmd.arg("daemon")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_target)
        .env_remove("INFIGRAPH_BACKEND");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    cmd
}

fn spawn_daemon(root: &Path, tg_dir: &Path, watch_binary: &Path) -> DaemonStartOutcome {
    let mut cmd = build_daemon_command(root, tg_dir, watch_binary);
    match cmd.spawn() {
        Ok(_) => DaemonStartOutcome::Spawned,
        Err(e) => DaemonStartOutcome::Failed(e.to_string()),
    }
}

/// Locate the `infigraph` CLI binary as a sibling of the currently-running
/// executable (used by MCP, which has no `watch` subcommand of its own).
pub fn resolve_cli_binary_sibling_of(current_exe: &Path) -> Result<std::path::PathBuf> {
    let dir = current_exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent directory for {}", current_exe.display()))?;
    let name = if cfg!(windows) {
        "infigraph.exe"
    } else {
        "infigraph"
    };
    let candidate = dir.join(name);
    if candidate.exists() {
        return Ok(candidate);
    }
    // `cargo test` integration-test binaries live one level below the real
    // build output directory (target/debug/deps/<test>-<hash> vs
    // target/debug/), so the sibling check above never finds the CLI binary
    // there even though it genuinely exists one directory up. This fallback
    // only matters under that layout: production installs always place
    // `infigraph` and `infigraph-mcp` as true siblings, so the check above
    // already succeeds for them and this branch is never reached.
    if let Some(grandparent) = dir.parent() {
        let grandparent_candidate = grandparent.join(name);
        if grandparent_candidate.exists() {
            return Ok(grandparent_candidate);
        }
    }
    Err(anyhow::anyhow!(
        "expected infigraph CLI binary at {}",
        candidate.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::{daemon_is_alive, holder_is_stale_build, prune_stale_daemon, wait_for_pid_exit};
    use crate::lockfile::LockInfo;

    fn write_lock_info(path: &std::path::Path, info: &LockInfo) {
        std::fs::write(path, serde_json::to_string(info).unwrap()).unwrap();
    }

    #[test]
    fn daemon_is_alive_when_lock_held() {
        let tmp = tempfile::tempdir().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        std::fs::create_dir_all(&tg_dir).unwrap();
        let lock_path = tg_dir.join("watch.lock");

        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        use fs2::FileExt;
        file.lock_exclusive().unwrap();

        assert!(daemon_is_alive(&lock_path));

        file.unlock().unwrap();
        assert!(!daemon_is_alive(&lock_path));
    }

    #[test]
    fn daemon_is_alive_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        // No `.infigraph` dir at all — simulates probing a project that was
        // never indexed. `daemon_is_alive` must be a pure read: it must not
        // create the lock file (or its parent dir) as a side effect of
        // merely checking status.
        let infigraph_dir = tmp.path().join(".infigraph");
        let lock_path = infigraph_dir.join("watch.lock");
        assert!(!infigraph_dir.exists());

        assert!(!daemon_is_alive(&lock_path));

        assert!(
            !infigraph_dir.exists(),
            "daemon_is_alive must not create .infigraph as a side effect of probing"
        );
        assert!(
            !lock_path.exists(),
            "daemon_is_alive must not create watch.lock as a side effect of probing"
        );
    }

    /// A holder PID that no longer exists at all: the lock is simply stale
    /// (the process crashed or was `kill -9`'d without releasing it), so
    /// there's nothing to signal — but it must still be reported as prunable
    /// so the caller retries the acquisition.
    #[test]
    fn prune_stale_daemon_reports_dead_pid_as_prunable() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("watch.lock");

        write_lock_info(
            &lock_path,
            &LockInfo {
                pid: std::process::id() + 1_000_000, // implausible as a real live pid
                role: "cli-watch".to_string(),
                build_hash: "some-old-build".to_string(),
                acquired_at: 0,
                last_heartbeat: 0,
                holder_started_at: 0,
            },
        );

        assert!(
            prune_stale_daemon(&lock_path, Some(crate::build_hash())),
            "a dead holder PID must be reported as prunable"
        );
    }

    /// A holder that's alive and on the current build has nothing to prune
    /// — this is the ordinary "a watcher is already running and up to date"
    /// case, and must be a true no-op (no signal sent, lock untouched). If
    /// this incorrectly sent SIGTERM to our own PID, the test process would
    /// terminate here rather than reach the assertion.
    #[test]
    fn prune_stale_daemon_leaves_live_current_build_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("watch.lock");

        write_lock_info(
            &lock_path,
            &LockInfo {
                pid: std::process::id(),
                role: "cli-watch".to_string(),
                build_hash: crate::build_hash().to_string(),
                acquired_at: 0,
                last_heartbeat: 0,
                holder_started_at: 0,
            },
        );

        assert!(
            !prune_stale_daemon(&lock_path, Some(crate::build_hash())),
            "a live holder on the current build must not be pruned"
        );
    }

    /// #135: staleness is relative to the *installed* binary, never to this
    /// process's own build. A holder on the installed build is current even
    /// when the judge was built from something else; an unknown installed
    /// hash never justifies a signal.
    #[test]
    fn holder_is_stale_build_is_relative_to_the_installed_binary_only() {
        assert!(
            !holder_is_stale_build("installed-hash-X", Some("installed-hash-X")),
            "holder on the installed build is current, whatever this judge was built from"
        );
        assert!(
            holder_is_stale_build("older-hash", Some("installed-hash-X")),
            "holder on a different build than what's installed is stale"
        );
        assert!(
            !holder_is_stale_build("anything", None),
            "unknown installed hash must never be treated as stale"
        );
        // The judge's own hash is irrelevant: a holder matching what's
        // installed is current even though it differs from crate::build_hash().
        assert_ne!("installed-hash-X", crate::build_hash());
        assert!(!holder_is_stale_build(
            "installed-hash-X",
            Some("installed-hash-X")
        ));
    }

    /// PID-reuse guard: a live PID with a mismatched build_hash is only
    /// terminated if the live process at that PID still looks like an
    /// infigraph binary. This test's own PID is alive with a deliberately
    /// mismatched build_hash, but the running process is a `cargo test`
    /// binary (name never contains "infigraph") — so this must be a no-op,
    /// not a self-inflicted SIGTERM.
    #[test]
    fn prune_stale_daemon_does_not_signal_a_live_non_infigraph_process() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("watch.lock");

        write_lock_info(
            &lock_path,
            &LockInfo {
                pid: std::process::id(),
                role: "cli-watch".to_string(),
                build_hash: "definitely-not-the-current-build".to_string(),
                acquired_at: 0,
                last_heartbeat: 0,
                holder_started_at: 0,
            },
        );

        assert!(
            !prune_stale_daemon(&lock_path, Some(crate::build_hash())),
            "a live PID whose process name doesn't look like infigraph must not be pruned/signaled"
        );
    }

    /// An already-exited (and reaped) PID must be reported as gone on the
    /// very first check -- the ordinary "the signal worked" case.
    #[test]
    #[cfg(unix)]
    fn wait_for_pid_exit_returns_true_once_the_child_actually_exits() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = sysinfo::Pid::from_u32(child.id());
        child.wait().unwrap(); // let it fully exit and reap before polling
        let mut sys = sysinfo::System::new();

        assert!(
            wait_for_pid_exit(pid, &mut sys, 5, std::time::Duration::from_millis(10)),
            "an already-exited (and reaped) PID must be reported as gone on the first check"
        );
    }

    /// The gap the old code had: it treated the lock *file's* payload
    /// disappearing as proof the signaled holder was gone, and always
    /// returned `true` regardless -- so a caller could spawn a second
    /// daemon while the first was still fully alive. A process still
    /// running once the wait budget is exhausted must be reported as
    /// NOT confirmed-exited.
    #[test]
    #[cfg(unix)]
    fn wait_for_pid_exit_returns_false_when_the_process_outlives_the_budget() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let pid = sysinfo::Pid::from_u32(child.id());
        let mut sys = sysinfo::System::new();

        assert!(
            !wait_for_pid_exit(pid, &mut sys, 3, std::time::Duration::from_millis(20)),
            "a process still running once the budget is exhausted must not be reported as exited"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// `build_daemon_command` must open `daemon.log` in append mode: a losing
    /// spawn attempt racing against an already-running daemon (see
    /// `ensure_daemon_running`'s probe-then-spawn window) must not truncate
    /// away the winner's existing log content.
    #[test]
    fn build_daemon_command_appends_to_an_existing_log_instead_of_truncating() {
        let tmp = tempfile::tempdir().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        std::fs::create_dir_all(&tg_dir).unwrap();
        std::fs::write(tg_dir.join("daemon.log"), b"previous daemon's output\n").unwrap();

        // Building the command opens (but doesn't write to) the log for the
        // child's stderr; drop it immediately, as a real spawn would hand
        // the fd to the child rather than writing through it here.
        let _cmd =
            super::build_daemon_command(tmp.path(), &tg_dir, std::path::Path::new("infigraph"));

        let contents = std::fs::read_to_string(tg_dir.join("daemon.log")).unwrap();
        assert_eq!(
            contents, "previous daemon's output\n",
            "opening the log for a new spawn attempt must not truncate prior content"
        );
    }

    /// `is_orphaned_daemon` is the pure decision `find_orphaned_daemons`
    /// applies per live process -- exercised here directly with synthetic
    /// facts rather than a real spawned process, since a real daemon's own
    /// self-check (`run_write_coordinator`'s root-existence check)
    /// would race this test's own deletion the moment it's spawned.
    mod is_orphaned_daemon_tests {
        use super::super::is_orphaned_daemon;

        #[test]
        fn matches_a_daemon_process_whose_cwd_is_gone() {
            let tmp = tempfile::tempdir().unwrap();
            let gone = tmp.path().join("gone-project");
            std::fs::create_dir_all(&gone).unwrap();
            std::fs::remove_dir_all(&gone).unwrap();

            assert!(is_orphaned_daemon(
                "infigraph",
                &["/usr/local/bin/infigraph".to_string(), "daemon".to_string()],
                Some(&gone),
            ));
        }

        #[test]
        fn does_not_match_when_cwd_still_exists() {
            let tmp = tempfile::tempdir().unwrap();
            assert!(
                !is_orphaned_daemon(
                    "infigraph",
                    &["/usr/local/bin/infigraph".to_string(), "daemon".to_string()],
                    Some(tmp.path()),
                ),
                "a daemon watching a directory that still exists is healthy, not orphaned"
            );
        }

        #[test]
        fn does_not_match_a_differently_named_binary() {
            let tmp = tempfile::tempdir().unwrap();
            let gone = tmp.path().join("gone-project");
            std::fs::create_dir_all(&gone).unwrap();
            std::fs::remove_dir_all(&gone).unwrap();

            assert!(
                !is_orphaned_daemon(
                    "infigraph-mcp",
                    &[
                        "/usr/local/bin/infigraph-mcp".to_string(),
                        "daemon".to_string()
                    ],
                    Some(&gone),
                ),
                "must not match an unrelated binary that merely shares a substring, \
                 same discipline prune_stale_daemon's own guard documents"
            );
        }

        #[test]
        fn does_not_match_an_infigraph_process_that_is_not_the_daemon_role() {
            let tmp = tempfile::tempdir().unwrap();
            let gone = tmp.path().join("gone-project");
            std::fs::create_dir_all(&gone).unwrap();
            std::fs::remove_dir_all(&gone).unwrap();

            assert!(
                !is_orphaned_daemon(
                    "infigraph",
                    &["/usr/local/bin/infigraph".to_string(), "index".to_string()],
                    Some(&gone),
                ),
                "a plain `infigraph index` invocation must never be mistaken for a daemon"
            );
        }

        #[test]
        fn does_not_match_when_cwd_could_not_be_determined() {
            assert!(
                !is_orphaned_daemon(
                    "infigraph",
                    &["/usr/local/bin/infigraph".to_string(), "daemon".to_string()],
                    None,
                ),
                "no cwd signal at all must never be treated as proof the root is gone"
            );
        }
    }
}
