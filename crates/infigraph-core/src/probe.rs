//! Out-of-process probe: "can this graph file be opened at all?"
//!
//! Answering that question in-process is not safe. Of the thirteen
//! quarantined images found on one machine, twelve open fine and one
//! (`infigraph-mcp` #1787359144, 13.6 MB) exits with SIGBUS and prints
//! nothing at all -- it does not return an `Err`, it kills the process.
//! `GraphStore::open`'s size preflight does not catch it: the file is a
//! plausible size and still faults on access.
//!
//! So the recovery path cannot simply try the open and see. A daemon that
//! probes a damaged image inline dies, restarts, probes again and dies
//! again -- a crash loop strictly worse than the wipe it was trying to
//! avoid. The probe therefore runs in a child process, where a fatal signal
//! is just a non-zero exit status.
//!
//! The child is this same executable, re-invoked with [`PROBE_ENV`] set.
//! Both binaries call [`run_if_probe_child`] first thing in `main`, the same
//! shape the `concurrent_reader_helper` test child already uses.

use std::path::{Path, PathBuf};

/// Set on the child to the graph path it should try to open. Its presence is
/// also what tells the child it is a probe rather than a normal run.
pub const PROBE_ENV: &str = "INFIGRAPH_PROBE_GRAPH_PATH";

