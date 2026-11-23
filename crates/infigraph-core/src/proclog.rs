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
/// or socket it gave an MCP server made every one of them a panic).
///
/// The real stderr is duplicated (close-on-exec, so no child keeps the client's descriptor open past the
/// server), fd 2 becomes the write end of a new pipe, and one thread forwards what arrives to the saved
/// descriptor without ever waiting for it. The saved descriptor is whatever the client gave: a pipe or a unix
/// socket in practice. It is written only after `poll(POLLOUT)` says it can take a piece, in pieces of at most
/// `PIPE_BUF` bytes, which cannot block a pipe (POSIX) or a unix socket (`POLLOUT` promises `SO_SNDLOWAT`
/// bytes); `send(MSG_DONTWAIT)` was measured to block on a full macOS socket, so it is not used. One lock around poll and write keeps a second writer from using the
/// space the first just saw. What cannot go is dropped and counted (`dropped_stderr_bytes`). `O_NONBLOCK` is
/// never set on the saved descriptor, since that open file description is shared with the client. An exit
/// drains the pipe first (`atexit`, bounded by `DRAIN_BOUND`), as a courtesy rather than a guarantee. Call it
/// first thing in `main`, before any thread starts; fd 1 is never touched. Returns the reason when a step
/// failed, and then the process carries on unshielded.
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

#[cfg(unix)]
mod shield {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// POSIX's minimum `PIPE_BUF` (macOS has exactly this, Linux 4096): a write of at most this many bytes to
    /// a pipe that polled writable never blocks and is never split.
    const PIPE_BUF: usize = 512;
    /// How long an exit waits for the forwarder to hand over what the pipe holds.
    const DRAIN_BOUND: Duration = Duration::from_millis(300);
    /// How often the forwarder looks at its read end even when nothing arrives.
    const FORWARDER_TICK_MS: libc::c_int = 1000;
    /// How many times a writer re-tries the write lock before it drops its bytes.
    const LOCK_SPINS: u32 = 20_000;

    /// The saved real stderr; -1 until the shield is up.
    static SAVED: AtomicI32 = AtomicI32::new(-1);
    static READ_END: AtomicI32 = AtomicI32::new(-1);
    static DROPPED: AtomicU64 = AtomicU64::new(0);
    /// The forwarder holds bytes it has read and not yet delivered or dropped.
    static BUSY: AtomicBool = AtomicBool::new(false);
    /// Held while a writer polls and writes the saved descriptor. A spin lock because one writer is a signal
    /// handler, which may not take a mutex.
    static WRITING: AtomicBool = AtomicBool::new(false);

    pub(super) fn is_up() -> bool {
        SAVED.load(Ordering::Acquire) >= 0
    }

    pub(super) fn dropped() -> u64 {
        DROPPED.load(Ordering::Relaxed)
    }

    fn last_errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// One piece to `fd` without waiting: bytes written, 0 when it cannot take any now, -1 on a failure that
    /// should be retried (EINTR).
    ///
    /// `poll(POLLOUT)` first, then at most `PIPE_BUF` bytes. For a pipe POSIX guarantees such a write cannot
    /// block. For a unix stream socket it holds because `POLLOUT` is only reported with at least `SO_SNDLOWAT`
    /// bytes of room (2048 on macOS, a quarter of the buffer's room on Linux), which `install` raises to
    /// `PIPE_BUF` when a client lowered it. `send(MSG_DONTWAIT)` would be the obvious alternative, and does not
    /// work: measured on macOS, `send` to a full unix socket blocks despite the flag.
    fn write_piece(fd: i32, bytes: &[u8]) -> isize {
        let n = bytes.len().min(PIPE_BUF);
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: `p` is a valid pollfd and the count is 1.
        let ready = unsafe { libc::poll(&mut p, 1, 0) };
        if ready < 0 {
            return if last_errno() == libc::EINTR { -1 } else { 0 };
        }
        if ready == 0 || p.revents & libc::POLLOUT == 0 {
            return 0;
        }
        // SAFETY: `bytes` holds at least `n` readable bytes.
        let w = unsafe { libc::write(fd, bytes.as_ptr().cast(), n) };
        if w < 0 {
            return if last_errno() == libc::EINTR { -1 } else { 0 };
        }
        w
    }

