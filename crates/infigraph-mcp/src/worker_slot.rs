//! The supervisor's one worker, as a process-group leader it created.
//!
//! The worker used to share the client's process group, so when the
//! supervisor went away nothing could reach what the worker had started (an
//! `infigraph index-docs`), and the worker itself noticed only on its 5s poll
//! (#124). Here it leads a group of its own (`GroupChild::spawn`), and every
//! way the supervisor ends -- a signal, a client that went away, an error, a
//! panic -- stops that group, TERM first and KILL after a short grace, while
//! the leader is still unreaped. A worker that exits by itself has what it
//! left behind swept before it is reaped.
//!
//! The slot is process-global because the signal handler's thread must be able
//! to stop the worker while the main thread polls it; the mutex makes "reap"
//! and "signal" exclusive, so the group is never signalled after the leader is
//! reaped.
//!
//! Windows has no process groups here, so only the direct worker is stopped;
//! Job Objects would be the equivalent (#208).

use std::io;
use std::process::{ChildStdin, ChildStdout, Command, ExitStatus};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use infigraph_core::child::GroupChild;

/// How long a worker told to stop may take before its group is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(1);

static WORKER: Mutex<Option<GroupChild>> = Mutex::new(None);

fn slot() -> MutexGuard<'static, Option<GroupChild>> {
    WORKER.lock().unwrap_or_else(|e| e.into_inner())
}

/// Spawns `command` as the worker, leading its own group, and hands back the
/// pipes the command asked for.
pub fn spawn(command: Command) -> io::Result<(Option<ChildStdin>, Option<ChildStdout>)> {
    let mut group = GroupChild::spawn(command)?;
    let stdin = group.child_mut().stdin.take();
    let stdout = group.child_mut().stdout.take();
    *slot() = Some(group);
    Ok((stdin, stdout))
}

/// The worker's exit status if it has exited, `None` while it runs (or when
/// there is no worker). On an exit, what it left in its group is swept
/// *before* it is reaped.
pub fn exited() -> io::Result<Option<ExitStatus>> {
    let mut guard = slot();
    let Some(group) = guard.as_mut() else {
        return Ok(None);
    };
    if !group.exited()? {
        return Ok(None);
    }
    let status = group.reap()?;
    *guard = None;
    Ok(Some(status))
}

/// Stops the worker and its group: TERM, up to `grace` for it to exit, then
/// KILL. A zero `grace` kills at once. A no-op when there is no worker.
pub fn stop(grace: Duration) {
    if let Some(mut group) = slot().take() {
        group.stop(grace);
    }
}

/// [`stop`] for a panic hook: immediate, and it gives up rather than wait if
/// the main thread holds the slot.
pub fn stop_if_free() {
    if let Ok(mut guard) = WORKER.try_lock() {
        if let Some(mut group) = guard.take() {
            group.stop(Duration::ZERO);
        }
    }
}
