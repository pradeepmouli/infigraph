//! Naming for the daemon's read-service endpoint.
//!
//! Deliberately NOT a path under `.infigraph/`. macOS caps a Unix socket's
//! `sun_path` at about 104 bytes, and this project's own worktree
//! convention (`scratchpad/wt-*`) already produces 94-byte candidates --
//! the failure mode is an obscure bind error, not a clear one. Windows
//! named pipes are not filesystem paths at all, so the identifier is opaque
//! from the outset.

use std::io::{self, Read, Write};
use std::path::Path;

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToNsName};

/// An opaque local-socket identity for one project's read service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadEndpoint {
    name: String,
}

impl ReadEndpoint {
    /// Derive the endpoint for a project root. The root is canonicalised
    /// when possible so that `.` and a symlinked path reach the same daemon.
    ///
    /// The hash is `embed::fnv1a64`, not `DefaultHasher`: this name is a
    /// cross-process rendezvous, and `DefaultHasher`'s output is only
    /// guaranteed stable within one Rust release. A daemon and a CLI built
    /// by different toolchains must still agree on it -- binary skew is
    /// routine enough here that `daemon::warn_if_cli_build_differs` exists
    /// for it. FNV-1a's constants are fixed forever.
    pub fn for_root(root: &Path) -> Self {
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let hash = crate::embed::fnv1a64(canonical.to_string_lossy().as_bytes());
        Self {
            name: format!("infigraph-read-{hash:016x}"),
        }
    }

    /// The transport-level name. Fixed length regardless of project depth.
    pub fn as_name(&self) -> String {
        self.name.clone()
    }

    /// Listen on this endpoint. Server side; the daemon calls this once.
    ///
    /// Reclaims a socket left behind by a daemon that died without running
    /// `Drop`. `reclaim_name` (on by default) unlinks on drop, which is
    /// exactly what a hard exit skips -- so without `try_overwrite` a
    /// single ungraceful exit locks every later daemon out of that
    /// project's endpoint permanently, and every routed read for it fails
    /// with "no daemon read service is listening". 120 orphaned sockets had
    /// accumulated in `/tmp` before this was found, none held by any
    /// process.
    ///
    /// Overwriting is safe *here* because of who calls it: the read service
    /// binds only from a daemon that already holds `watch.lock` for this
    /// root, and that lock is what makes a daemon exclusive. So an
    /// `AddrInUse` means the previous holder is gone, not that a live peer
    /// is serving.
    ///
    /// The spin bound matters -- `try_overwrite` spins unboundedly under
    /// contention by default, and a daemon startup path is the wrong place
    /// to hang.
    pub fn bind(&self) -> io::Result<ReadListener> {
        let name = self.name.as_str().to_ns_name::<GenericNamespaced>()?;
        Ok(ReadListener {
            inner: ListenerOptions::new()
                .name(name)
                .try_overwrite(true)
                .max_spin_time(std::time::Duration::from_secs(5))
                .create_sync()?,
        })
    }

    /// Best-effort removal of this endpoint's socket, for a process that is
    /// about to exit without dropping its listener.
    ///
    /// The other half of the belt-and-braces pair with `bind`'s
    /// `try_overwrite`: reclaiming on bind fixes the *next* daemon, but a
    /// client that never retries still sees a refused connection in the
    /// meantime, and a socket nobody ever binds again is simply litter.
    /// Neither mechanism covers the other's gap -- `SIGKILL` runs no code at
    /// all, so only `bind` saves that case, while this one keeps `/tmp`
    /// clean for endpoints no daemon returns to.
    ///
    /// Deliberately infallible: every caller is on an exit path, where
    /// there is nothing useful to do with an error and nowhere to report
    /// it.
    pub fn unlink(&self) {
        #[cfg(unix)]
        for dir in Self::namespace_dirs() {
            let _ = std::fs::remove_file(dir.join(&self.name));
        }
    }

