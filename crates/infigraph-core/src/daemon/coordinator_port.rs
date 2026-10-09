//! The daemon's side of socket control and writes (#155, #204): what
//! `Status` reads without the coordinator, and the one bounded channel
//! control and writes reach it through.
//!
//! Owned by the coordinator for the daemon's whole run, like `Liveness`, and
//! handed to each `ReadService` it binds -- the service is rebuilt on a #187
//! socket rebind, and state living there would reset under a live daemon.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::liveness::Liveness;
use super::read_protocol::{ControlRequest, RoleState, StatusReport, WatchRole};
use crate::daemon_protocol::{WriteRequest, WriteResult};

/// Requests the coordinator may have queued. Control fails fast when it is
/// full, so a wedged coordinator refuses instead of piling up threads; a
/// write waits for a slot while its client is still connected (#204 D1).
pub const PORT_QUEUE: usize = 64;

/// How long a control thread waits for the coordinator's outcome. The
/// client's own deadline is longer, so this arrives as a reply, not an EOF.
pub const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

pub const BUSY: &str = "daemon busy: the coordinator has not taken control requests \
    for a while; `infigraph daemon-stop` falls back to the watch.stop sentinel";
pub const SHUTTING_DOWN: &str = "the daemon is shutting down";
pub const NO_CONTROL: &str = "this read service has no daemon control attached";

pub type ControlReply = std::result::Result<(), String>;

pub struct ControlMsg {
    pub request: ControlRequest,
    pub reply: mpsc::Sender<ControlReply>,
}

pub const DROPPED: &str =
    "the daemon dropped this write without answering it; the cause is in .infigraph/daemon.log";
pub const GAVE_UP: &str = "the client left before the coordinator had room for its write";

/// What the coordinator's one channel carries.
pub enum PortMsg {
    Control(ControlMsg),
    Write {
        request: WriteRequest,
        reply: WriteReply,
    },
}

/// Where one write's answer goes. Consumed by `send`, so a waiter is answered
/// at most once by type; answered `DROPPED` by `Drop` if it never was, so a
/// panic, an early return or a teardown cannot strand a client (#204 D8).
pub struct WriteReply {
    tx: Option<mpsc::Sender<WriteResult>>,
    gone: Arc<AtomicBool>,
}

impl WriteReply {
    pub fn channel() -> (Self, mpsc::Receiver<WriteResult>) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                tx: Some(tx),
                gone: Arc::default(),
            },
            rx,
        )
    }

    /// For the daemon's own writes (compaction, auto-recovery): nobody waits.
    pub fn internal() -> Self {
        Self {
            tx: None,
            gone: Arc::default(),
        }
    }

    pub fn send(mut self, result: WriteResult) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(result);
        }
    }

    /// Set by the connection thread when its client disconnects (#204 D2).
    pub fn gone_flag(&self) -> Arc<AtomicBool> {
        self.gone.clone()
    }

    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::SeqCst)
    }
}

impl Drop for WriteReply {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(WriteResult::Err {
                message: DROPPED.to_string(),
            });
        }
    }
}

impl std::fmt::Debug for WriteReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteReply")
            .field("waits", &self.tx.is_some())
            .field("gone", &self.is_gone())
            .finish()
    }
}

/// `Disabled` only when the persisted policy is off; a role that is merely
/// not running with the policy on is `Stopped`.
pub fn role_state(running: bool, policy_on: bool) -> RoleState {
    match (running, policy_on) {
        (true, _) => RoleState::Running,
        (false, true) => RoleState::Stopped,
        (false, false) => RoleState::Disabled,
    }
}

fn encode(s: RoleState) -> u8 {
    match s {
        RoleState::Running => 0,
        RoleState::Stopped => 1,
        RoleState::Disabled => 2,
        RoleState::NotOwned => 3,
        RoleState::Starting => 4,
    }
}

fn decode(v: u8) -> RoleState {
    match v {
        0 => RoleState::Running,
        1 => RoleState::Stopped,
        2 => RoleState::Disabled,
        4 => RoleState::Starting,
        _ => RoleState::NotOwned,
    }
}

pub struct DaemonState {
    code: AtomicU8,
    docs: AtomicU8,
    work_in_flight: AtomicBool,
    grace_secs: u64,
    idle_check_secs: u64,
}