    /// Write `bytes` to `fd` without ever waiting, one writer at a time; whatever cannot go is dropped and
    /// its length returned. Async-signal-safe: atomics, `send`, `poll` and `write` only.
    pub(super) fn write_nowait(fd: i32, mut bytes: &[u8]) -> usize {
        let mut spins = 0u32;
        while WRITING
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;
            if spins > LOCK_SPINS {
                return bytes.len();
            }
            std::hint::spin_loop();
        }
        while !bytes.is_empty() {
            match write_piece(fd, bytes) {
                -1 => continue,
                0 => break,
                n => bytes = &bytes[n as usize..],
            }
        }
        WRITING.store(false, Ordering::Release);
        bytes.len()
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

    /// `SIGPIPE` must be ignored for the whole process (Rust's runtime does that for every binary): a `send`
    /// to a closed socket would otherwise kill it, on platforms without `MSG_NOSIGNAL`.
    fn sigpipe_is_ignored() -> bool {
        // SAFETY: a read-only sigaction query into a zeroed struct.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut old) == 0
                && old.sa_sigaction == libc::SIG_IGN
        }
    }

    /// A socket client may have set `SO_SNDLOWAT` below `PIPE_BUF`, which would let `POLLOUT` promise less room
    /// than one piece. Raise it (on our end only; a pipe or file just fails the call).
    fn raise_send_low_water(fd: i32) {
        let mut have: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: getsockopt/setsockopt on a descriptor this process owns, with correctly sized buffers.
        unsafe {
            if libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDLOWAT,
                (&mut have as *mut libc::c_int).cast(),
                &mut len,
            ) == 0
                && (have as usize) < PIPE_BUF
            {
                let want = PIPE_BUF as libc::c_int;
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDLOWAT,
                    (&want as *const libc::c_int).cast(),
                    len,
                );
            }
        }
    }

    pub(super) fn install() -> Result<(), String> {
        if is_up() {
            return Ok(());
        }
        if !sigpipe_is_ignored() {
            return Err(
                "SIGPIPE is not ignored, so a write to a closed socket could kill the process"
                    .into(),
            );
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
            raise_send_low_water(saved);
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
                // The forwarder is already running on `r`; it ends when `w` closes.
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

    /// Wait up to `bound` for what the pipe holds to reach the real stderr (or be dropped).
    fn drain(bound: Duration) {
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

    /// The forwarder cannot go on (its read end failed or ended): fd 2 would be a pipe nobody drains, and once
    /// it filled every `eprintln!` in the process would block. Point fd 2 at /dev/null first, then let go of
    /// the read end.
    fn give_up(read_end: i32) {
        if let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
            use std::os::fd::AsRawFd as _;
            // SAFETY: both descriptors are valid; `null` closes itself when it drops.
            unsafe { libc::dup2(null.as_raw_fd(), 2) };
        }
        READ_END.store(-1, Ordering::Release);
        BUSY.store(false, Ordering::Release);
        // SAFETY: this thread owns `read_end`.
        unsafe { libc::close(read_end) };
    }

    fn forward(read_end: i32, saved: i32) {
        let mut buf = [0u8; 4096];
        loop {
            let mut p = libc::pollfd {
                fd: read_end,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `p` is a valid pollfd. A timeout, not -1: the descriptor number is looked up again on
            // every round, so a read end that was closed or replaced under a sleeping poll (which the kernel
            // does not wake) is noticed within a tick instead of stranding fd 2 for good.
            let ready = unsafe { libc::poll(&mut p, 1, FORWARDER_TICK_MS) };
            if ready < 0 {
                if last_errno() == libc::EINTR {
                    continue;
                }
                return give_up(read_end);
            }
            if ready == 0 {
                continue;
            }
            BUSY.store(true, Ordering::Release);
            // SAFETY: `buf` is a writable buffer of the length passed.
            let n = unsafe { libc::read(read_end, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 && last_errno() == libc::EINTR {
                BUSY.store(false, Ordering::Release);
                continue;
            }
            if n <= 0 {
                return give_up(read_end);
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
