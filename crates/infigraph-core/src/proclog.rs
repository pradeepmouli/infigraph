//! Shared "why did this process exit" logging primitive, used by both
//! `infigraph daemon` and `infigraph-mcp` so a future incident leaves an
//! actual diagnostic trail instead of nothing. Every voluntary exit path
//! (clean shutdown, a caught signal, a panic) can and should call this
//! before exiting -- the one exit reason that can never be logged this way
//! is an uncatchable kill (SIGKILL, or the OS's own resource-pressure
//! reaper), since no code runs for those at all.

use std::io::Write;
use std::path::Path;

/// Write one line to stderr and ignore the outcome. `eprintln!` PANICS when the write fails -- a
/// broken pipe, which is what stderr becomes once the client that launched an MCP server or a
/// daemon has closed its end -- and a logging call must never take its caller down with it.
///
/// With the stderr shield up (`shield_stderr`) the line goes straight to the saved real stderr and is dropped
/// when that cannot take it without waiting, so a panic hook never blocks on a client that stopped reading.
pub fn stderr_line(line: &str) {
    #[cfg(unix)]
    if shield::is_up() {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        shield::write_real(&bytes);
        return;
    }
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// Write `bytes` to the process's real stderr from a signal handler (async-signal-safe: no allocation, no lock,
/// only `poll(2)` and `write(2)`). With the shield up that is the saved descriptor, written without waiting,
/// because a line left in the shield's own pipe dies with the process; otherwise it is fd 2.
pub fn write_stderr_async_safe(bytes: &[u8]) {
    #[cfg(unix)]
    {
        shield::write_real(bytes);
    }
    #[cfg(not(unix))]
    {
        let _ = bytes;
    }
}

/// Put a pipe this process owns in front of its stderr, so nothing written to fd 2 can fail with a broken pipe
/// while the process lives (`eprintln!` panics on a failed write, and a client that closes its end of the pipe
/// it gave an MCP server made every one of them a panic).
///
/// The real stderr is duplicated (close-on-exec, so no child keeps the client's pipe open past the server),
/// fd 2 becomes the write end of a new pipe, and one thread forwards what arrives to the saved descriptor
/// without ever waiting for it: it writes only when `poll(POLLOUT)` says the descriptor can take up to
/// `PIPE_BUF` bytes at once, and counts what it has to drop (`dropped_bytes`). `O_NONBLOCK` is not set on the
/// saved descriptor, since that open file description is shared with the client. An exit drains the pipe first
/// (`atexit`, bounded by `DRAIN_BOUND`). Call it first thing in `main`, before any thread starts; fd 1 is never
/// touched. Returns the reason when a step failed, and then the process carries on unshielded.
pub fn shield_stderr() -> Result<(), String> {
    #[cfg(unix)]
    {
        shield::install()
    }
    #[cfg(not(unix))]
    {
        Ok(())
    }
}

/// The descriptor that is the process's real stderr: the saved one while the shield is up, else fd 2.
pub fn real_stderr_fd() -> i32 {
    #[cfg(unix)]
    {
        shield::real_fd()
    }
    #[cfg(not(unix))]
    {
        2
    }
}

/// Bytes the shield could not forward because the real stderr could not take them without waiting.
pub fn dropped_stderr_bytes() -> u64 {
    #[cfg(unix)]
    {
        shield::dropped()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Wait up to `bound` for everything written to fd 2 so far to reach the real stderr (or be dropped).
/// Runs by itself at exit once the shield is up; a no-op without it.
pub fn drain_stderr(bound: std::time::Duration) {
    #[cfg(unix)]
    shield::drain(bound);
    #[cfg(not(unix))]
    let _ = bound;
}

#[cfg(unix)]
mod shield {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// POSIX's minimum `PIPE_BUF` (macOS has exactly this, Linux 4096): a write of at most this many bytes to
    /// a pipe that polled writable never blocks and is never split.
    const PIPE_BUF: usize = 512;
    pub(super) const DRAIN_BOUND: Duration = Duration::from_millis(300);

    /// The saved real stderr; -1 until the shield is up.
    static SAVED: AtomicI32 = AtomicI32::new(-1);
    static READ_END: AtomicI32 = AtomicI32::new(-1);
    static DROPPED: AtomicU64 = AtomicU64::new(0);
    /// The forwarder holds bytes it has read and not yet delivered or dropped.
    static BUSY: AtomicBool = AtomicBool::new(false);

    pub(super) fn is_up() -> bool {
        SAVED.load(Ordering::Acquire) >= 0
    }

    pub(super) fn real_fd() -> i32 {
        let saved = SAVED.load(Ordering::Acquire);
        if saved >= 0 {
            saved
        } else {
            2
        }
    }

    pub(super) fn dropped() -> u64 {
        DROPPED.load(Ordering::Relaxed)
    }

    /// Write `bytes` to `fd` in `PIPE_BUF` pieces, each only when `fd` polls writable right now; whatever
    /// cannot go is dropped and its length returned. Async-signal-safe.
    pub(super) fn write_nowait(fd: i32, mut bytes: &[u8]) -> usize {
        while !bytes.is_empty() {
            let mut p = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: `p` is a valid pollfd and the count is 1.
            let ready = unsafe { libc::poll(&mut p, 1, 0) };
            if ready < 0 {
                if last_errno() == libc::EINTR {
                    continue;
                }
                return bytes.len();
            }
            if ready == 0 || p.revents & libc::POLLOUT == 0 {
                return bytes.len();
            }
            let n = bytes.len().min(PIPE_BUF);
            // SAFETY: `bytes` holds at least `n` readable bytes.
            let w = unsafe { libc::write(fd, bytes.as_ptr().cast(), n) };
            if w < 0 {
                if last_errno() == libc::EINTR {
                    continue;
                }
                return bytes.len();
            }
            bytes = &bytes[w as usize..];
        }
        0
    }

    fn last_errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// To the real stderr: the saved descriptor without waiting when the shield is up, else a plain write to fd 2.
    pub(super) fn write_real(bytes: &[u8]) {
        let saved = SAVED.load(Ordering::Acquire);
        if saved >= 0 {
            let dropped = write_nowait(saved, bytes);
            DROPPED.fetch_add(dropped as u64, Ordering::Relaxed);
        } else {
            // SAFETY: `bytes` is a valid buffer; a failed write is the caller's to ignore.
            unsafe { libc::write(2, bytes.as_ptr().cast(), bytes.len()) };
        }
    }

    fn set_cloexec(fd: i32) -> bool {
        // SAFETY: fcntl on a descriptor this function was just given.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            flags >= 0 && libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) >= 0
        }
    }

    pub(super) fn install() -> Result<(), String> {
        if is_up() {
            return Ok(());
        }
        // SAFETY: plain descriptor calls; every descriptor opened here is closed on every failure path.
        unsafe {
            if libc::fcntl(2, libc::F_GETFD) < 0 {
                // Nothing to protect: Rust treats a closed stderr as a write that succeeds.
                return Err("stderr is closed".into());
            }
            let saved = libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3);
            if saved < 0 {
                return Err(format!("dup stderr: {}", std::io::Error::last_os_error()));
            }
            let mut fds = [0i32; 2];
            if libc::pipe(fds.as_mut_ptr()) < 0 {
                let e = std::io::Error::last_os_error();
                libc::close(saved);
                return Err(format!("pipe: {e}"));
            }
            let [r, w] = fds;
            if !set_cloexec(r) || !set_cloexec(w) {
                libc::close(saved);
                libc::close(r);
                libc::close(w);
                return Err("fcntl(FD_CLOEXEC) on the shield's pipe failed".into());
            }
            READ_END.store(r, Ordering::Release);
            let spawned = std::thread::Builder::new()
                .name("stderr-shield".into())
                .stack_size(128 * 1024)
                .spawn(move || forward(r, saved));
            if let Err(e) = spawned {
                libc::close(saved);
                libc::close(r);
                libc::close(w);
                return Err(format!("forwarder thread: {e}"));
            }
            if libc::dup2(w, 2) < 0 {
                // The forwarder is already running on `r`; leave it (it ends when `w` closes) and stay unshielded.
                let e = std::io::Error::last_os_error();
                libc::close(w);
                libc::close(saved);
                return Err(format!("dup2 onto stderr: {e}"));
            }
            libc::close(w);
            SAVED.store(saved, Ordering::Release);
            libc::atexit(drain_at_exit);
        }
        Ok(())
    }

    extern "C" fn drain_at_exit() {
        drain(DRAIN_BOUND);
    }

    pub(super) fn drain(bound: Duration) {
        let r = READ_END.load(Ordering::Acquire);
        if r < 0 || !is_up() {
            return;
        }
        let deadline = Instant::now() + bound;
        loop {
            let mut pending: libc::c_int = 0;
            // SAFETY: FIONREAD writes one c_int.
            let ok = unsafe { libc::ioctl(r, libc::FIONREAD, &mut pending) } == 0;
            if (ok && pending == 0 && !BUSY.load(Ordering::Acquire)) || Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn forward(read_end: i32, saved: i32) {
        let mut buf = [0u8; 4096];
        loop {
            let mut p = libc::pollfd {
                fd: read_end,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `p` is a valid pollfd.
            let ready = unsafe { libc::poll(&mut p, 1, -1) };
            if ready < 0 {
                if last_errno() == libc::EINTR {
                    continue;
                }
                return;
            }
            BUSY.store(true, Ordering::Release);
            // SAFETY: `buf` is a writable buffer of the length passed.
            let n = unsafe { libc::read(read_end, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 && last_errno() == libc::EINTR {
                BUSY.store(false, Ordering::Release);
                continue;
            }
            if n <= 0 {
                BUSY.store(false, Ordering::Release);
                return;
            }
            let dropped = write_nowait(saved, &buf[..n as usize]);
            DROPPED.fetch_add(dropped as u64, Ordering::Relaxed);
            BUSY.store(false, Ordering::Release);
        }
    }
}

/// Append one timestamped `[epoch] LEVEL: msg` line to `path`, rotating it
/// first if it's grown past 10MB. Best-effort throughout: a logging
/// failure must never fail (or even meaningfully slow down) the caller's
/// own shutdown -- this is diagnostics, not a durability guarantee.
pub fn write_log_line(path: &Path, level: &str, msg: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{ts}] {level}: {msg}");
    stderr_line(&line);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    crate::logrotate::rotate_if_over(path, 10 * 1024 * 1024);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// `"<payload> at <file>:<line>:<col>"` for a panic, the same text every process's panic hook logs.
pub fn describe_panic(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    };
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "unknown".to_string());
    format!("{payload} at {location}")
}

/// Install a panic hook that hands `on_panic` the description and a forced backtrace of every
/// panic in this process. The one hook installer for `infigraph daemon` and `infigraph-mcp`.
///
/// A panic inside a panic hook aborts the process (SIGABRT) and `catch_unwind` cannot stop it
/// (std tracks "in hook" and aborts on the nested panic itself), so `on_panic` must not panic:
/// write through `write_log_line` and `stderr_line`, which ignore a failed write, never `eprintln!`.
pub fn install_panic_hook(on_panic: impl Fn(&str, &str) + Send + Sync + 'static) {
    std::panic::set_hook(Box::new(move |info| {
        let desc = describe_panic(info);
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        on_panic(&desc, &bt);
    }));
}

#[cfg(test)]
mod tests {
    use super::write_log_line;

    #[test]
    fn writes_a_timestamped_line_and_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("proc.log");
        write_log_line(&path, "INFO", "hello");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("] INFO: hello"), "got: {content}");
    }

    #[test]
    fn rotates_before_appending_once_over_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("proc.log");
        std::fs::write(&path, vec![0u8; 11 * 1024 * 1024]).unwrap();
        write_log_line(&path, "INFO", "after rotation");
        let rotated = tmp.path().join("proc.log.1");
        assert!(rotated.exists(), "oversized log must be rotated aside");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("after rotation"));
    }
}
