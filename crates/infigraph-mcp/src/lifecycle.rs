//! Worker/supervisor process lifecycle.
//!
//! The MCP binary runs as a supervisor that spawns itself with `--worker`.
//! If the supervisor dies abnormally (SIGKILL, crash), the worker used to
//! survive re-parented to launchd/init (PPID 1) while still holding the
//! instance lock — blocking every future MCP start until killed by hand.
//!
//! The supervisor passes its PID via `INFIGRAPH_SUPERVISOR_PID`; the worker
//! polls that PID and exits when it disappears. Stdin EOF alone is not
//! enough: the worker inherits the client's pipe (so it outlives a dead
//! supervisor while the client is up), and the `--ui`/`--serve` modes park
//! in infinite sleep loops that never read stdin at all.

use std::time::Duration;

/// Env var carrying the supervisor's PID to the `--worker` child.
pub const SUPERVISOR_PID_ENV: &str = "INFIGRAPH_SUPERVISOR_PID";

/// How often the worker checks that its supervisor is still alive.
const PARENT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Returns whether a process with the given PID currently exists.
///
/// Unix: `kill(pid, 0)` — success or `EPERM` both mean the process exists.
/// Windows: `OpenProcess` + zero-timeout `WaitForSingleObject`; a handle we
/// can't open for a reason other than "no such process" is treated as alive
/// so a healthy worker is never killed spuriously.
/// Other platforms: conservatively returns `true`.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // pid 0 would signal the whole process group, and values that
        // don't fit pid_t would wrap negative (group/broadcast semantics) —
        // neither is a valid single-process PID.
        let Ok(pid_t) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if pid_t <= 0 {
            return false;
        }
        let res = unsafe { libc::kill(pid_t, 0) };
        if res == 0 {
            return true;
        }
        // EPERM: process exists but we can't signal it — still alive.
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };

        if pid == 0 {
            return false;
        }
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // ERROR_INVALID_PARAMETER: no such process. Anything else
            // (e.g. access denied) means it exists but is inaccessible —
            // err on the side of "alive" so we never exit spuriously.
            return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
        }
        // Zero-timeout wait: WAIT_TIMEOUT ⇒ still running; WAIT_OBJECT_0
        // (or failure) ⇒ terminated.
        let res = unsafe { WaitForSingleObject(handle, 0) };
        unsafe { CloseHandle(handle) };
        res == WAIT_TIMEOUT
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

/// Returns the current parent PID on Unix, `None` elsewhere.
fn current_ppid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(unsafe { libc::getppid() } as u32)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// If `INFIGRAPH_SUPERVISOR_PID` is set, spawn a background thread that
/// exits this process once the supervisor is gone. No-op when the env var
/// is absent (e.g. `--worker` launched directly for debugging).
///
/// When the worker is a direct child of the supervisor (the normal case),
/// the check is `getppid() != supervisor_pid`: on supervisor death the
/// kernel re-parents the worker, so this is immune to PID reuse. If the
/// worker is not a direct child (unusual debug setups), it falls back to
/// `process_alive` polling, which can in theory be fooled by PID reuse
/// but never exits a healthy process spuriously.
pub fn spawn_parent_monitor() {
    let Some(pid) = std::env::var(SUPERVISOR_PID_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return;
    };

    let direct_child = current_ppid() == Some(pid);

    let spawned = std::thread::Builder::new()
        .name("parent-monitor".into())
        .spawn(move || loop {
            std::thread::sleep(PARENT_POLL_INTERVAL);
            let gone = if direct_child {
                // Re-parented ⇒ the supervisor died. PID-reuse-proof.
                current_ppid() != Some(pid)
            } else {
                !process_alive(pid)
            };
            if gone {
                // `std::process::exit` skips Drop, so the `InstanceGuard`
                // on `run()`'s stack never fires here -- deregister
                // explicitly first, mirroring the SIGTERM handler's own
                // signal-context cleanup (`instance_path` is `pub` for
                // exactly this: neither this thread nor a signal handler
                // can reach the guard).
                let _ = std::fs::remove_file(infigraph_core::instances::instance_path(
                    std::process::id(),
                ));
                crate::mcp_log(
                    "INFO",
                    &format!("supervisor (pid {pid}) is gone — worker exiting to avoid orphan"),
                );
                std::process::exit(0);
            }
        });
    if let Err(e) = spawned {
        // Worker still exits on stdin EOF in MCP mode; a missing monitor
        // only matters for abnormal supervisor death, so log and continue
        // rather than killing a healthy worker at startup.
        crate::mcp_log(
            "WARN",
            &format!("failed to spawn parent-monitor thread: {e} — orphan reaping disabled"),
        );
    }
}

