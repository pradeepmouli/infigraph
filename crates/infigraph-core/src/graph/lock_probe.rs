//! Non-leaking answer to "is this graph file locked, and by whom?".
//!
//! lbug takes its inter-process lock with `fcntl(F_SETLK)` on the database
//! file (`local_file_system.cpp`, `LocalFileSystem::openFile`), and on
//! failure throws **without closing the fd it just opened** -- so every
//! `Database::new` that loses to another process leaks one file descriptor
//! (observed on sittir: a daemon retrying every tick for a day accumulated
//! 7,600 fds on one graph file). Any wait-for-the-lock loop must therefore
//! not probe by re-opening the database. This module probes with
//! `fcntl(F_GETLK)` instead, which reports the conflicting holder's pid and
//! opens nothing inside lbug.
//!
//! The probe fd is kept open for the life of the process (one per distinct
//! graph path). That is deliberate, not a leak: POSIX releases *all* of a
//! process's fcntl locks on a file the moment *any* fd for that file is
//! closed, so closing a short-lived probe fd would silently drop this
//! process's own live lbug lock on the same graph.

use std::path::Path;

/// Which lbug open the caller is about to attempt: a write open takes
/// `F_WRLCK`, a read-only open takes `F_RDLCK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFor {
    Read,
    Write,
}

/// Outcome of a lock probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockProbe {
    /// No conflicting lock -- an lbug open of that kind would succeed now.
    Free,
    /// A conflicting lock is held; `Some(pid)` when the OS reports the
    /// holder (it does on macOS/Linux for fcntl locks).
    Locked(Option<u32>),
    /// This platform has no non-leaking probe (Windows uses `LockFileEx`);
    /// callers fall back to whatever they did before.
    Unsupported,
}

#[cfg(unix)]
pub fn probe_graph_lock(path: &Path, want: ProbeFor) -> LockProbe {
    use std::collections::HashMap;
    use std::fs::File;
    use std::os::unix::io::AsRawFd;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    static PROBES: OnceLock<Mutex<HashMap<PathBuf, File>>> = OnceLock::new();
    let probes = PROBES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut probes = probes.lock().unwrap_or_else(|p| p.into_inner());
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let file = match probes.entry(key) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => match File::open(path) {
            Ok(f) => v.insert(f),
            // A path that can't even be opened for reading can't be probed;
            // let the caller's real open produce the real error.
            Err(_) => return LockProbe::Free,
        },
    };

    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = match want {
        ProbeFor::Read => libc::F_RDLCK as libc::c_short,
        ProbeFor::Write => libc::F_WRLCK as libc::c_short,
    };
    fl.l_whence = libc::SEEK_SET as libc::c_short;
    fl.l_start = 0;
    fl.l_len = 0;
    // SAFETY: `fl` is a fully initialised `flock` and `file` is a valid fd.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut fl) };
    if rc != 0 {
        return LockProbe::Unsupported;
    }
    if fl.l_type == libc::F_UNLCK as libc::c_short {
        LockProbe::Free
    } else {
        LockProbe::Locked((fl.l_pid > 0).then_some(fl.l_pid as u32))
    }
}

#[cfg(not(unix))]
pub fn probe_graph_lock(_path: &Path, _want: ProbeFor) -> LockProbe {
    LockProbe::Unsupported
}

/// Human-readable "held by ..." fragment for a lock-contention error, so
/// the operator (or the daemon log) sees *which* process to look at
/// instead of a generic "another infigraph process".
pub fn describe_lock_holder(path: &Path, want: ProbeFor) -> String {
    match probe_graph_lock(path, want) {
        LockProbe::Locked(Some(pid)) => {
            let name = crate::ps::process_name(pid).unwrap_or_else(|| "unknown".into());
            format!("held by PID {pid} ({name})")
        }
        LockProbe::Locked(None) => "held by another process (pid not reported)".to_string(),
        LockProbe::Free => "no longer held (transient contention)".to_string(),
        LockProbe::Unsupported => "held by another process".to_string(),
    }
}
