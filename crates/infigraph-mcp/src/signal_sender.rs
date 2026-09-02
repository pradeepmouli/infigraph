//! Who sent us the termination signal (#123).
//!
//! `ctrlc` runs a closure on SIGTERM/SIGINT but drops the one fact that
//! tells a harness restart apart from `infigraph kill`, a takeover by
//! another instance, or a stray external signal: the sender. POSIX delivers
//! it in `siginfo_t.si_pid`, but only to a handler installed with
//! `SA_SIGINFO`, which `ctrlc` does not use.
//!
//! [`install`] therefore goes in *after* `ctrlc::set_handler`: it records
//! the action `ctrlc` installed for each signal, replaces it with an
//! `SA_SIGINFO` handler that stashes `si_pid` in an atomic (the only work a
//! signal context can do safely), and chains to `ctrlc`'s handler so its
//! self-pipe machinery -- and the closure -- still run. The closure then
//! reads [`describe`] to name the sender in its log line. `si_pid` is 0 for
//! senders that don't set it (the kernel, e.g. the OOM killer), in which
//! case the log line is unchanged.
//!
//! Windows has no sender identity for console events; everything here is a
//! no-op there.

use std::sync::atomic::{AtomicI32, Ordering};

/// `si_pid` of the last SIGTERM/SIGINT delivered, 0 if none or unknown.
static SENDER_PID: AtomicI32 = AtomicI32::new(0);

/// The pid that sent the termination signal, if a signal has arrived and
/// the sender was a process.
pub fn sender() -> Option<u32> {
    match SENDER_PID.load(Ordering::Acquire) {
        0 => None,
        pid => u32::try_from(pid).ok(),
    }
}

/// A clause for the shutdown log line: `" (signal from PID 123, infigraph)"`
/// when the sender is known (the name is best-effort -- a short-lived
/// `kill` has usually exited by the time the closure runs), otherwise "".
pub fn describe() -> String {
    let Some(pid) = sender() else {
        return String::new();
    };
    match infigraph_core::ps::process_name(pid) {
        Some(name) => format!(" (signal from PID {pid}, {name})"),
        None => format!(" (signal from PID {pid})"),
    }
}

/// Install the sender-capturing handlers. Call once, immediately after
/// `ctrlc::set_handler`; calling it before would chain to the default
/// action instead of `ctrlc`'s handler.
pub fn install() {
    imp::install();
}

#[cfg(unix)]
mod imp {
    use super::SENDER_PID;
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