/// Exit code a worker uses when its watchdog restarts it (R5.2, #19): the
/// supervisor starts a fresh worker instead of exiting. EX_TEMPFAIL.
pub const WATCHDOG_RESTART_EXIT: i32 = 75;

/// Exit code a worker uses after handing `mcp.lock` to a newer build
/// (R2.3.2): the supervisor starts a fresh worker -- from the binary now on
/// disk -- instead of following it out, so the session keeps its server.
pub const HANDOVER_EXIT: i32 = 76;

/// Read-held while the worker runs a tool call (calls over HTTP can run
/// concurrently). The watchdog takes it for writing before restarting the
/// worker, so a restart happens between calls, never inside one.
pub static SERVING: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Exit with `code` for the supervisor to start a fresh worker: once the
/// call in progress, if any, has finished, and with this worker's instance
/// registration removed.
pub fn exit_between_calls(code: i32) -> ! {
    let _between_calls = SERVING.write().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::remove_file(infigraph_core::instances::instance_path(std::process::id()));
    std::process::exit(code);
}

/// R5.2 (#19): watch this worker's own memory, descriptors and threads.
/// Over a soft ceiling, drop the search caches (rebuilt on the next
/// search); over a hard ceiling, wait for the call in progress to finish
/// and exit with [`WATCHDOG_RESTART_EXIT`] so the supervisor starts a
/// fresh worker. Everything a worker holds is rebuilt from disk.
pub fn spawn_self_watch() {
    std::thread::spawn(|| {
        let mut watch = infigraph_core::watchdog::SelfWatch::new("mcp");
        loop {
            std::thread::sleep(Duration::from_secs(1));
            match watch.check(std::time::Instant::now()) {
                infigraph_core::watchdog::Action::DropCaches => {
                    crate::tools::search::drop_search_cache();
                    infigraph_core::embed::invalidate_hnsw_cache();
                }
                infigraph_core::watchdog::Action::Restart(why) => {
                    crate::mcp_log("WATCHDOG", &format!("{why} -- restarting the worker"));
                    exit_between_calls(WATCHDOG_RESTART_EXIT);
                }
                infigraph_core::watchdog::Action::None => {}
            }
        }
    });
}

// #18: startup phases. Every phase that must finish before the worker
// serves has a time limit, so a hung one fails startup naming itself
// instead of blocking the MCP handshake; work that need not finish first
// runs in the background.
infigraph_core::settings! {
    mcp_startup {
        phase_secs: u64 = 30,
    }
}

/// Exit code of a worker whose required startup phase hung or panicked. A
/// plain exit, not a crash or a watchdog restart, so the supervisor exits
/// with it too instead of restarting into the same hang.
pub const STARTUP_PHASE_FAILED_EXIT: i32 = 3;

/// Test hook: the startup phase named here hangs, the way
/// `INFIGRAPH_MCP_DEBUG_STALL_TOOL` stalls a tool.
const STALL_STARTUP_PHASE_ENV: &str = "INFIGRAPH_MCP_DEBUG_STALL_STARTUP_PHASE";