    /// Where a `GenericNamespaced` socket can land on Unix, in the order
    /// `interprocess` tries them: `/run/user/<uid>/`, then `/tmp/`
    /// (`uds_local_socket.rs`, `write_run_user` falling back to `tmpdir()`).
    /// `$TMPDIR` is consulted only on Android. On macOS `/run/user` does not
    /// exist, so every socket is in `/tmp` -- which is why orphans were found
    /// there while `$TMPDIR` pointed at `/var/folders/.../T/`, and why macOS's
    /// three-day `/tmp` reaping can remove a live daemon's socket (#187).
    ///
    /// On Linux the namespace is abstract and there is no file anywhere, so
    /// every lookup here comes up empty and `unlink` and the sweep are no-ops.
    /// Windows named pipes are not files; there is nothing to remove, and
    /// they vanish with the process that made them.
    #[cfg(unix)]
    fn namespace_dirs() -> Vec<std::path::PathBuf> {
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        vec![
            std::path::PathBuf::from(format!("/run/user/{uid}")),
            std::path::PathBuf::from("/tmp"),
        ]
    }

    /// The socket file this endpoint currently has on disk, or `None` --
    /// nothing bound, or a transport with no file at all (Linux's abstract
    /// namespace, Windows named pipes).
    ///
    /// Exists so a daemon can notice the file being removed out from under
    /// its live listener. macOS reaps `/tmp` entries older than about three
    /// days, and a daemon that outlived that kept its fd, its `watch.lock`
    /// and every sign of health while no client could reach it (#187).
    pub fn socket_path(&self) -> Option<std::path::PathBuf> {
        #[cfg(unix)]
        {
            Self::namespace_dirs()
                .into_iter()
                .map(|dir| dir.join(&self.name))
                .find(|path| path.exists())
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Open a connection to a listening read service. Client side.
    pub fn connect(&self) -> io::Result<ReadStream> {
        let name = self.name.as_str().to_ns_name::<GenericNamespaced>()?;
        Ok(ReadStream {
            inner: interprocess::local_socket::Stream::connect(name)?,
        })
    }
}

/// How long to keep trying while a daemon is demonstrably alive but has not
/// bound its read endpoint yet.
///
/// The CLI takes `watch.lock` -- every caller's "the daemon is ready" signal
/// -- before `run_write_coordinator` is entered, so a client that starts a
/// daemon and immediately reads can arrive first. Generous because the
/// coordinator builds the bundled language registry on the way, which costs
/// seconds in a debug build.
pub const DAEMON_STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Connect, tolerating a daemon that is starting but not yet listening.
///
/// The grace period applies *only* while `watch.lock` says a daemon is
/// alive. With no daemon there is nothing to wait for, so the error is
/// immediate -- a CLI run with the daemon down must not hang for 30s
/// before reporting it.
pub fn connect_allowing_for_startup(root: &Path) -> anyhow::Result<ReadStream> {
    use anyhow::Context as _;
    let endpoint = ReadEndpoint::for_root(root);
    let mut last = match endpoint.connect() {
        Ok(stream) => return Ok(stream),
        Err(e) => e,
    };
    let lock = root.join(".infigraph").join("watch.lock");
    let deadline = std::time::Instant::now() + DAEMON_STARTUP_GRACE;
    while std::time::Instant::now() < deadline && crate::daemon::lifecycle::daemon_is_alive(&lock) {
        std::thread::sleep(std::time::Duration::from_millis(50));
        match endpoint.connect() {
            Ok(stream) => return Ok(stream),
            Err(e) => last = e,
        }
    }
    Err(last).with_context(|| "no daemon read service is listening for this project")
}

/// Prefix every read endpoint's socket shares. Also what makes a sweep
/// safe: anything else in the directory is somebody else's.
#[cfg(unix)]
const ENDPOINT_PREFIX: &str = "infigraph-read-";

/// Remove read-service sockets no daemon is listening on, across every
/// directory a `GenericNamespaced` socket can land in.
///
/// `ReadEndpoint::unlink` and `cmd_kill`'s sweep only reach endpoints
/// somebody still knows about -- the daemon's own on its way out, or a root
/// in the registry. A daemon for an *unregistered* root, overwhelmingly a
/// test tempdir, is covered by neither, so its socket outlives it forever.
/// 120 had accumulated before this was noticed, and ten more appeared
/// within half an hour of clearing them (#162).
///
/// Returns how many were removed. Best-effort throughout: this runs on a
/// startup path where a directory that cannot be read is not worth failing
/// over.
pub fn sweep_orphaned_endpoints(older_than: std::time::Duration) -> usize {
    #[cfg(unix)]
    {
        ReadEndpoint::namespace_dirs()
            .iter()
            .map(|d| sweep_orphaned_endpoints_in(d, older_than))
            .sum()
    }
    #[cfg(not(unix))]
    {
        let _ = older_than;
        0
    }
}

/// [`sweep_orphaned_endpoints`] for one directory, so tests can point it at
/// a tempdir instead of the real namespace.
///
/// Liveness is decided by *connecting*: a listening daemon accepts, an
/// orphan refuses. That needs no external tooling and no pid bookkeeping,
/// and it cannot mistake a live endpoint for a dead one.
///
/// `older_than` is the floor that keeps this from racing daemon startup --
/// an endpoint that has been bound but has not yet reached `accept` refuses
/// a connection and is otherwise indistinguishable from an orphan.
#[cfg(unix)]
pub fn sweep_orphaned_endpoints_in(dir: &Path, older_than: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(ENDPOINT_PREFIX) {
            // Not ours.
            continue;
        }
        let path = entry.path();
        let recent = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(std::io::Error::other))
            .map(|age| age < older_than)
            .unwrap_or(true); // unreadable age -- leave it alone
        if recent {
            continue;
        }
        // Connect to *this file*, not to the name. `ReadEndpoint::connect`
        // resolves through the namespace, which ignores `dir` entirely --
        // so it would answer for a different socket than the one about to
        // be deleted. Caught by the test that sweeps a tempdir: both
        // sockets came back dead and the live one was removed.
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            continue; // somebody is listening
        }
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// A bound read-service listener.
///
/// A newtype rather than a re-export of `interprocess`'s `Listener`, so this
/// module stays the only place the transport crate's API shape appears.
/// `interprocess` is trait-based (`accept` comes from its `Listener` trait), and
/// re-exporting the raw type would push that import onto every caller and
/// spread the dependency across the crate.
pub struct ReadListener {
    inner: interprocess::local_socket::Listener,
}

impl ReadListener {
    /// Block until one client connects.
    pub fn accept(&self) -> io::Result<ReadStream> {
        Ok(ReadStream {
            inner: self.inner.accept()?,
        })
    }