    const SIGNALS: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGINT];

    /// The `sa_sigaction`/`sa_flags` that were installed (by `ctrlc`) when
    /// we took over, per signal in `SIGNALS` order. Atomics rather than a
    /// lock so the handler can read them from signal context.
    pub(super) static PREVIOUS_ACTION: [AtomicUsize; 2] =
        [AtomicUsize::new(0), AtomicUsize::new(0)];
    pub(super) static PREVIOUS_FLAGS: [AtomicI32; 2] = [AtomicI32::new(0), AtomicI32::new(0)];

    pub(super) fn slot(sig: libc::c_int) -> usize {
        usize::from(sig != libc::SIGTERM)
    }

    #[cfg(target_os = "linux")]
    unsafe fn sender_of(info: *const libc::siginfo_t) -> libc::pid_t {
        (*info).si_pid()
    }

    #[cfg(not(target_os = "linux"))]
    unsafe fn sender_of(info: *const libc::siginfo_t) -> libc::pid_t {
        (*info).si_pid
    }

    pub(super) extern "C" fn handler(
        sig: libc::c_int,
        info: *mut libc::siginfo_t,
        ctx: *mut libc::c_void,
    ) {
        // Async-signal-safe only from here on: atomics and tail-calling
        // the previous handler. No allocation, no logging.
        if !info.is_null() {
            SENDER_PID.store(unsafe { sender_of(info) }, Ordering::Release);
        }
        let idx = slot(sig);
        let previous = PREVIOUS_ACTION[idx].load(Ordering::Acquire);
        let flags = PREVIOUS_FLAGS[idx].load(Ordering::Acquire);
        match previous {
            libc::SIG_IGN => {}
            libc::SIG_DFL => {
                // Nothing to chain to: put the default action back and
                // re-deliver, so the process still dies of the signal.
                let mut default: libc::sigaction = unsafe { std::mem::zeroed() };
                default.sa_sigaction = libc::SIG_DFL;
                unsafe {
                    libc::sigaction(sig, &default, std::ptr::null_mut());
                    libc::raise(sig);
                }
            }
            f if flags & libc::SA_SIGINFO != 0 => {
                let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                    unsafe { std::mem::transmute(f) };
                f(sig, info, ctx);
            }
            f => {
                let f: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(f) };
                f(sig);
            }
        }
    }

    pub fn install() {
        for sig in SIGNALS {
            let idx = slot(sig);
            // Record what is installed now *before* replacing it, so a
            // signal that lands mid-install still chains correctly.
            let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
            if unsafe { libc::sigaction(sig, std::ptr::null(), &mut current) } != 0 {
                continue;
            }
            PREVIOUS_ACTION[idx].store(current.sa_sigaction, Ordering::Release);
            PREVIOUS_FLAGS[idx].store(current.sa_flags, Ordering::Release);

            let mut ours: libc::sigaction = unsafe { std::mem::zeroed() };
            ours.sa_sigaction = handler as extern "C" fn(_, _, _) as usize;
            ours.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            unsafe {
                libc::sigemptyset(&mut ours.sa_mask);
                libc::sigaction(sig, &ours, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    pub fn install() {}
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// The handler chain end to end, in-process: a `ctrlc`-style previous
    /// handler (plain, no SA_SIGINFO) must still run, and the sender -- us,
    /// via `kill(getpid())` -- must be recorded. SIGUSR1 stands in for
    /// SIGTERM so the test binary is not asked to terminate itself; the
    /// chaining logic is signal-agnostic.
    #[test]
    fn records_the_sender_and_chains_to_the_previous_handler() {
        use std::sync::atomic::AtomicBool;
        static PREVIOUS_RAN: AtomicBool = AtomicBool::new(false);
        extern "C" fn previous(_sig: libc::c_int) {
            PREVIOUS_RAN.store(true, Ordering::SeqCst);
        }

        // Stand in for ctrlc: a plain handler on SIGUSR1.
        let mut plain: libc::sigaction = unsafe { std::mem::zeroed() };
        plain.sa_sigaction = previous as extern "C" fn(libc::c_int) as usize;
        unsafe {
            libc::sigemptyset(&mut plain.sa_mask);
            libc::sigaction(libc::SIGUSR1, &plain, std::ptr::null_mut());
        }

        // Install ours over it, the way `install()` does for SIGTERM/SIGINT.
        let idx = imp::slot(libc::SIGUSR1);
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        unsafe { libc::sigaction(libc::SIGUSR1, std::ptr::null(), &mut current) };
        imp::PREVIOUS_ACTION[idx].store(current.sa_sigaction, Ordering::Release);
        imp::PREVIOUS_FLAGS[idx].store(current.sa_flags, Ordering::Release);
        let mut ours: libc::sigaction = unsafe { std::mem::zeroed() };
        ours.sa_sigaction = imp::handler as extern "C" fn(_, _, _) as usize;
        ours.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        unsafe {
            libc::sigemptyset(&mut ours.sa_mask);
            libc::sigaction(libc::SIGUSR1, &ours, std::ptr::null_mut());
        }

        // SIGUSR1 shares SIGINT's slot (`slot` is "SIGTERM or not"), which
        // is fine here: nothing else in this test binary touches SIGINT.
        unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while sender().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert_eq!(
            sender(),
            Some(std::process::id()),
            "si_pid must be our own pid"
        );
        assert!(
            PREVIOUS_RAN.load(Ordering::SeqCst),
            "the previous handler must still run"
        );
    }
}
