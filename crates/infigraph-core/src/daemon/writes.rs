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

/// How often a waiting write re-reads the daemon's fault record -- a file
/// read plus a pid-liveness check, so slower than the wait's own poll.
const FAULT_POLL: Duration = Duration::from_millis(250);

/// True at most once per `period`: a throttle for a check inside a faster
/// loop.
struct Every {
    period: Duration,
    next: Instant,
}

impl Every {
    fn new(period: Duration, now: Instant) -> Self {
        Self { period, next: now }
    }

    fn due(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + self.period;
        true
    }
}

/// Where bulk payloads wait for the daemon, under `.infigraph/`.
pub const SIDECAR_DIR: &str = "write-tmp";

pub struct WriteOpts<'a> {
    pub timeout: Duration,
    pub cancel: Option<&'a CancellationToken>,
    /// For a `FullReindex` only (#150): time the daemon spends waiting for a
    /// machine-wide reindex slot is not counted against `timeout`, and this
    /// is called once, with the daemon's notice, when that wait is first
    /// seen. `None` (every other write) counts all time, as before.
    pub on_slot_wait: Option<&'a dyn Fn(&crate::degraded::Notice)>,
}

/// How often a `FullReindex` submit asks the daemon whether it is waiting
/// for a slot.
const SLOT_WAIT_POLL: Duration = Duration::from_secs(2);

/// How much of a submit's wait was the daemon waiting for a reindex slot.
/// That time is not the rebuild running, so it does not count against the
/// client's timeout -- up to the daemon's own bound on such a wait, so a
/// client can never be held longer than the daemon itself would wait.
#[derive(Default)]
struct SlotWaitClock {
    paused: Duration,
    last_seen_waiting: Option<Instant>,
    announced: bool,
}

impl SlotWaitClock {
    /// Record one status poll at `now`. True when the wait should be
    /// announced, which happens once.
    fn observe(&mut self, now: Instant, waiting: bool) -> bool {
        if !waiting {
            self.last_seen_waiting = None;
            return false;
        }
        if let Some(prev) = self.last_seen_waiting {
            self.paused = (self.paused + now.saturating_duration_since(prev))
                .min(super::reindex_gate::MAX_SLOT_WAIT);
        }
        self.last_seen_waiting = Some(now);
        !std::mem::replace(&mut self.announced, true)
    }

    /// Time that counts against the timeout, of `elapsed` in total.
    fn counted(&self, elapsed: Duration) -> Duration {
        elapsed.saturating_sub(self.paused)
    }
}

/// The daemon's "waiting for a reindex slot" notice, if it reports one.
fn slot_wait_notice(root: &Path) -> Option<crate::degraded::Notice> {
    super::control::query_status(root)
        .ok()?
        .degraded
        .into_iter()
        .find(|n| n.key == crate::degraded::REINDEX_WAITING_FOR_SLOT)
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
    let mut fault_check = Every::new(FAULT_POLL, started);
    let mut slot_poll = Every::new(SLOT_WAIT_POLL, started + SLOT_WAIT_POLL);
    let mut slot_wait = SlotWaitClock::default();
    exchange(
        stream,
        &WriteFrame {
            write: request.clone(),
        },
        || {
            if opts.cancel.is_some_and(|t| t.is_cancelled()) {
                return Some(anyhow::Error::new(WriteRequestCancelled));
            }
            if fault_check.due(Instant::now()) {
                if let Some(faulted) = blocking_fault(&infigraph_dir, request) {
                    return Some(anyhow::Error::new(faulted));
                }
            }
            if let Some(announce) = opts.on_slot_wait {
                let now = Instant::now();
                if slot_poll.due(now) {
                    let notice = slot_wait_notice(root);
                    if slot_wait.observe(now, notice.is_some()) {
                        if let Some(notice) = &notice {
                            announce(notice);
                        }
                    }
                }
            }
            (slot_wait.counted(started.elapsed()) >= opts.timeout).then(|| {
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
            on_slot_wait: None,
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

    /// Review minor (#204): the fault record is a file read plus a pid
    /// check, so the wait checks it on its own slower cadence.
    #[test]
    fn a_throttle_is_due_once_per_period() {
        let t0 = Instant::now();
        let mut every = Every::new(Duration::from_millis(250), t0);
        assert!(every.due(t0), "due at once");
        assert!(!every.due(t0 + Duration::from_millis(100)));
        assert!(!every.due(t0 + Duration::from_millis(249)));
        assert!(every.due(t0 + Duration::from_millis(250)));
        assert!(!every.due(t0 + Duration::from_millis(300)));
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

#[cfg(test)]
mod slot_wait_tests {
    use super::*;
    use crate::daemon::reindex_gate::MAX_SLOT_WAIT;

    /// Time the daemon reports waiting for a slot is taken off what counts
    /// against the timeout; time it is rebuilding is not.
    #[test]
    fn waiting_time_is_not_counted_and_running_time_is() {
        let t0 = Instant::now();
        let mut clock = SlotWaitClock::default();
        let secs = Duration::from_secs;

        assert!(
            clock.observe(t0 + secs(2), true),
            "the first sighting announces"
        );
        assert!(!clock.observe(t0 + secs(4), true), "and only the first");
        assert!(!clock.observe(t0 + secs(302), true));
        assert_eq!(clock.paused, secs(300));
        // The slot came; the rebuild runs for ten more minutes.
        assert!(!clock.observe(t0 + secs(304), false));
        assert_eq!(clock.counted(secs(304)), secs(4));
        assert_eq!(clock.counted(secs(904)), secs(604));
    }

    /// The pause can never exceed the daemon's own bound on a slot wait,
    /// whatever the daemon keeps reporting.
    #[test]
    fn the_pause_is_bounded_by_the_daemons_own_limit() {
        let t0 = Instant::now();
        let mut clock = SlotWaitClock::default();
        for minute in 0..=30 {
            clock.observe(t0 + Duration::from_secs(60 * minute), true);
        }
        assert_eq!(clock.paused, MAX_SLOT_WAIT);
        let elapsed = Duration::from_secs(1800);
        assert_eq!(clock.counted(elapsed), elapsed - MAX_SLOT_WAIT);
    }

    /// A gap between two sightings with a "not waiting" in between is not
    /// paused time: only consecutive waiting polls accumulate.
    #[test]
    fn only_consecutive_waiting_polls_accumulate() {
        let t0 = Instant::now();
        let mut clock = SlotWaitClock::default();
        let secs = Duration::from_secs;
        clock.observe(t0, true);
        clock.observe(t0 + secs(10), false);
        clock.observe(t0 + secs(100), true);
        assert_eq!(clock.paused, Duration::ZERO);
        clock.observe(t0 + secs(110), true);
        assert_eq!(clock.paused, secs(10));
    }
}
