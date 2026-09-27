//! The client side of socket control and status (#155, #202). The only code
//! that sends `Status` or `Control` frames: the CLI, MCP, doctor and `ps` all
//! come through here.
//!
//! Neither call takes a lease or starts a daemon. Checking on a daemon must
//! never keep it alive.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
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
        }
    }
}

impl std::error::Error for ControlError {}

fn watch_lock_held(root: &Path) -> bool {
    super::lifecycle::daemon_is_alive(&root.join(".infigraph").join("watch.lock"))
}

fn not_connected(root: &Path) -> ControlError {
    if watch_lock_held(root) {
        ControlError::Unresponsive
    } else {
        ControlError::NoDaemon
    }
}

pub fn query_status(root: &Path) -> Result<StatusReport, ControlError> {
    let stream = ReadEndpoint::for_root(root)
        .connect()
        .map_err(|_| not_connected(root))?;
    exchange(stream, &StatusFrame::default(), STATUS_DEADLINE)
}

pub fn send_control(root: &Path, role: WatchRole, action: WatchAction) -> Result<(), ControlError> {
    let stream = connect_allowing_for_startup(root).map_err(|_| not_connected(root))?;
    exchange(
        stream,
        &ControlFrame {
            control: ControlRequest { role, action },
        },
        CONTROL_DEADLINE,
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

/// Send `op` and wait at most `deadline` for its one reply frame.
///
/// The read runs on a helper thread so the caller's deadline holds on every
/// transport. On unix a timed-out read is also woken with `shutdown(2)`, so
/// a wedged daemon never pins a thread in a long-lived client (MCP). The
/// handle sits behind a mutex the reader clears before dropping the stream:
/// never shut down an fd number that may since have been reused. On Windows
/// the timed-out reader thread stays blocked until the daemon closes the
/// pipe -- a known gap, tracked in #206.
fn exchange<O: DaemonOp>(
    mut stream: ReadStream,
    op: &O,
    deadline: Duration,
) -> Result<O::Reply, ControlError>
where
    O::Reply: Send + 'static,
{
    write_op(&mut stream, op).map_err(|_| ControlError::Unresponsive)?;
    #[cfg(unix)]
    let hangup = Arc::new(Mutex::new(stream.lease_shutdown()));
    #[cfg(not(unix))]
    let hangup = Arc::new(Mutex::new(None::<()>));
    let reader_hangup = hangup.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let got = read_reply::<_, O::Reply>(&mut stream);
        reader_hangup
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        drop(stream);
        let _ = tx.send(got);
    });
    match rx.recv_timeout(deadline) {
        Ok(Ok(Some(OpReply::Ok(v)))) => Ok(v),
        Ok(Ok(Some(OpReply::Err(msg)))) => Err(ControlError::Refused(msg)),
        Ok(Ok(None)) | Ok(Err(_)) => Err(ControlError::Incompatible),
        Err(_) => {
            #[cfg(unix)]
            if let Some(h) = hangup.lock().unwrap_or_else(|e| e.into_inner()).take() {
                h.shutdown();
            }
            #[cfg(not(unix))]
            let _ = hangup;
            Err(ControlError::Unresponsive)
        }
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