/// Whether this executable honours [`PROBE_ENV`] -- i.e. whether re-invoking
/// it yields a probe rather than a second copy of whatever it normally does.
///
/// This is not paranoia; it is a fork bomb this code actually caused.
/// [`graph_opens`] re-invokes `current_exe()`. For `infigraph` and
/// `infigraph-mcp` that is a probe, because their `main` calls
/// [`run_if_probe_child`] first thing. For a **test binary**, `current_exe()`
/// is libtest's harness, which has never heard of this variable -- so the
/// child re-ran the entire test suite, which reached this same recovery path,
/// which spawned another child. 299 processes and a load average over 300,
/// after first being misread as "the tests are just slow".
///
/// Defaulting to `false` also fails in the safe direction: an executable that
/// cannot probe reports "did not open", so recovery declines and the caller
/// keeps whatever behaviour it had before.
static PROBE_CAPABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Declare that this executable calls [`run_if_probe_child`] in `main`.
/// Without it, [`graph_opens`] refuses to spawn anything.
pub fn mark_probe_capable() {
    PROBE_CAPABLE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Call at the very top of `main`. When [`PROBE_ENV`] is set, attempts the
/// open and exits: 0 if the graph opened, 1 if it returned an error. A fatal
/// signal (SIGBUS/SIGSEGV on a damaged image) exits the child too, which the
/// parent reads as "did not open" all the same.
///
/// Returns `false` when this is an ordinary run and `main` should continue.
pub fn run_if_probe_child() -> bool {
    mark_probe_capable();
    let Some(path) = std::env::var_os(PROBE_ENV) else {
        return false;
    };
    // Opening is NOT enough, and assuming it was nearly shipped a bug: the
    // one genuinely damaged image found on this machine OPENS cleanly and
    // then faults with SIGBUS on the first query that touches node data. A
    // probe that only opened would have called that graph recoverable and
    // handed it straight back to the daemon to die on.
    let ok = crate::graph::GraphStore::open_read_only(Path::new(&path))
        .and_then(|store| {
            let conn = store.connection()?;
            // A count is not enough either: it can be answered from
            // metadata without ever touching row data, and the damaged image
            // returns one happily. Scan the rows and CONSUME them -- results
            // are lazy, so an un-iterated query proves nothing.
            let rows = conn
                .query("MATCH (s:Symbol) RETURN s.id, s.name, s.file")
                .map_err(|e| anyhow::anyhow!("probe query failed: {e}"))?;
            let mut seen = 0usize;
            for _ in rows {
                seen += 1;
            }
            let _ = seen;
            Ok(())
        })
        .is_ok();
    std::process::exit(if ok { 0 } else { 1 });
}

/// Whether `graph_path` opens read-only AND survives a query that reads
/// node data, decided in a child process so a damaged file cannot take this
/// one down.
///
/// A process that cannot run a probe at all gets [`Probe::Unavailable`], not
/// `Indeterminate`: no retry can ever decide, so "retry shortly" would be false.
pub fn graph_opens(graph_path: &Path) -> Probe {
    // Never re-invoke an executable that does not handle PROBE_ENV; a test
    // binary would re-run its whole suite. See PROBE_CAPABLE. Not being able
    // to ask is not an answer about the graph.
    if !PROBE_CAPABLE.load(std::sync::atomic::Ordering::Relaxed) {
        return Probe::Unavailable;
    }
    let Ok(exe) = std::env::current_exe() else {
        return Probe::Unavailable;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.env(PROBE_ENV, graph_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // A probe child must never inherit -- or default into -- a backend that
    // would send it somewhere other than this file. The whole question being
    // asked is "does THIS file open?", so it is pinned, not merely unset:
    // once local stops being the default (#159) an unset variable routes the
    // probe to a daemon and it reports on a socket instead.
    cmd.env(crate::BACKEND_ENV, crate::LOCAL_BACKEND);
    run_probe_command(cmd, PROBE_TIMEOUT)
}

/// What a probe of one graph file established.
///
/// Three answers, because only one of them is evidence about the graph. A
/// probe that could not start, was killed from outside (the kernel's memory
/// pressure kill, a user's `kill`), or ran out of time never answered
/// anything: quarantining on it would discard a healthy graph over a
/// transient condition. Only the probe's own verdict, or a fault raised by
/// the image itself, says the graph does not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The child opened and scanned the graph.
    Opens,
    /// The child reported it does not open, or the image faulted under it.
    DoesNotOpen,
    /// Nothing was learned: the probe did not start, was killed from outside,
    /// or ran past [`PROBE_TIMEOUT`]. The caller must not quarantine.
    Indeterminate,
    /// This process cannot run a probe at all: it never declared itself
    /// probe-capable ([`run_if_probe_child`]), or it cannot name its own
    /// executable. No retry can change that, so it is not "could not be
    /// checked just now". Every caller decides exactly as it did before probes
    /// could be undecided, which is as [`Probe::DoesNotOpen`].
    Unavailable,
}

/// How long a probe may run before it is stopped. A probe opens the graph and
/// scans every symbol, so under swap it can take far longer than an ordinary
/// open; the budget is generous, and running out of it is never "does not open".
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Runs a prepared probe command, waits at most `timeout`, and classifies how
/// it ended. A timed-out child is killed and reaped before this returns.
pub fn run_probe_command(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> Probe {
    use std::process::Stdio;
    use std::time::Instant;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return Probe::Indeterminate;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return classify_probe_exit(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Probe::Indeterminate;
            }
        }
    }
}

fn classify_probe_exit(status: std::process::ExitStatus) -> Probe {
    if status.success() {
        return Probe::Opens;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        match status.signal() {
            // The child answered: it ran and exited non-zero.
            None => Probe::DoesNotOpen,
            // A fault in the image (SIGBUS on a damaged file, lbug's abort),
            // the evidence this probe exists to find.
            Some(libc::SIGBUS | libc::SIGSEGV | libc::SIGILL | libc::SIGABRT | libc::SIGFPE) => {
                Probe::DoesNotOpen
            }
            // Killed by something else: nothing was decided.
            Some(_) => Probe::Indeterminate,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        Probe::DoesNotOpen
    }
}

/// Where a torn WAL is filed when the base image beneath it turns out to be
/// fine. Kept, not deleted: it is the evidence for why a recovery happened.
pub fn torn_wal_path(infigraph_dir: &Path, graph_name: &str, suffix: &str, ts: u64) -> PathBuf {
    infigraph_dir.join(format!("{graph_name}.torn-wal.{ts}{suffix}"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    /// A probe that cannot even start says nothing about the graph: it must
    /// never read as "does not open", which would quarantine a healthy graph.
    #[test]
    fn a_probe_that_cannot_start_is_indeterminate() {
        let command = Command::new("/nonexistent/infigraph-probe-binary");
        assert_eq!(
            run_probe_command(command, Duration::from_secs(5)),
            Probe::Indeterminate
        );
    }

    /// Killed from outside (the kernel's memory pressure kill, a user's
    /// `kill`): the probe never answered, so nothing was decided.
    #[test]
    fn a_probe_killed_from_outside_is_indeterminate() {
        assert_eq!(
            run_probe_command(sh("kill -KILL $$"), Duration::from_secs(5)),
            Probe::Indeterminate
        );
    }

    /// A probe that hangs (swap thrashing, a stalled read) is cut off and
    /// reported as indeterminate, within the budget it was given.
    #[test]
    fn a_probe_that_hangs_is_indeterminate_and_is_stopped() {
        let started = Instant::now();
        assert_eq!(
            run_probe_command(sh("sleep 60"), Duration::from_millis(300)),
            Probe::Indeterminate
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the probe was not stopped at its budget: {:?}",
            started.elapsed()
        );
    }

    /// The probe answered: a non-zero exit is the child's own verdict that the
    /// graph does not open.
    #[test]
    fn a_probe_that_exits_nonzero_says_the_graph_does_not_open() {
        assert_eq!(
            run_probe_command(sh("exit 1"), Duration::from_secs(5)),
            Probe::DoesNotOpen
        );
    }

    /// A fault raised by the image itself (the SIGBUS of a damaged graph, an
    /// uncaught lbug abort) is the evidence the probe exists to find.
    #[test]
    fn a_fault_in_the_image_says_the_graph_does_not_open() {
        assert_eq!(
            run_probe_command(sh("kill -BUS $$"), Duration::from_secs(5)),
            Probe::DoesNotOpen
        );
        assert_eq!(
            run_probe_command(sh("kill -ABRT $$"), Duration::from_secs(5)),
            Probe::DoesNotOpen
        );
    }

    #[test]
    fn a_probe_that_exits_zero_opens_the_graph() {
        assert_eq!(
            run_probe_command(sh("exit 0"), Duration::from_secs(5)),
            Probe::Opens
        );
    }
}
