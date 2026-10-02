//! The client side of socket control and status (#155, #202). The only code
//! that sends `Status` or `Control` frames: the CLI, MCP, doctor and `ps` all
//! come through here.
//!
//! Neither call takes a lease or starts a daemon. Checking on a daemon must
//! never keep it alive.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use super::read_endpoint::{connect_allowing_for_startup, ReadEndpoint, ReadStream};
use super::read_protocol::{
    read_reply, write_op, ControlFrame, ControlRequest, DaemonOp, OpReply, StatusFrame,
    StatusReport, WatchAction, WatchRole,
};

pub const STATUS_DEADLINE: Duration = Duration::from_millis(500);
/// Longer than the daemon's own `CONTROL_REPLY_TIMEOUT`, so a slow
/// coordinator arrives as that timeout's reply, not as a hang-up.
pub const CONTROL_DEADLINE: Duration = Duration::from_secs(35);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// Nothing is listening and nobody holds `watch.lock`.
    NoDaemon,
    /// The daemon closed without replying: it could not parse the frame, so
    /// its build is older or newer than this one.
    Incompatible,
    /// No reply within the deadline, or `watch.lock` is held with no
    /// listener (starting, or wedged).
    Unresponsive,
    /// The daemon answered with an error.
    Refused(String),
    /// The daemon accepted a write, then closed before answering: it exited
    /// while serving it (#204).
    Lost,
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::NoDaemon => f.write_str("no daemon is running"),
            ControlError::Incompatible => {
                f.write_str("the daemon is an incompatible build; run `infigraph daemon-restart`")
            }
            ControlError::Unresponsive => {
                f.write_str("the daemon is not responding; run `infigraph daemon-restart`")
            }
            ControlError::Refused(msg) => write!(f, "the daemon refused: {msg}"),
            ControlError::Lost => f.write_str(
                "the daemon exited while serving this request; see .infigraph/daemon.log",
            ),
        }
    }
}

impl std::error::Error for ControlError {}

fn watch_lock_held(root: &Path) -> bool {
    super::lifecycle::daemon_is_alive(&root.join(".infigraph").join("watch.lock"))
}

pub(crate) fn not_connected(root: &Path) -> ControlError {
    if watch_lock_held(root) {
        ControlError::Unresponsive
    } else {
        ControlError::NoDaemon
    }
}

pub fn query_status(root: &Path) -> Result<StatusReport, ControlError> {
    query_status_within(root, STATUS_DEADLINE)
}

/// [`query_status`] with the caller's own `deadline`, for a caller that
/// would rather go without the answer than wait the full one.
pub fn query_status_within(root: &Path, deadline: Duration) -> Result<StatusReport, ControlError> {
    let stream = ReadEndpoint::for_root(root)
        .connect()
        .map_err(|_| not_connected(root))?;
    exchange(stream, &StatusFrame::default(), by(deadline))
}

pub fn send_control(root: &Path, role: WatchRole, action: WatchAction) -> Result<(), ControlError> {
    let stream = connect_allowing_for_startup(root).map_err(|_| not_connected(root))?;
    exchange(
        stream,
        &ControlFrame {
            control: ControlRequest { role, action },
        },
        by(CONTROL_DEADLINE),
    )
}

/// One status query per root, all at once, so a `ps` over many daemons
/// costs about one deadline.
pub fn query_status_many(roots: &[PathBuf]) -> Vec<Result<StatusReport, ControlError>> {
    let handles: Vec<_> = roots
        .iter()
        .cloned()
        .map(|root| std::thread::spawn(move || query_status(&root)))
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().unwrap_or(Err(ControlError::Unresponsive)))
        .collect()
}

/// How often a waiting caller's stop check runs.
const EXCHANGE_POLL: Duration = Duration::from_millis(50);

/// Stops at `deadline` with `Unresponsive`: status and control's wait.
fn by(deadline: Duration) -> impl FnMut() -> Option<ControlError> {
    let started = std::time::Instant::now();
    move || (started.elapsed() >= deadline).then_some(ControlError::Unresponsive)
}

/// Send `op` and wait for its reply, running `stop` every `EXCHANGE_POLL`;
/// its first `Some` ends the wait with that error.
///
/// The read runs on a helper thread so the caller's deadline holds on every
/// transport. An abandoned read is also woken through the stream's
/// [`ReadAbort`](super::read_endpoint::ReadAbort) (`shutdown(2)` on unix,
/// `CancelIoEx` on Windows, #206), so a wedged daemon never pins a thread in
/// a long-lived client (MCP) -- and the woken reader drops the stream, which
/// a daemon serving a write reads as its client leaving (#204).
pub(crate) fn exchange<O: DaemonOp, E: From<ControlError>>(
    mut stream: ReadStream,
    op: &O,
    mut stop: impl FnMut() -> Option<E>,
) -> Result<O::Reply, E>
where
    O::Reply: Send + 'static,
{
    write_op(&mut stream, op).map_err(|_| E::from(ControlError::Unresponsive))?;
    let abort = stream.read_abort();
    let reader_abort = abort.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let got = read_outcome::<O>(&mut stream);
        reader_abort.disarm();
        drop(stream);
        let _ = tx.send(got);
    });
    loop {
        match rx.recv_timeout(EXCHANGE_POLL) {
            Ok(got) => return got.map_err(E::from),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(E::from(ControlError::Incompatible))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(e) = stop() {
                    abort.abort();
                    return Err(e);
                }
            }
        }
    }
}

/// The reply frames for one op: an admission frame first when `O::ACKED`
/// (#204), so an EOF after it is `Lost`, not `Incompatible`.
fn read_outcome<O: DaemonOp>(stream: &mut ReadStream) -> Result<O::Reply, ControlError> {
    if O::ACKED {
        match read_reply::<_, ()>(stream) {
            Ok(Some(OpReply::Ok(()))) => {}
            Ok(Some(OpReply::Err(m))) => return Err(ControlError::Refused(m)),
            Ok(None) | Err(_) => return Err(ControlError::Incompatible),
        }
    }
    match read_reply::<_, O::Reply>(stream) {
        Ok(Some(OpReply::Ok(v))) => Ok(v),
        Ok(Some(OpReply::Err(m))) => Err(ControlError::Refused(m)),
        Ok(None) | Err(_) if O::ACKED => Err(ControlError::Lost),
        Ok(None) | Err(_) => Err(ControlError::Incompatible),
    }
}

/// What `watch-status` (CLI) and `get_watch_status` (MCP) print: one text,
/// so the two never drift.
pub fn describe_status(root: &Path, result: &Result<StatusReport, ControlError>) -> String {
    let shown = root.display();
    match result {
        Ok(report) => format!("Watcher active for {shown}\n{report}"),
        Err(ControlError::NoDaemon) => format!("No watcher running for {shown}."),
        Err(ControlError::Unresponsive) => {
            let lock = root.join(".infigraph").join("watch.lock");
            let holder = crate::lockfile::read_holder(&lock)
                .map(|h| format!("PID {} (role: {})", h.pid, h.role))
                .unwrap_or_else(|| "a process".to_string());
            format!(
                "Watcher for {shown}: {holder} holds watch.lock but is not answering on its \
                 socket (starting, or wedged). `infigraph daemon-stop` falls back to the stop \
                 sentinel."
            )
        }
        Err(e) => format!("Watcher for {shown}: {e}"),
    }
}
