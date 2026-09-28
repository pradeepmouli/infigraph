//! The write client (#204): the only sender of `Write` frames. Every routed
//! write -- `DaemonKuzuBackend`, `cmd_index`'s full reindex,
//! `index_via_daemon` -- comes through `submit`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::control::{exchange, not_connected};
use super::read_endpoint::connect_allowing_for_startup;
use super::read_protocol::WriteFrame;
use crate::daemon_protocol::{DaemonFaulted, WriteRequest, WriteRequestCancelled, WriteResult};

/// Where bulk payloads wait for the daemon, under `.infigraph/`.
pub const SIDECAR_DIR: &str = "write-tmp";

pub struct WriteOpts<'a> {
    pub timeout: Duration,
    pub cancel: Option<&'a CancellationToken>,
}

/// The latched fault that should stop `request` from waiting on the daemon,
/// if any (#165).
pub(crate) fn blocking_fault(
    infigraph_dir: &Path,
    request: &WriteRequest,
) -> Option<DaemonFaulted> {
    let fault = crate::daemon::fault::live_fault(infigraph_dir)?;
    (!fault.class.admits(request)).then_some(DaemonFaulted(fault))
}

/// Send one write and wait for its result. Fails fast on a latched fault,
/// before connecting and while waiting; `opts.cancel` and `opts.timeout` end
/// the wait, and ending it closes the connection -- the daemon reads that
/// as the client leaving and drops the write if it has not started.
pub fn submit(root: &Path, request: &WriteRequest, opts: WriteOpts) -> anyhow::Result<WriteResult> {
    let infigraph_dir = root.join(".infigraph");
    if let Some(faulted) = blocking_fault(&infigraph_dir, request) {
        return Err(anyhow::Error::new(faulted));
    }
    // A routed write is a use of the daemon for as long as it waits: its
    // lease must not be released under it (`daemon::lease`).
    let _use = crate::daemon::lease::in_use(root);
    let stream =
        connect_allowing_for_startup(root).map_err(|_| anyhow::Error::new(not_connected(root)))?;
    let started = Instant::now();
    exchange(
        stream,
        &WriteFrame {
            write: request.clone(),
        },
        || {
            if opts.cancel.is_some_and(|t| t.is_cancelled()) {
                return Some(anyhow::Error::new(WriteRequestCancelled));
            }
            if let Some(faulted) = blocking_fault(&infigraph_dir, request) {
                return Some(anyhow::Error::new(faulted));
            }
            (started.elapsed() >= opts.timeout).then(|| {
                anyhow::anyhow!(
                    "the daemon did not answer the {} write within {:?}",
                    request.kind(),
                    opts.timeout
                )
            })
        },
    )
}

/// Distinguishes sidecars one process creates within the same nanosecond.
static SIDECAR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh sidecar path for a bulk payload, `<pid>-<nanos>-<n>.<ext>`, with
/// its directory created. The caller removes it on every error; the daemon
/// removes it once read; the daemon's startup sweeps leftovers older than 6h.
pub fn sidecar_path(root: &Path, ext: &str) -> PathBuf {
    let dir = root.join(".infigraph").join(SIDECAR_DIR);
    let _ = std::fs::create_dir_all(&dir);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let n = SIDECAR_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{}-{nanos}-{n}.{ext}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::coordinator_port::{CoordinatorPort, PortMsg};
    use crate::daemon::fault::{record, FaultClass};
    use std::sync::Arc;

    fn opts(timeout: Duration) -> WriteOpts<'static> {
        WriteOpts {
            timeout,
            cancel: None,
        }
    }

    /// A project whose daemon has latched `class`. This test process stands
    /// in for the daemon: its record is live for exactly as long as it runs.
    fn root_with_fault(class: FaultClass) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let infigraph_dir = dir.path().join(".infigraph");
        std::fs::create_dir_all(&infigraph_dir).unwrap();
        record(
            &infigraph_dir,
            class,
            "No space left on device (os error 28)",
        );
        dir
    }

    #[test]
    fn a_latched_fault_fails_a_submit_fast_with_the_daemons_error() {
        let dir = root_with_fault(FaultClass::DiskFull);
        let start = Instant::now();
        let err = submit(
            dir.path(),
            &WriteRequest::FullReindex,
            opts(Duration::from_secs(600)),
        )
        .expect_err("a latched fault must fail the submit");
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(err.downcast_ref::<DaemonFaulted>().is_some(), "{err:#}");
        assert!(
            err.to_string().contains("No space left on device"),
            "{err:#}"
        );
    }

    /// A fault the daemon latches while a client waits ends the wait too,
    /// and closing the connection withdraws the write.
    #[test]
    fn a_fault_latched_mid_wait_ends_the_wait_and_withdraws_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let infigraph_dir = dir.path().join(".infigraph");
        std::fs::create_dir_all(&infigraph_dir).unwrap();
        let (port, rx) = CoordinatorPort::new(1800, 60);
        let svc = crate::daemon::read_service::ReadService::start_serving(
            dir.path(),
            Arc::new(|| None),
            None,
            2,
            Arc::new(crate::daemon::liveness::Liveness::new()),
            Some(port),
        )
        .unwrap();
        let daemon = std::thread::spawn(move || {
            let PortMsg::Write { reply, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap()
            else {
                panic!("a write")
            };
            record(
                &infigraph_dir,
                FaultClass::OpenFailed,
                "graph will not open",
            );
            reply
        });
        let err = submit(
            dir.path(),
            &WriteRequest::Index { paths: None },
            opts(Duration::from_secs(30)),
        )
        .expect_err("the fault must end the wait");
        assert!(err.downcast_ref::<DaemonFaulted>().is_some(), "{err:#}");
        let reply = daemon.join().unwrap();
        #[cfg(unix)]
        {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !reply.is_gone() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(reply.is_gone(), "the write was withdrawn");
        }
        drop(reply);
        svc.shutdown();
    }

    /// A growth refusal still lets a full reindex through: it is the remedy.
    #[test]
    fn a_growth_refusal_still_admits_a_full_reindex() {
        let dir = root_with_fault(FaultClass::GrowthRefused);
        let err = submit(
            dir.path(),
            &WriteRequest::FullReindex,
            opts(Duration::from_millis(300)),
        )
        .expect_err("no daemon answers in this test");
        assert!(err.downcast_ref::<DaemonFaulted>().is_none(), "{err:#}");
        assert_eq!(
            err.downcast_ref::<crate::daemon::control::ControlError>(),
            Some(&crate::daemon::control::ControlError::NoDaemon),
            "it got as far as connecting: {err:#}"
        );
    }
}