fn stall_if_requested(name: &str) {
    if std::env::var(STALL_STARTUP_PHASE_ENV).ok().as_deref() == Some(name) {
        crate::mcp_log("DEBUG", &format!("stalling startup phase `{name}`"));
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}

/// Time limit for each required startup phase
/// (`INFIGRAPH_MCP_STARTUP_PHASE_SECS`, default 30s).
pub fn startup_phase_budget() -> Duration {
    let settings = McpStartup::resolve_or_default(
        RawMcpStartup::default(),
        infigraph_core::settings_file::ConfigScope::User,
    );
    Duration::from_secs(settings.phase_secs)
}

/// Runs startup phase `name` on its own thread and waits at most `budget`
/// for it. A phase that overruns keeps running detached -- a thread cannot
/// be cancelled -- but startup no longer waits on it.
pub fn startup_phase<T: Send + 'static>(
    name: &'static str,
    budget: Duration,
    phase: impl FnOnce() -> T + Send + 'static,
) -> anyhow::Result<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    std::thread::Builder::new()
        .name(format!("startup:{name}"))
        .spawn(move || {
            stall_if_requested(name);
            let _ = tx.send(phase());
        })?;
    match rx.recv_timeout(budget) {
        Ok(value) => {
            crate::mcp_log(
                "INFO",
                &format!("startup phase `{name}` took {:?}", started.elapsed()),
            );
            Ok(value)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
            "startup phase `{name}` did not finish within {}s",
            budget.as_secs_f64()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(anyhow::anyhow!("startup phase `{name}` panicked"))
        }
    }
}

/// A phase the worker cannot serve without: on a timeout or panic, log it
/// by name and exit with [`STARTUP_PHASE_FAILED_EXIT`].
pub fn required_startup_phase<T: Send + 'static>(
    name: &'static str,
    phase: impl FnOnce() -> T + Send + 'static,
) -> T {
    match startup_phase(name, startup_phase_budget(), phase) {
        Ok(value) => value,
        Err(e) => {
            crate::mcp_log("ERROR", &format!("{e:#} -- exiting"));
            eprintln!("infigraph-mcp: {e:#}");
            std::process::exit(STARTUP_PHASE_FAILED_EXIT);
        }
    }
}

/// A phase that must not delay serving (the startup true-up reindex can
/// take minutes): runs on its own named thread and logs its duration.
pub fn background_startup_phase(name: &'static str, phase: impl FnOnce() + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name(format!("startup:{name}"))
        .spawn(move || {
            let started = std::time::Instant::now();
            stall_if_requested(name);
            phase();
            crate::mcp_log(
                "INFO",
                &format!("startup phase `{name}` finished in {:?}", started.elapsed()),
            );
        });
    if let Err(e) = spawned {
        crate::mcp_log(
            "WARN",
            &format!("could not start startup phase `{name}`: {e}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_alive_true_for_self() {
        assert!(process_alive(std::process::id()));
    }

    /// Regression test for orphan workers: after a child exits and is reaped,
    /// its PID must be reported dead so the parent-monitor terminates the
    /// worker instead of leaving it re-parented to PID 1 holding the lock.
    #[cfg(unix)]
    #[test]
    fn process_alive_false_for_reaped_child() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();
        child.wait().expect("wait for child");
        assert!(
            !process_alive(pid),
            "reaped child pid {pid} must be reported dead"
        );
    }

    #[test]
    fn a_startup_phase_that_finishes_returns_its_value() {
        let got = startup_phase("quick", Duration::from_secs(5), || 42).unwrap();
        assert_eq!(got, 42);
    }

    /// #18: a phase that hangs must fail, naming itself, once its budget is
    /// spent -- not block startup (and the MCP handshake) indefinitely.
    #[test]
    fn a_hung_startup_phase_fails_naming_itself_within_its_budget() {
        let started = std::time::Instant::now();
        let err = startup_phase("stuck_phase", Duration::from_millis(200), || {
            std::thread::sleep(Duration::from_secs(30));
        })
        .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(err.to_string().contains("stuck_phase"), "{err}");
    }

    #[test]
    fn a_panicking_startup_phase_fails_naming_itself() {
        let err = startup_phase("boom_phase", Duration::from_secs(5), || -> u32 {
            panic!("phase blew up")
        })
        .unwrap_err();
        assert!(err.to_string().contains("boom_phase"), "{err}");
    }
}
