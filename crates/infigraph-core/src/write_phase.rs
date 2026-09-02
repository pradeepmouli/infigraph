//! Which lbug write the process is in the middle of, readable from a signal
//! handler (#132).
//!
//! An uncaught C++ exception inside lbug calls `std::terminate`, which
//! aborts the process before any Rust `Result`, panic hook, or guard runs.
//! The daemon's only trace of the 2026-08-31 sittir crash was libc++abi's
//! one line naming the exception type -- nothing said what the daemon was
//! doing. Every write path now brackets its lbug calls with [`enter`], and
//! [`install_abort_breadcrumb`] prints the current phase from the SIGABRT
//! handler, so the next abort names the operation that triggered it.
//!
//! Everything here is async-signal-safe by construction: the phase is a
//! pointer to a `&'static str` (a string literal's reference is promoted
//! to a static, so `enter(&"...", n)` is all callers write), the count is
//! one atomic, and the abort line is formatted into a stack buffer.
//!
//! One global slot, not per-thread: reading a `thread_local!` from a signal
//! handler is not async-signal-safe, and the write paths are serialized by
//! the graph's write lock anyway, so at most one write phase is active in
//! practice. A guard restores the phase it replaced on drop, so nesting
//! (a COPY inside a batch) and an interleaving reader thread degrade to a
//! possibly-stale label, never a crash.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

static PHASE: AtomicPtr<&'static str> = AtomicPtr::new(std::ptr::null_mut());
static COUNT: AtomicU64 = AtomicU64::new(0);

/// Upper bound on the abort line, including the trailing newline.
pub const ABORT_LINE_CAP: usize = 256;

/// Restores the phase it replaced when dropped.
pub struct PhaseGuard {
    prev: *mut &'static str,
    prev_count: u64,
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        COUNT.store(self.prev_count, Ordering::SeqCst);
        PHASE.store(self.prev, Ordering::SeqCst);
    }
}

/// Marks `phase` as the in-flight write until the returned guard drops.
/// `count` is whatever size the phase has on hand (files in the batch,
/// rows in the COPY, retry attempt), 0 when nothing fits.
pub fn enter(phase: &'static &'static str, count: u64) -> PhaseGuard {
    let prev = PHASE.load(Ordering::SeqCst);
    let prev_count = COUNT.load(Ordering::SeqCst);
    PHASE.store(
        phase as *const &'static str as *mut &'static str,
        Ordering::SeqCst,
    );
    COUNT.store(count, Ordering::SeqCst);
    PhaseGuard { prev, prev_count }
}

/// The in-flight write phase and its count, `None` outside any phase.
pub fn current() -> Option<(&'static str, u64)> {
    let ptr = PHASE.load(Ordering::SeqCst);
    if ptr.is_null() {
        return None;
    }
    // SAFETY: only ever set from `enter`, which stores the address of a
    // `&'static &'static str` -- the referent lives for the whole program.
    let phase: &'static str = unsafe { *ptr };
    Some((phase, COUNT.load(Ordering::SeqCst)))
}

/// Formats the breadcrumb line into `buf` without allocating; returns the
/// number of bytes written. Truncates rather than overflows.
pub fn format_abort_line(
    buf: &mut [u8; ABORT_LINE_CAP],
    phase: Option<(&str, u64)>,
    pid: u32,
) -> usize {
    let mut w = Cursor { buf, len: 0 };
    w.push(b"[daemon] SIGABRT pid=");
    w.push_u64(pid as u64);
    w.push(b" (uncaught C++ exception or abort)");
    match phase {
        Some((name, count)) => {
            w.push(b" during write phase: ");
            w.push(name.as_bytes());
            w.push(b" (n=");
            w.push_u64(count);
            w.push(b")");
        }
        None => w.push(b" outside any write phase"),
    }
    w.push(b"\n");
    w.len
}

struct Cursor<'a> {
    buf: &'a mut [u8; ABORT_LINE_CAP],
    len: usize,
}

impl Cursor<'_> {
    fn push(&mut self, bytes: &[u8]) {
        let room = ABORT_LINE_CAP - self.len;
        let n = bytes.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
    }

    fn push_u64(&mut self, mut v: u64) {
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.push(&digits[i..]);
    }
}

/// Installs a SIGABRT handler that writes the breadcrumb line to stderr
/// (the daemon's `daemon.log`) and lets the abort proceed. Call once, from
/// the daemon's main, next to its panic hook.
#[cfg(unix)]
pub fn install_abort_breadcrumb() {
    extern "C" fn on_abort(_sig: libc::c_int) {
        let mut buf = [0u8; ABORT_LINE_CAP];
        let n = format_abort_line(&mut buf, current(), std::process::id());
        // SAFETY: write(2) and raise(2) are async-signal-safe; the handler
        // was installed with SA_RESETHAND, so the re-raised SIGABRT gets the
        // default action once this handler returns and the process still
        // dies by SIGABRT (core dump semantics untouched).
        unsafe {
            libc::write(libc::STDERR_FILENO, buf.as_ptr().cast(), n);
            libc::raise(libc::SIGABRT);
        }
    }
    // SAFETY: plain sigaction(2) install with a zeroed struct; the handler
    // above touches only atomics, a stack buffer, and async-signal-safe
    // libc calls.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        let handler: extern "C" fn(libc::c_int) = on_abort;
        sa.sa_sigaction = handler as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESETHAND;
        libc::sigaction(libc::SIGABRT, &sa, std::ptr::null_mut());
    }
}

/// No SIGABRT on Windows; the phase tracking still works, only the
/// handler is absent.
#[cfg(not(unix))]
pub fn install_abort_breadcrumb() {}
