//! The client half of #38/#124: one held connection per (process, project)
//! telling that project's daemon someone still needs it. Idempotent and
//! fire-and-forget -- a lease is an optimisation over respawning, never a
//! correctness requirement, so nothing here blocks, errors or panics.
//!
//! A lease this process has not used for `daemon.client_release_secs`
//! is released (unix only), so an idle session stops keeping its daemon
//! alive. The next use -- `hold` on every `Infigraph::init`, [`in_use`]
//! around every routed read and write -- leases again, and
//! `ensure_daemon_for_routed_access` respawns a daemon that has since
//! exited. A [`pin`], a use in progress, or the process's
//! [`set_release_guard`] veto keeps a lease past the idle period.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The deadline a lease thread is currently waiting against for a daemon to
/// connect to. Shared so a `hold_spawned` for a root still waiting can push
/// it out rather than being a no-op (#203 M1).
type Deadline = Arc<Mutex<Instant>>;

/// One lease this process holds, or is establishing.
struct Lease {
    deadline: Deadline,
    /// When this process last used the root: the idle-release clock.
    last_use: Mutex<Instant>,
    /// Uses in progress ([`InUse`]); an idle release waits them out.
    in_use: AtomicUsize,
    /// The parked connection's shutdown handle and whether the lease was
    /// released, under one lock, so a release cannot miss a connection
    /// parked just after it looked.
    hangup: Mutex<Hangup>,
}

#[derive(Default)]
struct Hangup {
    #[cfg(unix)]
    handle: Option<super::read_endpoint::LeaseShutdown>,
    released: bool,
}

impl Lease {
    fn new(in_use: usize) -> Self {
        Self {
            deadline: Arc::new(Mutex::new(Instant::now())),
            last_use: Mutex::new(Instant::now()),
            in_use: AtomicUsize::new(in_use),
            hangup: Mutex::new(Hangup::default()),
        }
    }

    fn touch(&self) {
        *lock(&self.last_use) = Instant::now();
    }

    #[cfg(unix)]
    fn idle_for(&self) -> Duration {
        lock(&self.last_use).elapsed()
    }

    fn released(&self) -> bool {
        lock(&self.hangup).released
    }

    /// Record the parked connection so a release can end it. False if the
    /// lease was released meanwhile: the caller drops the connection.
    fn park(&self, stream: &super::read_endpoint::ReadStream) -> bool {
        // Mutated only where there is a handle to store (unix).
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut hangup = lock(&self.hangup);
        #[cfg(unix)]
        {
            hangup.handle = stream.lease_shutdown();
        }
        #[cfg(not(unix))]
        let _ = stream;
        !hangup.released
    }

    /// Forget the parked connection before it is dropped: the handle is a
    /// bare fd, valid only while the stream lives.
    fn unpark(&self) {
        #[cfg(unix)]
        {
            lock(&self.hangup).handle = None;
        }
    }

