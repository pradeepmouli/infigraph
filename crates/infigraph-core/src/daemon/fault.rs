//! The daemon's latched-failure record (#165).
//!
//! When the daemon's writes fail in a way the next tick cannot fix -- the
//! disk is full, the growth breaker refuses, the graph will not open -- it
//! keeps retrying every tick, and all a client used to see was its own
//! 600s timeout. sittir (2026-09-10) spent that stall on ENOSPC with the
//! daemon logging only `index operation busy (...), retrying next tick`.
//!
//! The daemon now writes `.infigraph/daemon.fault.json` naming itself,
//! the failure and its class, and removes it on its next success. A client
//! reads it before and while waiting, and fails fast with the daemon's own
//! error. The record carries the writer's [`LockInfo`], so it counts only
//! while *that* process lives: a record left by a daemon that died is
//! ignored, never an obstacle to the next one.
//!
//! Best-effort throughout. On a full disk the record itself may not fit;
//! a client then falls back to its ordinary timeout, which is where it was
//! before this existed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::daemon_protocol::WriteRequest;
use crate::lockfile::LockInfo;

const FAULT_FILE: &str = "daemon.fault.json";

/// Why the daemon cannot make progress. Each class is one the daemon's own
/// retry loop cannot clear; transient failures (lock contention, a
/// checkpoint in progress) never produce a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FaultClass {
    /// The disk is full (ENOSPC).
    DiskFull,
    /// The growth breaker refused a write (#100). A full reindex is the
    /// remedy, so it is still admitted.
    GrowthRefused,
    /// The daemon cannot open its graph.
    OpenFailed,
}

impl FaultClass {
    /// The class of a failed write or open, or `None` for one worth
    /// retrying. Open failures are classified separately by
    /// [`FaultClass::of_open_failure`]: only the caller knows the error came
    /// from an open.
    pub fn of(err: &anyhow::Error) -> Option<Self> {
        if is_disk_full(err) {
            Some(Self::DiskFull)
        } else if crate::graph::growth_gate::is_write_refusal(err) {
            Some(Self::GrowthRefused)
        } else {
            None
        }
    }

    /// [`FaultClass::of`] for a failed graph open: anything but a transient
    /// open failure is a fault.
    pub fn of_open_failure(err: &anyhow::Error) -> Option<Self> {
        Self::of(err).or_else(|| {
            (!crate::graph::store::is_transient_open_error(err)).then_some(Self::OpenFailed)
        })
    }

    /// Whether a daemon latched on this class should still be sent
    /// `request`. Only a full reindex under a growth refusal: rebuilding is
    /// how a too-large graph is fixed, while a full disk or an unopenable
    /// graph fails a rebuild exactly as it fails everything else.
    pub fn admits(self, request: &WriteRequest) -> bool {
        self == Self::GrowthRefused && matches!(request, WriteRequest::FullReindex)
    }

    /// Whether starting a fresh daemon can clear this class. A full disk
    /// can be: the old process may be what holds the space, through an
    /// unlinked graph it still has open. An open failure can be too. A
    /// growth refusal cannot -- the graph is the problem, and a rebuild
    /// through the running daemon fixes it without costing readers a
    /// restart.
    pub fn cleared_by_restart(self) -> bool {
        self != Self::GrowthRefused
    }
}

/// Whether anything in `err`'s cause chain is a full disk. lbug reports
/// through strings rather than `io::Error`, so its message is matched too.
fn is_disk_full(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::StorageFull)
            || cause.to_string().contains("No space left on device")
    })
}

/// A latched failure, as published by the daemon that hit it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonFault {
    /// The daemon that wrote this record.
    pub holder: LockInfo,
    pub class: FaultClass,
    /// The error, with its context chain.
    pub error: String,
    /// Unix epoch seconds when this class was first recorded.
    pub since: u64,
}

impl std::fmt::Display for DaemonFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let now = LockInfo::current("").acquired_at;
        write!(
            f,
            "the daemon (pid {}) has been failing for {}s with a {:?} fault it cannot retry \
             past: {}",
            self.holder.pid,
            now.saturating_sub(self.since),
            self.class,
            self.error
        )
    }
}

pub fn fault_path(infigraph_dir: &Path) -> PathBuf {
    infigraph_dir.join(FAULT_FILE)
}

/// Record `class` for this process. Rewrites only when the class changes,
/// so a daemon failing every tick writes once and keeps its `since`.
pub fn record(infigraph_dir: &Path, class: FaultClass, error: &str) {
    let path = fault_path(infigraph_dir);
    if read(&path).is_some_and(|f| f.holder.pid == std::process::id() && f.class == class) {
        return;
    }
    let holder = LockInfo::current("daemon");
    let fault = DaemonFault {
        since: holder.acquired_at,
        holder,
        class,
        error: error.to_string(),
    };
    let written = serde_json::to_string(&fault)
        .map_err(anyhow::Error::from)
        .and_then(|json| crate::daemon_protocol::write_atomic(&path, &json));
    match written {
        Ok(()) => eprintln!("[daemon] latched a {class:?} fault: {error}"),
        Err(e) => eprintln!(
            "[daemon] could not record a {class:?} fault at {} ({e:#}): {error}",
            path.display()
        ),
    }
}

