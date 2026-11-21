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
    let result = open_and_scan(Path::new(&path));
    if let Err(e) = &result {
        // The parent captures this and logs it with its verdict: an error the
        // probe threw away is how a 162 MB graph was quarantined with nothing
        // to say why (sittir, 2026-11-20).
        eprintln!("{e:#}");
    }
    std::process::exit(exit_code_for(&result));
}

/// The probe child's exit status when it could not open the graph for a
/// reason that says nothing about the image: memory, a lock, an IO error that
/// is not the file's. [`classify_probe_exit`] reads it as
/// [`Probe::Indeterminate`].
const UNDECIDED_EXIT: i32 = 3;

/// The probe child's exit status for its result. Only a verdict against the
/// image itself is "does not open" (exit 1): that is the one answer that
/// licenses a quarantine (DESIGN-hardening R3.1.1, no destructive recovery
/// without a corruption verdict), so any other failure is undecided. The
/// verdict is [`crate::graph::GraphCorruption`], which `open_read_only` and
/// `validate_db_file` raise exactly when they have judged the file; a fault
/// signal from a damaged image is judged by the parent instead.
fn exit_code_for(result: &anyhow::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) if e.chain().any(|c| c.is::<crate::graph::GraphCorruption>()) => 1,
        Err(_) => UNDECIDED_EXIT,
    }
}

fn open_and_scan(path: &Path) -> anyhow::Result<()> {
    // Opening is NOT enough, and assuming it was nearly shipped a bug: the
    // one genuinely damaged image found on this machine OPENS cleanly and
    // then faults with SIGBUS on the first query that touches node data. A
    // probe that only opened would have called that graph recoverable and
    // handed it straight back to the daemon to die on.
    crate::graph::GraphStore::open_read_only(path).and_then(|store| {
        let conn = store.connection()?;
        // A count is not enough either: it can be answered from metadata
        // without ever touching row data, and the damaged image returns one
        // happily. Scan the rows and CONSUME them -- results are lazy, so an
        // un-iterated query proves nothing. A damaged image faults here (a
        // signal the parent judges); a query that merely errors (a full
        // buffer pool) is no verdict on the file.
        let rows = conn
            .query("MATCH (s:Symbol) RETURN s.id, s.name, s.file")
            .map_err(|e| anyhow::anyhow!("probe query failed: {e}"))?;
        for _ in rows {}
        Ok(())
    })
}

/// Whether `graph_path` opens read-only AND survives a query that reads
/// node data, decided in a child process so a damaged file cannot take this
/// one down.
///
/// A process that cannot run a probe at all gets [`Probe::Unavailable`], not
/// `Indeterminate`: no retry can ever decide, so "retry shortly" would be false.
pub fn graph_opens(graph_path: &Path) -> Probe {
    #[cfg(test)]
    if let Some(forced) = test_seam::answer() {
        return forced;
    }
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
    cmd.env(PROBE_ENV, graph_path);
    // A probe child must never inherit -- or default into -- a backend that
    // would send it somewhere other than this file. The whole question being
    // asked is "does THIS file open?", so it is pinned, not merely unset:
    // once local stops being the default (#159) an unset variable routes the
    // probe to a daemon and it reports on a socket instead.
    cmd.env(crate::BACKEND_ENV, crate::LOCAL_BACKEND);
    let run = run_probe_command(cmd, PROBE_TIMEOUT);
    if run.verdict != Probe::Opens {
        // Every answer but "opens" can lead to a quarantine or a refused
        // open, so each one says why, in this process's log.
        eprintln!(
            "[graph] probe of {}: {:?}{}",
            graph_path.display(),
            run.verdict,
            if run.stderr.is_empty() {
                String::new()
            } else {
                format!(" -- {}", run.stderr)
            }
        );
    }
    run.verdict
}

/// How a probe child ended, and what it printed on stderr (at most
/// [`PROBE_STDERR_MAX`] bytes, trimmed).
#[derive(Debug)]
pub struct ProbeRun {
    pub verdict: Probe,
    pub stderr: String,
}

/// The most of a probe child's stderr kept for the log. The child prints one
/// error line; lbug may add a few. The rest is read and dropped, so a chatty
/// child never blocks on a full pipe.
pub const PROBE_STDERR_MAX: usize = 2048;