    /// End the lease: its parked connection sees EOF and its thread exits
    /// without attaching again.
    #[cfg(unix)]
    fn release(&self) {
        let mut hangup = lock(&self.hangup);
        hangup.released = true;
        if let Some(handle) = &hangup.handle {
            handle.shutdown();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

static HELD: Mutex<Option<HashMap<PathBuf, Arc<Lease>>>> = Mutex::new(None);
static SELF_DAEMON: Mutex<Option<PathBuf>> = Mutex::new(None);
static PINNED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
/// See [`set_release_guard`].
type ReleaseGuard = fn(&Path) -> bool;
static RELEASE_GUARD: Mutex<Option<ReleaseGuard>> = Mutex::new(None);
static OVERRIDES: Mutex<Option<HashMap<PathBuf, Overrides>>> = Mutex::new(None);

/// Per-root test overrides, per root so they cannot leak into a
/// concurrently running test.
#[derive(Clone, Copy, Default)]
struct Overrides {
    grace: Option<Duration>,
    release_after: Option<Duration>,
}

fn overrides(root: &Path) -> Overrides {
    lock(&OVERRIDES)
        .as_ref()
        .and_then(|m| m.get(root).copied())
        .unwrap_or_default()
}

fn set_override(root: &Path, f: impl FnOnce(&mut Overrides)) {
    f(lock(&OVERRIDES)
        .get_or_insert_with(HashMap::new)
        .entry(key(root))
        .or_default());
}

/// How long a lease waits for a daemon to come up: the startup grace, or a
/// per-root test override.
fn grace_for(root: &Path) -> Duration {
    overrides(root)
        .grace
        .unwrap_or(super::read_endpoint::DAEMON_STARTUP_GRACE)
}

/// How long a lease may go unused before it is released; zero never.
#[cfg(unix)]
fn release_after_for(root: &Path) -> Duration {
    overrides(root).release_after.unwrap_or_else(|| {
        Duration::from_secs(super::daemon_idle_settings(root).client_release_secs)
    })
}

/// Test-only: shorten `root`'s startup grace so a test can wait one out.
#[doc(hidden)]
pub fn set_grace_for_test(root: &Path, grace: Duration) {
    set_override(root, |o| o.grace = Some(grace));
}

/// Test-only: shorten `root`'s idle-release period. Applies to leases taken
/// after the call.
#[doc(hidden)]
pub fn set_release_after_for_test(root: &Path, after: Duration) {
    set_override(root, |o| o.release_after = Some(after));
}

fn key(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn with_held<T>(f: impl FnOnce(&mut HashMap<PathBuf, Arc<Lease>>) -> T) -> T {
    f(lock(&HELD).get_or_insert_with(HashMap::new))
}

/// Called by `cmd_daemon` before its coordinator starts: a daemon holding a
/// lease on itself would never idle out.
pub fn mark_self_daemon(root: &Path) {
    *lock(&SELF_DAEMON) = Some(key(root));
}

/// Whether this process is `root`'s daemon.
pub fn is_self_daemon(root: &Path) -> bool {
    lock(&SELF_DAEMON).as_ref() == Some(&key(root))
}

/// Whether this process holds, or is establishing, a lease on `root`'s daemon.
pub fn is_held(root: &Path) -> bool {
    with_held(|h| h.contains_key(&key(root)))
}

/// Keep `root`'s lease past its idle period until [`unpin`]. Survives a
/// release: a pin taken while no lease is held applies to the next one.
pub fn pin(root: &Path) {
    lock(&PINNED)
        .get_or_insert_with(HashSet::new)
        .insert(key(root));
}

/// Undo a [`pin`]; the idle clock was running all along.
pub fn unpin(root: &Path) {
    if let Some(p) = lock(&PINNED).as_mut() {
        p.remove(&key(root));
    }
}

#[cfg(unix)]
fn is_pinned(root: &Path) -> bool {
    lock(&PINNED).as_ref().is_some_and(|p| p.contains(root))
}

/// Install this process's veto over idle releases: asked with the canonical
/// root just before an idle lease would be released, `true` keeps it for
/// another period. MCP keeps a lease while it runs a watcher of its own on
/// that root. It runs on the release thread and must not call into this
/// module. Replaces any earlier guard.
pub fn set_release_guard(guard: ReleaseGuard) {
    *lock(&RELEASE_GUARD) = Some(guard);
}

#[cfg(unix)]
fn guard_keeps(root: &Path) -> bool {
    let guard = *lock(&RELEASE_GUARD);
    guard.is_some_and(|g| g(root))
}

/// Lease `root`'s daemon, and count this as a use of it. Returns at once;
/// the attach happens on a background thread.
pub fn hold(root: &Path) {
    hold_inner(root, false, false);
}

/// As [`hold`], for a daemon this process has just spawned: the child takes
/// `watch.lock` only once it is up, so the attach first waits for it (up to
/// the startup grace) instead of concluding there is no daemon.
pub(crate) fn hold_spawned(root: &Path) {
    hold_inner(root, true, false);
}

/// A use of `root`'s daemon in progress: while any is alive its lease is
/// not released, and ending one restarts the idle clock. Leases again if an
/// earlier lease was released.
#[must_use = "the use ends when this is dropped"]
pub struct InUse(Option<Arc<Lease>>);

impl Drop for InUse {
    fn drop(&mut self) {
        if let Some(lease) = &self.0 {
            lease.touch();
            lease.in_use.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Mark a routed read or write on `root` as in progress; see [`InUse`].
pub fn in_use(root: &Path) -> InUse {
    InUse(hold_inner(root, false, true))
}

/// Lease `root` (a no-op if leased already) and touch its idle clock; with
/// `claim`, also count a use in progress, under the same lock that an idle
/// release checks it under.
fn hold_inner(root: &Path, just_spawned: bool, claim: bool) -> Option<Arc<Lease>> {
    let root = key(root);
    if lock(&SELF_DAEMON).as_ref() == Some(&root) {
        return None;
    }
    let grace = grace_for(&root);
    let (lease, fresh) = with_held(|h| match h.get(&root) {
        // Already leased or waiting. A fresh spawn re-arms a wait that may
        // be about to give up on the daemon it is replacing.
        Some(existing) => {
            if just_spawned {
                let rearmed = Instant::now() + grace;
                let mut at = lock(&existing.deadline);
                *at = (*at).max(rearmed);
            }
            existing.touch();
            if claim {
                existing.in_use.fetch_add(1, Ordering::SeqCst);
            }
            (existing.clone(), false)
        }
        None => {
            let lease = Arc::new(Lease::new(usize::from(claim)));
            h.insert(root.clone(), lease.clone());
            (lease, true)
        }
    });
    if fresh {
        start_lease(root, lease.clone(), just_spawned, grace);
    }
    Some(lease)
}

fn start_lease(root: PathBuf, lease: Arc<Lease>, just_spawned: bool, grace: Duration) {
    let spawned = std::thread::Builder::new()
        .name("infigraph-lease-hold".into())
        .spawn({
            let (root, lease) = (root.clone(), lease.clone());
            move || {
                hold_until_no_daemon(&root, just_spawned, grace, &lease);
                forget(&root, &lease);
            }
        });
    let Ok(_) = spawned else {
        forget(&root, &lease);
        return;
    };
    #[cfg(unix)]
    {
        let after = release_after_for(&root);
        if !after.is_zero() {
            let _ = std::thread::Builder::new()
                .name("infigraph-lease-idle".into())
                .spawn(move || release_when_idle(&root, &lease, after));
        }
    }
}

/// Drop `root`'s entry if it is still `lease` -- a released lease's entry
/// may already belong to its successor.
fn forget(root: &Path, lease: &Arc<Lease>) {
    with_held(|h| {
        if h.get(root).is_some_and(|l| Arc::ptr_eq(l, lease)) {
            h.remove(root);
        }
    });
}

/// What to do with a lease that has gone `idle` against a release period of
/// `after` (zero never releases).
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
enum Decision {
    Keep,
    KeptByGuard,
    Release,
}

/// The guard is asked last, and only about a lease that would otherwise go.
#[cfg_attr(not(unix), allow(dead_code))]
fn release_decision(
    idle: Duration,
    after: Duration,
    pinned: bool,
    in_use: bool,
    guard_keeps: impl FnOnce() -> bool,
) -> Decision {
    if after.is_zero() || idle < after || pinned || in_use {
        Decision::Keep
    } else if guard_keeps() {
        Decision::KeptByGuard
    } else {
        Decision::Release
    }
}

/// The idle-release watchdog for one lease. Returns once the lease ends,
/// whether released here or because its thread found no daemon left.
#[cfg(unix)]
fn release_when_idle(root: &Path, lease: &Arc<Lease>, after: Duration) {
    let tick = (after / 4).clamp(Duration::from_millis(10), Duration::from_secs(60));
    loop {
        std::thread::sleep(tick);
        if !with_held(|h| h.get(root).is_some_and(|l| Arc::ptr_eq(l, lease))) {
            return;
        }
        let idle = lease.idle_for();
        let busy = lease.in_use.load(Ordering::SeqCst) > 0;
        match release_decision(idle, after, is_pinned(root), busy, || guard_keeps(root)) {
            Decision::Keep => {}
            Decision::KeptByGuard => {
                eprintln!(
                    "[lease] {} kept by guard after {}s idle",
                    root.display(),
                    idle.as_secs()
                );
                lease.touch();
            }
            Decision::Release => {
                // Re-checked under the lock `in_use` claims under: a use
                // that began (or ended) since would be cut off.
                let gone = with_held(|h| {
                    let current = h.get(root).is_some_and(|l| Arc::ptr_eq(l, lease));
                    let still_idle =
                        lease.in_use.load(Ordering::SeqCst) == 0 && lease.idle_for() >= after;
                    if current && still_idle {
                        h.remove(root);
                    }
                    current && still_idle
                });
                if gone {
                    eprintln!(
                        "[lease] releasing {} after {}s idle",
                        root.display(),
                        idle.as_secs()
                    );
                    lease.release();
                    return;
                }
            }
        }
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
fn hold_until_no_daemon(root: &Path, just_spawned: bool, grace: Duration, lease: &Lease) {
    let deadline = &lease.deadline;
    // With no `watch.lock` file no daemon has ever run here: nothing to wait
    // for. Existence is a stat, not a probe. A spawn's trial lock creates the
    // file, but a spawned daemon is waited for regardless.
    let mut budget = if just_spawned || root.join(".infigraph").join("watch.lock").exists() {
        grace
    } else {
        Duration::ZERO
    };
    // Consecutive attaches that ended without an ack. One is ambiguous -- a
    // daemon shutting down (its listener still bound until its accept thread
    // is joined) looks exactly like one that refuses leases -- so it earns a
    // pause and a retry, which reaches the successor after a restart. Two in
    // a row is a daemon that does not support leases: stop.
    let mut unacked = 0;
    loop {
        if lease.released() {
            return;
        }
        *lock(deadline) = Instant::now() + budget;
        let Some(mut stream) = connect_within(root, deadline) else {
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
            std::thread::sleep(Duration::from_secs(1));
            budget = grace;
            continue;
        }
        unacked = 0;
        if !lease.park(&stream) {
            lease.unpark();
            return;
        }
        // Blocks until the daemon closes the connection, or an idle release
        // shuts it down.
        let _ = super::read_protocol::read_len_prefixed(&mut stream);
        lease.unpark();
        budget = grace;
    }
}

/// Connect to `root`'s read endpoint, retrying until `deadline` passes (one
/// attempt if it already has). The deadline is re-read on every retry, so a
/// `hold_spawned` that re-arms it extends this wait.
fn connect_within(root: &Path, deadline: &Deadline) -> Option<super::read_endpoint::ReadStream> {
    let endpoint = super::read_endpoint::ReadEndpoint::for_root(root);
    loop {
        if let Ok(stream) = endpoint.connect() {
            return Some(stream);
        }
        if Instant::now() >= *lock(deadline) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The release decision, alone: idle for at least the release period,
    /// and nothing -- a pin, a use in progress, the guard -- keeps it. The
    /// guard is asked last, and only when it would otherwise release.
    #[test]
    fn release_decision_releases_only_an_idle_unkept_lease() {
        let s = Duration::from_secs;
        let never = || -> bool { panic!("the guard is asked only about a release") };
        let d =
            |idle, after, pinned, busy| release_decision(s(idle), s(after), pinned, busy, never);
        assert_eq!(d(9, 10, false, false), Decision::Keep);
        assert_eq!(d(0, 0, false, false), Decision::Keep, "0 never releases");
        assert_eq!(d(99, 0, false, false), Decision::Keep, "0 never releases");
        assert_eq!(d(10, 10, true, false), Decision::Keep, "pinned");
        assert_eq!(d(10, 10, false, true), Decision::Keep, "in use");
        assert_eq!(
            release_decision(s(10), s(10), false, false, || false),
            Decision::Release,
            "the period is inclusive"
        );
        assert_eq!(
            release_decision(s(10), s(10), false, false, || true),
            Decision::KeptByGuard
        );
    }

    /// #203 M1: a lease thread still waiting for a (slow) successor has its
    /// deadline pushed out by a `hold_spawned` for the same root, instead of
    /// that call being a no-op and the thread giving up before the successor
    /// binds.
    #[test]
    fn hold_spawned_rearms_a_lease_still_waiting_for_its_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        std::fs::write(root.join(".infigraph").join("watch.lock"), b"").unwrap();
        set_grace_for_test(&root, std::time::Duration::from_millis(500));

        hold(&root); // waits up to 500ms for a daemon
        std::thread::sleep(std::time::Duration::from_millis(400));
        hold_spawned(&root); // a respawn: re-arm the wait
        std::thread::sleep(std::time::Duration::from_millis(300)); // past the first deadline

        let liveness = std::sync::Arc::new(super::super::liveness::Liveness::new());
        let _svc = super::super::read_service::ReadService::start_serving(
            &root,
            std::sync::Arc::new(|| None),
            None,
            2,
            liveness.clone(),
            None,
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while liveness.leases() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            liveness.leases(),
            1,
            "the re-armed lease must reach the successor"
        );
    }

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
            None,
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