impl DaemonState {
    fn new(grace_secs: u64, idle_check_secs: u64) -> Self {
        Self {
            code: AtomicU8::new(encode(RoleState::Starting)),
            docs: AtomicU8::new(encode(RoleState::NotOwned)),
            work_in_flight: AtomicBool::new(false),
            grace_secs,
            idle_check_secs,
        }
    }

    fn slot(&self, role: WatchRole) -> Option<&AtomicU8> {
        match role {
            WatchRole::Code => Some(&self.code),
            WatchRole::Docs => Some(&self.docs),
            WatchRole::Daemon => None,
        }
    }

    /// `WatchRole::Daemon` has no state of its own and is ignored.
    pub fn set_role(&self, role: WatchRole, state: RoleState) {
        if let Some(slot) = self.slot(role) {
            slot.store(encode(state), Ordering::SeqCst);
        }
    }

    pub fn role(&self, role: WatchRole) -> RoleState {
        self.slot(role)
            .map(|s| decode(s.load(Ordering::SeqCst)))
            .unwrap_or(RoleState::NotOwned)
    }

    pub fn set_work_in_flight(&self, on: bool) {
        self.work_in_flight.store(on, Ordering::SeqCst);
    }

    pub fn report(&self, liveness: &Liveness, now_secs: u64) -> StatusReport {
        StatusReport {
            pid: std::process::id(),
            build: crate::build_hash().to_string(),
            leases: liveness.leases(),
            idle_secs: liveness.idle_for(now_secs).map(|d| d.as_secs()),
            grace_secs: self.grace_secs,
            idle_check_secs: self.idle_check_secs,
            work_in_flight: self.work_in_flight.load(Ordering::SeqCst),
            code: self.role(WatchRole::Code),
            docs: self.role(WatchRole::Docs),
            degraded: crate::degraded::live::all(),
            judged: crate::degraded::live::judged(),
            lease_owners: liveness.lease_owners(),
        }
    }
}

pub struct CoordinatorPort {
    tx: mpsc::SyncSender<PortMsg>,
    pub state: DaemonState,
    in_flight: AtomicUsize,
}

