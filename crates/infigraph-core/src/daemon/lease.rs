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
    hold_inner(root, false);
}

/// As [`hold`], for a daemon this process has just spawned: the child takes
/// `watch.lock` only once it is up, so the attach first waits for it (up to
/// the startup grace) instead of concluding there is no daemon.
pub(crate) fn hold_spawned(root: &Path) {
    hold_inner(root, true);
}

fn hold_inner(root: &Path, just_spawned: bool) {
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
                hold_until_no_daemon(&root, just_spawned);
                with_held(|h| h.remove(&root));
            }
        });
    if spawned.is_err() {
        with_held(|h| h.remove(&root));
    }
}

/// Attach, wait for the daemon to go away, and attach again to a successor
/// that binds within the startup grace (a `daemon-restart`, a build-mismatch
/// respawn) -- so a session that never queries keeps its lease across
/// restarts. Returns once no daemon is left to lease from; the caller's next
/// `hold` (every `Infigraph::init`) starts over.
///
/// Never probes `watch.lock`. `daemon_is_alive` probes by briefly *taking*
/// the lock, so a lease thread that probed would make other probers in this
/// process -- the caller's own `wait_for_daemon_ready` right after a spawn --
/// read "alive" while no daemon holds it, and could take the lock out from
/// under a daemon that is starting. A successful connect is the one signal
/// that is both conclusive and free of side effects.
fn hold_until_no_daemon(root: &Path, just_spawned: bool) {
    // With no `watch.lock` file no daemon has ever run here: nothing to wait
    // for. Existence is a stat, not a probe. A spawn's trial lock creates the
    // file, but a spawned daemon is waited for regardless.
    let mut budget = if just_spawned || root.join(".infigraph").join("watch.lock").exists() {
        super::read_endpoint::DAEMON_STARTUP_GRACE
    } else {
        std::time::Duration::ZERO
    };
    // Consecutive attaches that ended without an ack. One is ambiguous -- a
    // daemon shutting down (its listener still bound until its accept thread
    // is joined) looks exactly like one that refuses leases -- so it earns a
    // pause and a retry, which reaches the successor after a restart. Two in
    // a row is a daemon that does not support leases: stop.
    let mut unacked = 0;
    loop {
        let Some(mut stream) = connect_within(root, budget) else {
            return;
        };
        if super::read_protocol::write_attach(&mut stream, std::process::id()).is_err() {
            return;
        }
        // The daemon acks a parked lease. EOF without one means it does not
        // support leases (a build from before them) or could not park this
        // one: stop, and let the next `hold` try again, rather than
        // reconnecting in a loop -- each attempt also costs that daemon a
        // log line.
        if !matches!(
            super::read_protocol::read_frame(&mut stream),
            Ok(Some(super::read_protocol::ReadFrame::End))
        ) {
            unacked += 1;
            if unacked >= 2 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            budget = super::read_endpoint::DAEMON_STARTUP_GRACE;
            continue;
        }
        unacked = 0;
        // Blocks until the daemon closes the connection.
        let _ = super::read_protocol::read_len_prefixed(&mut stream);
        budget = super::read_endpoint::DAEMON_STARTUP_GRACE;
    }
}

/// Connect to `root`'s read endpoint, retrying until `budget` runs out (one
/// attempt for a zero budget).
fn connect_within(
    root: &Path,
    budget: std::time::Duration,
) -> Option<super::read_endpoint::ReadStream> {
    let endpoint = super::read_endpoint::ReadEndpoint::for_root(root);
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Ok(stream) = endpoint.connect() {
            return Some(stream);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `daemon_is_alive` probes by briefly taking `watch.lock`, so a lease
    /// thread that probed it would make other probers in this process -- the
    /// caller's own `wait_for_daemon_ready` right after a spawn -- read
    /// "alive" while no daemon holds it (the docs daemon-start test failed
    /// 2/3 this way), and could take the lock from a daemon that is starting.
    /// A successful probe stamps its role into the file and its drop clears
    /// it again, so an unchanged mtime proves the lease thread never took it.
    #[test]
    fn a_pending_lease_never_probes_watch_lock() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        let lock = root.join(".infigraph").join("watch.lock");
        // As after a spawn: the spawn path's trial probe leaves the file
        // behind, unlocked, before the child takes it.
        std::fs::write(&lock, b"").unwrap();
        let before = std::fs::metadata(&lock).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        hold_spawned(&root);
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert_eq!(
            std::fs::metadata(&lock).unwrap().modified().unwrap(),
            before,
            "the lease thread took watch.lock (a probe stamped and cleared it)"
        );
    }

    /// A just-spawned daemon has no `watch.lock` yet; `hold_spawned` must
    /// wait for it rather than give up, or MCP boot's fresh spawn is never
    /// leased.
    #[cfg(unix)]
    #[test]
    fn hold_spawned_waits_for_the_daemon_to_come_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let graph = root.join(".infigraph").join("graph");
        std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
        drop(crate::graph::GraphStore::open(&graph).unwrap());
        let store = std::sync::Arc::new(crate::graph::GraphStore::open(&graph).unwrap());

        hold_spawned(&root);
        std::thread::sleep(std::time::Duration::from_millis(500));

        // The "daemon" comes up only now.
        let _lock = crate::lockfile::try_acquire(&root.join(".infigraph").join("watch.lock"), "t")
            .unwrap()
            .unwrap();
        let liveness = std::sync::Arc::new(super::super::liveness::Liveness::new());
        let _svc = super::super::read_service::ReadService::start_serving(
            &root,
            std::sync::Arc::new(move || Some(store.clone())),
            None,
            2,
            liveness.clone(),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while liveness.leases() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            liveness.leases(),
            1,
            "the spawned daemon must be leased once it is up"
        );
    }
}
