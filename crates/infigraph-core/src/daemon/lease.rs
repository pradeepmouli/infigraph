//! The client half of #38/#124: one held connection per (process, project)
//! telling that project's daemon someone still needs it. Idempotent and
//! fire-and-forget -- a lease is an optimisation over respawning, never a
//! correctness requirement, so nothing here blocks, errors or panics.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static HELD: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
static SELF_DAEMON: Mutex<Option<PathBuf>> = Mutex::new(None);

fn key(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn with_held<T>(f: impl FnOnce(&mut HashSet<PathBuf>) -> T) -> T {
    let mut guard = HELD.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashSet::new))
}

/// Called by `cmd_daemon` before its coordinator starts: a daemon holding a
/// lease on itself would never idle out.
pub fn mark_self_daemon(root: &Path) {
    *SELF_DAEMON.lock().unwrap_or_else(|e| e.into_inner()) = Some(key(root));
}

/// Whether this process holds, or is establishing, a lease on `root`'s daemon.
pub fn is_held(root: &Path) -> bool {
    with_held(|h| h.contains(&key(root)))
}

/// Lease `root`'s daemon for the rest of this process's life. Returns at
/// once; the attach happens on a background thread.
pub fn hold(root: &Path) {
    let root = key(root);
    if SELF_DAEMON
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        == Some(&root)
    {
        return;
    }
    if !with_held(|h| h.insert(root.clone())) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("infigraph-lease-hold".into())
        .spawn({
            let root = root.clone();
            move || {
                hold_until_no_daemon(&root);
                with_held(|h| h.remove(&root));
            }
        });
    if spawned.is_err() {
        with_held(|h| h.remove(&root));
    }
}

/// Attach, wait for the daemon to go away, and attach again to a successor
/// while `watch.lock` says there is one (a `daemon-restart`, a build-mismatch
/// respawn) -- so a session that never queries keeps its lease across
/// restarts. Returns once no daemon is left to lease from; the caller's next
/// `hold` (every `Infigraph::init`) starts over.
fn hold_until_no_daemon(root: &Path) {
    let lock = root.join(".infigraph").join("watch.lock");
    loop {
        let Ok(mut stream) = super::read_endpoint::connect_allowing_for_startup(root) else {
            return;
        };
        if super::read_protocol::write_attach(&mut stream, std::process::id()).is_err() {
            return;
        }
        // Blocks until the daemon closes the connection.
        let _ = super::read_protocol::read_len_prefixed(&mut stream);
        if !super::lifecycle::wait_for_daemon_ready(
            &lock,
            super::read_endpoint::DAEMON_STARTUP_GRACE,
        ) {
            return;
        }
    }
}
