//! Cross-process "ensure a watcher daemon is running for this repo"
//! primitive, shared by the CLI's `infigraph watch` auto-start and MCP's
//! opportunistic/bootstrap watch-start paths (toggle-gated — see
//! `watch_daemon_mode_enabled`). Generalizes what used to be CLI-only
//! (`spawn_watcher`/`ensure_watcher_running` in `infigraph-cli`), which
//! assumed the calling process *was* the binary to re-exec
//! (`std::env::current_exe()`) — that assumption breaks when the caller is
//! `infigraph-mcp`, which has no `watch` subcommand of its own. Callers now
//! pass the target binary path explicitly.

use std::path::Path;
use std::process::{Command, Stdio};

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
    std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false)
}

/// Opt-in toggle for the external-daemon watcher model. Off by default:
/// existing in-process-thread behavior (an MCP worker spawning a watcher
/// thread that dies with the worker) is unchanged unless this is set to
/// `"1"`.
pub fn watch_daemon_mode_enabled() -> bool {
    std::env::var("INFIGRAPH_WATCH_DAEMON")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Outcome of an `ensure_daemon_running` call.
#[derive(Debug, PartialEq, Eq)]
pub enum DaemonStartOutcome {
    /// A daemon is already alive for this repo (lock held), or watching is
    /// inapplicable (CI / remote backend) — no-op either way.
    AlreadyRunning,
    /// This call won the lock race and spawned a new daemon process.
    Spawned,
    /// Spawn was attempted but failed (e.g. binary not found).
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
pub fn ensure_daemon_running(root: &Path, watch_binary: &Path) -> DaemonStartOutcome {
    if is_ci_env() || is_remote_backend() {
        return DaemonStartOutcome::AlreadyRunning;
    }

    let tg_dir = root.join(".infigraph");
    if !tg_dir.exists() {
        return DaemonStartOutcome::Failed("not an indexed project (.infigraph missing)".into());
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
        Ok(None) => DaemonStartOutcome::AlreadyRunning,
        Err(e) => DaemonStartOutcome::Failed(e.to_string()),
    }
}

fn spawn_daemon(root: &Path, tg_dir: &Path, watch_binary: &Path) -> DaemonStartOutcome {
    let log_path = tg_dir.join("watch.log");
    let stderr_target = match std::fs::File::create(&log_path) {
        Ok(f) => Stdio::from(f),
        Err(_) => Stdio::null(),
    };

    let mut cmd = Command::new(watch_binary);
    cmd.arg("watch")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_target);

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
        Ok(candidate)
    } else {
        Err(anyhow::anyhow!(
            "expected infigraph CLI binary at {}",
            candidate.display()
        ))
    }
}