/// Classify `err` with `classify` and record it if it is a fault.
pub fn record_if_fault(
    infigraph_dir: &Path,
    err: &anyhow::Error,
    classify: fn(&anyhow::Error) -> Option<FaultClass>,
) {
    if let Some(class) = classify(err) {
        record(infigraph_dir, class, &format!("{err:#}"));
    }
}

/// Remove the record after a success. `only` limits it to one class: a
/// graph that opens again clears an open failure, but says nothing about a
/// full disk.
pub fn clear(infigraph_dir: &Path, only: Option<FaultClass>) {
    let path = fault_path(infigraph_dir);
    let Some(fault) = read(&path) else {
        return;
    };
    if only.is_some_and(|class| class != fault.class) {
        return;
    }
    if std::fs::remove_file(&path).is_ok() {
        eprintln!("[daemon] cleared the {:?} fault", fault.class);
    }
}

/// The fault the running daemon has latched, if any. A record whose writer
/// has exited is stale and ignored.
pub fn live_fault(infigraph_dir: &Path) -> Option<DaemonFault> {
    read(&fault_path(infigraph_dir)).filter(|f| crate::lockfile::holder_is_alive(&f.holder))
}

fn read(path: &Path) -> Option<DaemonFault> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enospc() -> anyhow::Error {
        anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::StorageFull))
            .context("drain failed")
    }

    #[test]
    fn a_full_disk_is_a_fault_as_an_io_error_or_as_lbug_text() {
        assert_eq!(FaultClass::of(&enospc()), Some(FaultClass::DiskFull));
        let lbug = anyhow::anyhow!("IO exception: No space left on device (os error 28)");
        assert_eq!(FaultClass::of(&lbug), Some(FaultClass::DiskFull));
    }

    #[test]
    fn a_growth_refusal_is_a_fault_that_still_admits_a_full_reindex() {
        let refusal = anyhow::anyhow!(
            "{}: graph grew 9x",
            crate::graph::growth_gate::WRITE_REFUSED_PREFIX
        )
        .context("drain failed");
        let class = FaultClass::of(&refusal).expect("a refusal is a fault");
        assert_eq!(class, FaultClass::GrowthRefused);
        assert!(class.admits(&WriteRequest::FullReindex));
        assert!(!class.cleared_by_restart());
        assert!(!FaultClass::DiskFull.admits(&WriteRequest::FullReindex));
    }

    #[test]
    fn ordinary_and_transient_failures_are_not_faults() {
        assert_eq!(
            FaultClass::of(&anyhow::anyhow!("parse error in a.rs")),
            None
        );
        let contention = anyhow::anyhow!("Could not set lock on file : /x/graph");
        assert_eq!(FaultClass::of_open_failure(&contention), None);
        assert_eq!(
            FaultClass::of_open_failure(&anyhow::anyhow!("Corrupted wal file")),
            Some(FaultClass::OpenFailed)
        );
    }

    #[test]
    fn a_recorded_fault_is_live_while_its_writer_runs_and_clears_on_success() {
        let dir = tempfile::tempdir().unwrap();
        assert!(live_fault(dir.path()).is_none());

        record_if_fault(dir.path(), &enospc(), FaultClass::of);
        let fault = live_fault(dir.path()).expect("this process is alive");
        assert_eq!(fault.class, FaultClass::DiskFull);
        assert!(fault.error.contains("drain failed"), "{}", fault.error);

        // An open succeeding says nothing about the disk.
        clear(dir.path(), Some(FaultClass::OpenFailed));
        assert!(live_fault(dir.path()).is_some());
        clear(dir.path(), None);
        assert!(live_fault(dir.path()).is_none());
    }

    #[test]
    fn repeated_failures_keep_the_first_since() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), FaultClass::DiskFull, "first");
        let mut first = live_fault(dir.path()).unwrap();
        first.since -= 100;
        std::fs::write(
            fault_path(dir.path()),
            serde_json::to_string(&first).unwrap(),
        )
        .unwrap();
        record(dir.path(), FaultClass::DiskFull, "second");
        let again = live_fault(dir.path()).unwrap();
        assert_eq!((again.since, again.error.as_str()), (first.since, "first"));
    }

    #[test]
    fn a_record_left_by_a_dead_daemon_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), FaultClass::DiskFull, "gone");
        let mut fault = read(&fault_path(dir.path())).unwrap();
        fault.holder.pid = u32::MAX - 1;
        std::fs::write(
            fault_path(dir.path()),
            serde_json::to_string(&fault).unwrap(),
        )
        .unwrap();
        assert!(live_fault(dir.path()).is_none());
    }
}