/// What a probe of one graph file established.
///
/// Three answers, because only one of them is evidence about the graph. A
/// probe that could not start, was killed from outside (the kernel's memory
/// pressure kill, a user's `kill`), ran out of time, or failed for a reason
/// that is not the image's (memory, a lock) never answered anything:
/// quarantining on it would discard a healthy graph over a transient
/// condition, as it did on sittir (2026-11-20, 162 MB for a 22 KB WAL). Only
/// the probe's own corruption verdict, or a fault raised by the image itself,
/// says the graph does not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The child opened and scanned the graph.
    Opens,
    /// The child judged the image corrupt, or the image faulted under it.
    DoesNotOpen,
    /// Nothing was learned: the probe did not start, was killed from outside,
    /// ran past [`PROBE_TIMEOUT`], or failed without a verdict on the image.
    /// The caller must not quarantine.
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

/// The one test-only override of what a probe answers. Every probe reaches
/// [`graph_opens`] (the writer open and the read path through
/// `try_recover_by_setting_wal_aside`, the daemon through the same), so the
/// override sits there and nowhere else. Thread-local: a test's `CodeIndex`
/// work runs on its own thread, so parallel tests cannot see each other's.
#[cfg(test)]
pub(crate) mod test_seam {
    use super::Probe;
    use std::cell::Cell;

    thread_local! {
        static FORCED: Cell<Option<Probe>> = const { Cell::new(None) };
        static CALLS: Cell<usize> = const { Cell::new(0) };
    }

    /// Makes every probe on this thread answer `answer` until the guard drops.
    /// Restores whatever was set before, including nothing.
    pub struct Guard {
        previous: Option<Probe>,
        previous_calls: usize,
    }

    pub fn force(answer: Probe) -> Guard {
        Guard {
            previous: FORCED.with(|f| f.replace(Some(answer))),
            previous_calls: CALLS.with(|c| c.replace(0)),
        }
    }

    impl Guard {
        /// Probes asked on this thread since the guard was taken.
        pub fn calls(&self) -> usize {
            CALLS.with(|c| c.get())
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            FORCED.with(|f| f.set(self.previous));
            CALLS.with(|c| c.set(self.previous_calls));
        }
    }

    /// Called by [`super::graph_opens`]: counts the probe and returns the
    /// forced answer, if one is set.
    pub(super) fn answer() -> Option<Probe> {
        let forced = FORCED.with(|f| f.get());
        if forced.is_some() {
            CALLS.with(|c| c.set(c.get() + 1));
        }
        forced
    }
}

/// Runs a prepared probe command, waits at most `timeout`, and classifies how
/// it ended, keeping what it printed on stderr. A timed-out child is killed
/// and reaped before this returns.
pub fn run_probe_command(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> ProbeRun {
    use std::process::Stdio;
    use std::time::Instant;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let Ok(mut child) = command.spawn() else {
        return ProbeRun {
            verdict: Probe::Indeterminate,
            stderr: "the probe could not be started".to_string(),
        };
    };
    // Drained on its own thread, so the child never blocks on a full pipe
    // while this one polls for its exit.
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(pipe) = child.stderr.take() {
        std::thread::spawn(move || {
            let _ = tx.send(read_bounded(pipe, PROBE_STDERR_MAX));
        });
    }
    let deadline = Instant::now() + timeout;
    let verdict = loop {
        match child.try_wait() {
            Ok(Some(status)) => break classify_probe_exit(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break Probe::Indeterminate;
            }
        }
    };
    // Bounded: a grandchild still holding the pipe must not hold this up.
    let stderr = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or_default();
    ProbeRun { verdict, stderr }
}

/// Reads `reader` to its end, keeping at most `max` bytes, trimmed.
fn read_bounded(mut reader: impl std::io::Read, max: usize) -> String {
    let mut kept = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = max.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).trim().to_string()
}

