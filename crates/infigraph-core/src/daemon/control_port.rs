//! The daemon's side of socket control (#155): what `Status` reads without
//! the coordinator, and the bounded channel `Control` uses to reach it.
//!
//! Owned by the coordinator for the daemon's whole run, like `Liveness`, and
//! handed to each `ReadService` it binds -- the service is rebuilt on a #187
//! socket rebind, and state living there would reset under a live daemon.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::liveness::Liveness;
use super::read_protocol::{ControlRequest, RoleState, StatusReport, WatchRole};

/// Control requests the coordinator may have queued before new ones are
/// refused. Bounded so a wedged coordinator fails requests fast instead of
/// piling up threads.
pub const CONTROL_QUEUE: usize = 8;

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
    }
}

fn decode(v: u8) -> RoleState {
    match v {
        0 => RoleState::Running,
        1 => RoleState::Stopped,
        2 => RoleState::Disabled,
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
            code: AtomicU8::new(encode(RoleState::Stopped)),
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
        }
    }
}

pub struct ControlPort {
    tx: mpsc::SyncSender<ControlMsg>,
    pub state: DaemonState,
    in_flight: AtomicUsize,
}

/// Counts one control thread. The coordinator waits for the count to reach
/// zero after its loop ends, which is what lets a Daemon Stop reply reach
/// the client before the process exits -- without the service ever joining
/// a control thread (a #187 rebind drops the service on the coordinator's
/// own thread, and joining there would deadlock against it).
pub struct InFlightGuard(Arc<ControlPort>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ControlPort {
    pub fn new(grace_secs: u64, idle_check_secs: u64) -> (Arc<Self>, mpsc::Receiver<ControlMsg>) {
        let (tx, rx) = mpsc::sync_channel(CONTROL_QUEUE);
        let port = Arc::new(Self {
            tx,
            state: DaemonState::new(grace_secs, idle_check_secs),
            in_flight: AtomicUsize::new(0),
        });
        (port, rx)
    }

    /// Queue a request for the coordinator without blocking.
    pub fn submit(&self, request: ControlRequest) -> Result<mpsc::Receiver<ControlReply>, String> {
        let (reply, rx) = mpsc::channel();
        match self.tx.try_send(ControlMsg { request, reply }) {
            Ok(()) => Ok(rx),
            Err(mpsc::TrySendError::Full(_)) => Err(BUSY.to_string()),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(SHUTTING_DOWN.to_string()),
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

    #[test]
    fn a_fresh_state_reports_code_stopped_and_docs_not_owned() {
        let (port, _rx) = ControlPort::new(1800, 60);
        let r = port.state.report(&Liveness::new(), now_secs());
        assert_eq!(r.code, RoleState::Stopped);
        assert_eq!(r.docs, RoleState::NotOwned);
        assert_eq!((r.grace_secs, r.idle_check_secs), (1800, 60));
        assert_eq!(r.pid, std::process::id());
        assert_eq!(r.build, crate::build_hash());
    }

    #[test]
    fn the_report_carries_leases_idle_and_work_in_flight() {
        let (port, _rx) = ControlPort::new(10, 1);
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
        let (port, rx) = ControlPort::new(0, 1);
        let replies: Vec<_> = (0..CONTROL_QUEUE)
            .map(|_| port.submit(req()).unwrap())
            .collect();
        assert_eq!(port.submit(req()).unwrap_err(), BUSY);
        drop(replies);
        drop(rx);
        assert_eq!(port.submit(req()).unwrap_err(), SHUTTING_DOWN);
    }

    #[test]
    fn wait_idle_returns_once_every_guard_is_dropped() {
        let (port, _rx) = ControlPort::new(0, 1);
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
}