    /// Wait at most `timeout` for a client: `Ok(None)` if none came.
    ///
    /// What lets the accept loop see its stop flag without being woken by a
    /// connection. `ReadService` used to stop a blocking `accept` by
    /// connecting to its own endpoint, which cannot work once the socket
    /// file is gone (#187) -- the join then waits forever, and the listener
    /// is never dropped, so a replacement could not be bound safely either.
    ///
    /// Windows falls back to a blocking `accept`: a named pipe cannot be
    /// removed from under its server, so the self-connect still reaches it.
    pub fn accept_timeout(&self, timeout: std::time::Duration) -> io::Result<Option<ReadStream>> {
        #[cfg(unix)]
        {
            use std::os::fd::{AsFd as _, AsRawFd as _};
            let interprocess::local_socket::Listener::UdSocket(listener) = &self.inner;
            let mut pollfd = libc::pollfd {
                fd: listener.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let millis = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
            // SAFETY: one valid pollfd, borrowed from a listener that
            // outlives the call.
            match unsafe { libc::poll(&mut pollfd, 1, millis) } {
                0 => return Ok(None),
                n if n < 0 => {
                    let err = io::Error::last_os_error();
                    return match err.kind() {
                        io::ErrorKind::Interrupted => Ok(None),
                        _ => Err(err),
                    };
                }
                _ => {}
            }
        }
        #[cfg(not(unix))]
        let _ = timeout;
        self.accept().map(Some)
    }
}

/// One client connection to the read service. `Read + Write`, so the
/// framing in `read_protocol` works over it without knowing the transport.
pub struct ReadStream {
    inner: interprocess::local_socket::Stream,
}

impl Read for ReadStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

/// Ends a parked lease from another thread: `shutdown(2)` wakes the lease
/// thread's blocking read with EOF and delivers EOF to the client. It never
/// closes the fd -- the lease thread still owns it -- so callers must stop
/// using a handle before its stream is dropped (see `LeaseBook`).
#[cfg(unix)]
pub(crate) struct LeaseShutdown(std::os::fd::RawFd);

#[cfg(unix)]
impl LeaseShutdown {
    pub(crate) fn shutdown(&self) {
        // SAFETY: the fd is open -- `LeaseBook` removes this handle, under
        // its lock, before the owning stream is dropped.
        unsafe {
            libc::shutdown(self.0, libc::SHUT_RDWR);
        }
    }
}

impl ReadStream {
    /// `None` where there is no socket fd (Windows named pipes): a daemon's
    /// exit still closes those, and in-process shutdown there simply leaves
    /// the lease until its client goes away.
    #[cfg(unix)]
    pub(crate) fn lease_shutdown(&self) -> Option<LeaseShutdown> {
        use std::os::fd::{AsFd as _, AsRawFd as _};
        let interprocess::local_socket::Stream::UdSocket(s) = &self.inner;
        Some(LeaseShutdown(s.as_fd().as_raw_fd()))
    }
}

impl Write for ReadStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The endpoint name must not grow with the project path. macOS caps
    /// `sun_path` at ~104 bytes, and a socket under `.infigraph/` for a
    /// nested worktree already measures 94 -- deep paths are routine here
    /// (`scratchpad/wt-*`), and overflowing produces an obscure bind error.
    #[test]
    fn endpoint_name_is_bounded_regardless_of_project_path_depth() {
        let deep = std::path::PathBuf::from("/Users/someone/GitHub.nosync/active/rust/infigraph")
            .join("scratchpad/wt-a-very-long-worktree-name/nested/deeper/deeper-still");
        let name = ReadEndpoint::for_root(&deep).as_name();
        assert!(
            name.len() <= 80,
            "endpoint name must stay well under the ~104-byte sun_path cap, got {} bytes: {name}",
            name.len()
        );
    }

