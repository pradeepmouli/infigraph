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
/// A probe that cannot be spawned at all returns `false`: "unknown" must not
/// masquerade as "healthy" on a path whose whole job is deciding whether to
/// keep the caller's data.
pub fn graph_opens(graph_path: &Path) -> bool {
    // Never re-invoke an executable that does not handle PROBE_ENV; a test
    // binary would re-run its whole suite. See PROBE_CAPABLE.
    if !PROBE_CAPABLE.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.env(PROBE_ENV, graph_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // A probe child must never inherit a backend override that would send it
    // somewhere other than this file.
    cmd.env_remove("INFIGRAPH_BACKEND");
    match cmd.status() {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// Where a torn WAL is filed when the base image beneath it turns out to be
/// fine. Kept, not deleted: it is the evidence for why a recovery happened.
pub fn torn_wal_path(infigraph_dir: &Path, graph_name: &str, suffix: &str, ts: u64) -> PathBuf {
    infigraph_dir.join(format!("{graph_name}.torn-wal.{ts}{suffix}"))
}