/// Counts one control thread. The coordinator waits for the count to reach
/// zero after its loop ends, which is what lets a Daemon Stop reply reach
/// the client before the process exits -- without the service ever joining
/// a control thread (a #187 rebind drops the service on the coordinator's
/// own thread, and joining there would deadlock against it).
pub struct InFlightGuard(Arc<CoordinatorPort>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl CoordinatorPort {
    pub fn new(grace_secs: u64, idle_check_secs: u64) -> (Arc<Self>, mpsc::Receiver<PortMsg>) {
        let (tx, rx) = mpsc::sync_channel(PORT_QUEUE);
        let port = Arc::new(Self {
            tx,
            state: DaemonState::new(grace_secs, idle_check_secs),
            in_flight: AtomicUsize::new(0),
        });
        (port, rx)
    }

    /// Queue a request for the coordinator without blocking.
    pub fn submit_control(
        &self,
        request: ControlRequest,
    ) -> Result<mpsc::Receiver<ControlReply>, String> {
        let (reply, rx) = mpsc::channel();
        match self
            .tx
            .try_send(PortMsg::Control(ControlMsg { request, reply }))
        {
            Ok(()) => Ok(rx),
            Err(mpsc::TrySendError::Full(_)) => Err(BUSY.to_string()),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(SHUTTING_DOWN.to_string()),
        }
    }

    /// Queue a write, waiting for a slot rather than refusing (#204 D1).
    /// Stops waiting when `give_up` says so -- its client left, or cancelled.
    pub fn admit_write(
        &self,
        request: WriteRequest,
        reply: WriteReply,
        mut give_up: impl FnMut() -> bool,
    ) -> Result<(), String> {
        let mut msg = PortMsg::Write { request, reply };
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Disconnected(_)) => return Err(SHUTTING_DOWN.to_string()),
                Err(mpsc::TrySendError::Full(back)) => {
                    if give_up() {
                        return Err(GAVE_UP.to_string());
                    }
                    msg = back;
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    pub fn enter(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlightGuard(self.clone())
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// `true` once no control thread is in flight, `false` if `timeout`
    /// passed first.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while self.in_flight() > 0 {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }
}

/// The daemon's own writes, from inside its process (#204 D6): the SCIP
/// enrichment callback's import. No socket, no files.
#[derive(Clone)]
pub struct WriteSubmitter(Arc<CoordinatorPort>);

impl WriteSubmitter {
    pub fn new(port: Arc<CoordinatorPort>) -> Self {
        Self(port)
    }

    /// Queue `request` and wait for its answer. `cancel` ends the wait
    /// promptly (#138) and marks the write gone, so the coordinator drops it
    /// at pickup if it has not started.
    pub fn submit(
        &self,
        request: WriteRequest,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<WriteResult> {
        let (reply, rx) = WriteReply::channel();
        let gone = reply.gone_flag();
        let cancelled = || {
            gone.store(true, Ordering::SeqCst);
            anyhow::Error::new(crate::daemon_protocol::WriteRequestCancelled)
        };
        self.0
            .admit_write(request, reply, || cancel.is_cancelled())
            .map_err(|msg| {
                if cancel.is_cancelled() {
                    cancelled()
                } else {
                    anyhow::anyhow!(msg)
                }
            })?;
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => return Ok(result),
                Err(mpsc::RecvTimeoutError::Timeout) if cancel.is_cancelled() => {
                    return Err(cancelled())
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!(SHUTTING_DOWN),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::liveness::{now_secs, Liveness};
    use crate::daemon::read_protocol::{ControlRequest, RoleState, WatchAction, WatchRole};

    fn req() -> ControlRequest {
        ControlRequest {
            role: WatchRole::Code,
            action: WatchAction::Stop,
        }
    }

    #[test]
    fn role_state_distinguishes_stopped_from_disabled() {
        assert_eq!(role_state(true, true), RoleState::Running);
        assert_eq!(role_state(true, false), RoleState::Running);
        assert_eq!(role_state(false, true), RoleState::Stopped);
        assert_eq!(role_state(false, false), RoleState::Disabled);
    }

    /// #75: what this daemon reports about itself reaches a client in the
    /// status reply, and stops being reported once it clears.
    #[test]
    fn the_status_report_carries_this_process_s_live_degraded_modes() {
        let root = tempfile::tempdir().unwrap();
        let (port, _rx) = CoordinatorPort::new(0, 1);
        let reported = |key: &str| {
            port.state
                .report(&Liveness::new(), now_secs())
                .degraded
                .iter()
                .any(|n| n.key == key && n.message.contains("unique-reason-4711"))
        };
        assert!(!reported(crate::degraded::DOC_READS_UNAVAILABLE));

        crate::degraded::live::set(
            root.path(),
            crate::degraded::DegradedMode::DocReadsUnavailable {
                reason: "unique-reason-4711".to_string(),
            },
        );
        assert!(reported(crate::degraded::DOC_READS_UNAVAILABLE));

        crate::degraded::live::clear(root.path(), crate::degraded::DOC_READS_UNAVAILABLE);
        assert!(!reported(crate::degraded::DOC_READS_UNAVAILABLE));
    }

    #[test]
    fn a_fresh_state_reports_code_starting_and_docs_not_owned() {
        let (port, _rx) = CoordinatorPort::new(1800, 60);
        let r = port.state.report(&Liveness::new(), now_secs());
        // The endpoint binds seconds before the loop first publishes: a
        // daemon in that window is starting, not stopped.
        assert_eq!(r.code, RoleState::Starting);
        assert_eq!(r.docs, RoleState::NotOwned);
        assert_eq!((r.grace_secs, r.idle_check_secs), (1800, 60));
        assert_eq!(r.pid, std::process::id());
        assert_eq!(r.build, crate::build_hash());
    }

    #[test]
    fn the_report_carries_leases_idle_and_work_in_flight() {
        let (port, _rx) = CoordinatorPort::new(10, 1);
        let liveness = Liveness::new();
        port.state.set_role(WatchRole::Code, RoleState::Running);
        port.state.set_work_in_flight(true);
        liveness.lease_opened();
        let r = port.state.report(&liveness, now_secs());
        assert_eq!((r.leases, r.idle_secs, r.work_in_flight), (1, None, true));
        assert_eq!(r.code, RoleState::Running);
        liveness.lease_closed();
        liveness.last_activity_for_test(now_secs() - 5);
        assert_eq!(port.state.report(&liveness, now_secs()).idle_secs, Some(5));
    }

    #[test]
    fn a_full_queue_refuses_at_once_and_a_dropped_receiver_reads_as_shutting_down() {
        let (port, rx) = CoordinatorPort::new(0, 1);
        let replies: Vec<_> = (0..PORT_QUEUE)
            .map(|_| port.submit_control(req()).unwrap())
            .collect();
        assert_eq!(port.submit_control(req()).unwrap_err(), BUSY);
        drop(replies);
        drop(rx);
        assert_eq!(port.submit_control(req()).unwrap_err(), SHUTTING_DOWN);
    }

    #[test]
    fn wait_idle_returns_once_every_guard_is_dropped() {
        let (port, _rx) = CoordinatorPort::new(0, 1);
        let guard = port.enter();
        assert_eq!(port.in_flight(), 1);
        assert!(!port.wait_idle(std::time::Duration::from_millis(50)));
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(guard);
        });
        assert!(port.wait_idle(std::time::Duration::from_secs(2)));
        t.join().unwrap();
    }

    use crate::daemon_protocol::{WriteRequest, WriteResult};

    #[test]
    fn a_reply_is_sent_once_and_an_unsent_one_answers_dropped() {
        let (reply, rx) = WriteReply::channel();
        reply.send(WriteResult::Ok {
            total_files: 1,
            indexed_files: 1,
        });
        assert!(matches!(rx.recv().unwrap(), WriteResult::Ok { .. }));
        assert!(rx.recv().is_err(), "exactly one answer");

        let (reply, rx) = WriteReply::channel();
        drop(reply);
        match rx.recv().unwrap() {
            WriteResult::Err { message } => {
                assert_eq!(message, DROPPED);
                // The cause (a panicked drain, say) is only in the log.
                assert!(message.contains("daemon.log"), "{message}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn gone_is_shared_with_the_flag_and_internal_is_never_gone() {
        let (reply, _rx) = WriteReply::channel();
        reply.gone_flag().store(true, Ordering::SeqCst);
        assert!(reply.is_gone());
        assert!(!WriteReply::internal().is_gone());
        // No receiver: sending must not panic.
        WriteReply::internal().send(WriteResult::Err {
            message: "x".into(),
        });
    }

    #[test]
    fn control_and_writes_share_one_channel_in_arrival_order() {
        let (port, rx) = CoordinatorPort::new(0, 1);
        let _c = port.submit_control(req()).unwrap();
        let (reply, _r) = WriteReply::channel();
        port.admit_write(WriteRequest::FullReindex, reply, || false)
            .unwrap();
        assert!(matches!(rx.recv().unwrap(), PortMsg::Control(_)));
        assert!(matches!(
            rx.recv().unwrap(),
            PortMsg::Write {
                request: WriteRequest::FullReindex,
                ..
            }
        ));
    }

    #[test]
    fn a_write_waits_for_a_slot_and_can_give_up() {
        let (port, rx) = CoordinatorPort::new(0, 1);
        let held: Vec<_> = (0..PORT_QUEUE)
            .map(|_| port.submit_control(req()).unwrap())
            .collect();
        let (reply, _r) = WriteReply::channel();
        let mut tries = 0;
        assert_eq!(
            port.admit_write(WriteRequest::FullReindex, reply, || {
                tries += 1;
                tries > 3
            }),
            Err(GAVE_UP.to_string())
        );
        drop(held);
        drop(rx);
        let (reply, _r) = WriteReply::channel();
        assert_eq!(
            port.admit_write(WriteRequest::FullReindex, reply, || false),
            Err(SHUTTING_DOWN.to_string())
        );
    }

    #[test]
    fn a_cancelled_in_process_write_is_marked_gone() {
        let (port, rx) = CoordinatorPort::new(0, 1);
        let token = tokio_util::sync::CancellationToken::new();
        let submitter = WriteSubmitter::new(port);
        let t = std::thread::spawn({
            let token = token.clone();
            move || submitter.submit(WriteRequest::FullReindex, &token)
        });
        let PortMsg::Write { reply, .. } = rx.recv().unwrap() else {
            panic!("a write")
        };
        token.cancel();
        let err = t.join().unwrap().unwrap_err();
        assert!(err
            .downcast_ref::<crate::daemon_protocol::WriteRequestCancelled>()
            .is_some());
        assert!(reply.is_gone(), "the coordinator must drop it at pickup");
    }
}