    /// A daemon that dies without running `Drop` leaves its socket file
    /// behind, and the next daemon for that root then cannot bind --
    /// `Address already in use` -- so every routed read for that project
    /// fails with "no daemon read service is listening" until someone
    /// deletes the file by hand.
    ///
    /// This is not hypothetical. sittir's daemon logged
    /// `graceful shutdown exceeded its budget -- hard exit` and the
    /// replacement logged `could not bind the read service: Address already
    /// in use`; 120 orphaned `infigraph-read-*` sockets had accumulated in
    /// `/tmp` since the routed-read work landed, every one of them held by
    /// no process at all.
    ///
    /// `reclaim_name` (on by default) does not cover this: it unlinks the
    /// socket when the listener is *dropped*, which is exactly what a hard
    /// exit skips.
    // Only where `GenericNamespaced` is a socket *file*. On Linux it is the
    // abstract namespace: there is no file to leave behind or unlink, and a
    // dead process releases the name by itself, so the hazard these pin does
    // not exist there -- while `mem::forget` in this same process keeps the
    // name bound, and the second bind rightly fails with `AddrInUse`.
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    #[test]
    fn bind_reclaims_a_socket_left_behind_by_a_hard_exit() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = ReadEndpoint::for_root(root.path());

        let first = endpoint.bind().expect("first bind");
        // Leak it: `Drop` never runs on a hard exit, so the socket file
        // outlives the process that made it.
        std::mem::forget(first);

        let second = endpoint.bind();
        assert!(
            second.is_ok(),
            "a stale socket from a dead daemon must not lock the next one out \
             of its own endpoint: {:?}",
            second.err()
        );
    }

    /// The suspenders half. `bind`'s `try_overwrite` rescues the *next*
    /// daemon, but a client that does not retry still sees a refused
    /// connection until then, and an endpoint nobody binds again just
    /// leaves litter -- 120 such sockets had piled up. Neither mechanism
    /// subsumes the other: `SIGKILL` runs no code, so only `bind` covers
    /// that, while only this keeps the namespace clean for endpoints no
    /// daemon returns to.
    // Only where `GenericNamespaced` is a socket *file*. On Linux it is the
    // abstract namespace: there is no file to leave behind or unlink, and a
    // dead process releases the name by itself, so the hazard these pin does
    // not exist there -- while `mem::forget` in this same process keeps the
    // name bound, and the second bind rightly fails with `AddrInUse`.
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    #[test]
    fn unlink_removes_the_socket_a_hard_exiting_daemon_would_leave() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = ReadEndpoint::for_root(root.path());