fn classify_probe_exit(status: std::process::ExitStatus) -> Probe {
    if status.success() {
        return Probe::Opens;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        match status.signal() {
            // The child judged the image itself (see `exit_code_for`). Any
            // other status -- its undecided exit, a panic -- is no verdict.
            None if status.code() == Some(1) => Probe::DoesNotOpen,
            None => Probe::Indeterminate,
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
        if status.code() == Some(1) {
            Probe::DoesNotOpen
        } else {
            Probe::Indeterminate
        }
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
            run_probe_command(command, Duration::from_secs(5)).verdict,
            Probe::Indeterminate
        );
    }

    /// Killed from outside (the kernel's memory pressure kill, a user's
    /// `kill`): the probe never answered, so nothing was decided.
    #[test]
    fn a_probe_killed_from_outside_is_indeterminate() {
        assert_eq!(
            run_probe_command(sh("kill -KILL $$"), Duration::from_secs(5)).verdict,
            Probe::Indeterminate
        );
    }

    /// A probe that hangs (swap thrashing, a stalled read) is cut off and
    /// reported as indeterminate, within the budget it was given.
    #[test]
    fn a_probe_that_hangs_is_indeterminate_and_is_stopped() {
        let started = Instant::now();
        assert_eq!(
            run_probe_command(sh("sleep 60"), Duration::from_millis(300)).verdict,
            Probe::Indeterminate
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the probe was not stopped at its budget: {:?}",
            started.elapsed()
        );
    }

    /// Exit 1 is the child's own verdict that the image is corrupt.
    #[test]
    fn a_probe_that_exits_one_says_the_graph_does_not_open() {
        assert_eq!(
            run_probe_command(sh("exit 1"), Duration::from_secs(5)).verdict,
            Probe::DoesNotOpen
        );
    }

    /// Any other exit is no verdict on the image: the child's undecided exit
    /// (a failure of memory or a lock), or a panic. The sittir quarantine of
    /// 2026-11-20 was a failure like this, read as "corrupt".
    #[test]
    fn a_probe_that_fails_without_a_verdict_is_indeterminate() {
        for script in [format!("exit {UNDECIDED_EXIT}"), "exit 101".to_string()] {
            assert_eq!(
                run_probe_command(sh(&script), Duration::from_secs(5)).verdict,
                Probe::Indeterminate,
                "{script}"
            );
        }
    }

    /// What the child printed comes back with its verdict, so the parent can
    /// log why it did not open.
    #[test]
    fn a_probe_keeps_what_the_child_printed() {
        let run = run_probe_command(
            sh("echo 'buffer pool is full' >&2; exit 3"),
            Duration::from_secs(5),
        );
        assert_eq!(run.verdict, Probe::Indeterminate);
        assert_eq!(run.stderr, "buffer pool is full");
    }

    /// A chatty child is drained to its end (it never blocks on a full pipe)
    /// and only the first bytes are kept.
    #[test]
    fn a_chatty_probe_is_drained_and_capped() {
        let run = run_probe_command(
            sh("head -c 200000 /dev/zero | tr '\\0' x >&2; exit 1"),
            Duration::from_secs(20),
        );
        assert_eq!(run.verdict, Probe::DoesNotOpen);
        assert_eq!(run.stderr.len(), PROBE_STDERR_MAX);
    }

    /// The child's own classification: only a corruption verdict, however
    /// deeply it is wrapped in context, is exit 1.
    #[test]
    fn only_a_corruption_verdict_is_exit_one() {
        use crate::graph::GraphCorruption;
        let corrupt = || {
            anyhow::Error::new(GraphCorruption {
                detail: "torn WAL".into(),
            })
        };
        assert_eq!(exit_code_for(&Ok(())), 0);
        assert_eq!(exit_code_for(&Err(corrupt())), 1);
        assert_eq!(
            exit_code_for(&Err(corrupt().context("opening the probe graph"))),
            1
        );
        assert_eq!(
            exit_code_for(&Err(anyhow::anyhow!(
                "failed to open kuzu db (read-only): Cannot allocate memory"
            ))),
            UNDECIDED_EXIT
        );
        assert_eq!(
            exit_code_for(&Err(anyhow::anyhow!(
                "probe query failed: buffer pool is full"
            ))),
            UNDECIDED_EXIT
        );
    }

    /// A file too short to be a graph is judged by the preflight alone, so the
    /// probe can still call it corrupt and recovery still rebuilds it.
    #[test]
    fn a_truncated_graph_file_is_a_corruption_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.kuzu");
        std::fs::write(&path, b"short").unwrap();
        let err = crate::graph::validate_db_file(&path).unwrap_err();
        assert_eq!(exit_code_for(&Err(err)), 1);
    }

    /// A fault raised by the image itself (the SIGBUS of a damaged graph, an
    /// uncaught lbug abort) is the evidence the probe exists to find.
    #[test]
    fn a_fault_in_the_image_says_the_graph_does_not_open() {
        assert_eq!(
            run_probe_command(sh("kill -BUS $$"), Duration::from_secs(5)).verdict,
            Probe::DoesNotOpen
        );
        assert_eq!(
            run_probe_command(sh("kill -ABRT $$"), Duration::from_secs(5)).verdict,
            Probe::DoesNotOpen
        );
    }

    #[test]
    fn a_probe_that_exits_zero_opens_the_graph() {
        assert_eq!(
            run_probe_command(sh("exit 0"), Duration::from_secs(5)).verdict,
            Probe::Opens
        );
    }
}