        let listener = endpoint.bind().expect("bind");
        std::mem::forget(listener); // as a hard exit leaves it

        let before: Vec<_> = ReadEndpoint::namespace_dirs()
            .into_iter()
            .map(|d| d.join(endpoint.as_name()))
            .filter(|p| p.exists())
            .collect();
        assert!(
            !before.is_empty(),
            "bind must leave a socket somewhere this can find it, or unlink \
             is looking in the wrong place: {:?}",
            ReadEndpoint::namespace_dirs()
        );

        endpoint.unlink();

        for p in before {
            assert!(!p.exists(), "unlink must remove {}", p.display());
        }
    }

    /// #162: `unlink` and `cmd_kill`'s sweep only cover endpoints somebody
    /// still knows about -- a root in the registry, or the daemon's own on
    /// its way out. A daemon for an *unregistered* root (overwhelmingly a
    /// test tempdir) is covered by neither, so its socket outlives it
    /// forever. 120 had accumulated before this was noticed, and ten more
    /// appeared within half an hour of clearing them.
    ///
    /// A live listener accepts a connection; an orphan refuses it. That is
    /// the distinction, and it needs no external tooling to make.
    #[cfg(unix)]
    #[test]
    fn sweep_removes_an_orphaned_socket_and_spares_a_live_one() {
        let dir = tempfile::tempdir().unwrap();

        // `std`'s UnixListener deliberately does NOT unlink on drop, which
        // is exactly the corpse a hard-exiting daemon leaves.
        let orphan = dir.path().join("infigraph-read-0000000000000001");
        drop(std::os::unix::net::UnixListener::bind(&orphan).unwrap());
        assert!(
            orphan.exists(),
            "test setup: std must leave the socket file"
        );

        let live = dir.path().join("infigraph-read-0000000000000002");
        let _listener = std::os::unix::net::UnixListener::bind(&live).unwrap();

        // Something that merely shares the directory must be left alone.
        let bystander = dir.path().join("something-else.sock");
        std::fs::write(&bystander, b"not ours").unwrap();

        let removed = sweep_orphaned_endpoints_in(dir.path(), std::time::Duration::ZERO);

        assert_eq!(removed, 1, "exactly the orphan");
        assert!(!orphan.exists(), "the orphan must go");
        assert!(
            live.exists(),
            "a socket someone is still listening on must not"
        );
        assert!(bystander.exists(), "and nothing that isn't ours");
    }

    /// The age floor exists to avoid racing a daemon that has bound its
    /// endpoint but not yet reached `accept` -- it would refuse a
    /// connection and look exactly like an orphan.
    #[cfg(unix)]
    #[test]
    fn sweep_leaves_a_socket_younger_than_the_floor_alone() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("infigraph-read-0000000000000003");
        drop(std::os::unix::net::UnixListener::bind(&fresh).unwrap());

        let removed = sweep_orphaned_endpoints_in(dir.path(), std::time::Duration::from_secs(3600));

        assert_eq!(removed, 0);
        assert!(
            fresh.exists(),
            "a just-bound endpoint must survive the sweep, or startup races itself"
        );
    }

    /// Two different roots must not collide onto one endpoint.
    #[test]
    fn distinct_roots_get_distinct_endpoints() {
        let a = ReadEndpoint::for_root(std::path::Path::new("/tmp/alpha")).as_name();
        let b = ReadEndpoint::for_root(std::path::Path::new("/tmp/beta")).as_name();
        assert_ne!(a, b);
    }

    /// The same root must resolve to the same endpoint across processes.
    #[test]
    fn the_same_root_is_stable() {
        let p = std::path::Path::new("/tmp/alpha");
        assert_eq!(
            ReadEndpoint::for_root(p).as_name(),
            ReadEndpoint::for_root(p).as_name()
        );
    }
}
