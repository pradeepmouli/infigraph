//! Framing for the daemon read service.
//!
//! Length-prefixed JSON frames. The explicit `End` frame is the point: a
//! stream that stops without one is a truncation, and the client must be
//! able to tell that from a query that legitimately returned no rows.

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Which of the daemon's two stores a request is for. `search` with
/// `scope='all'` touches both in one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Store {
    Graph,
    Docs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadRequest {
    pub store: Store,
    pub query: String,
    pub params: Vec<(String, serde_json::Value)>,
    pub chunk_size: usize,
}

/// A lease (#38, #124): sent as the first and only frame on a connection the
/// client holds for as long as it uses the daemon. The daemon answers nothing;
/// the connection exists so its EOF -- which the kernel delivers even when the
/// client is SIGKILLed -- tells the daemon a user went away.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attach {
    pub attach_pid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchRole {
    Code,
    Docs,
    /// Full-process stop/restart -- a per-role alternative to the
    /// undecorated `watch.stop` sentinel, which remains the mechanism the
    /// legacy `watch-stop` CLI alias and `worktree_commands.rs` use (see
    /// docs/superpowers/specs/2026-08-21-daemon-watch-command-split-design.md,
    /// "Crossing the process boundary").
    Daemon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchAction {
    Start,
    Stop,
    Enable,
    Disable,
    Restart,
}

impl WatchAction {
    /// The one mapping from an action to stop/start calls. `Enable`/`Disable`
    /// differ from `Start`/`Stop` only in whether the *caller* also wrote the
    /// persisted policy, so their effect on a live loop is the same.
    pub fn drive(self, mut stop: impl FnMut(), mut start: impl FnMut()) {
        match self {
            WatchAction::Stop | WatchAction::Disable => stop(),
            WatchAction::Start | WatchAction::Enable => start(),
            WatchAction::Restart => {
                stop();
                start();
            }
        }
    }
}

/// Asks the daemon how it is doing (#155, #202). Answered from memory on the
/// read service, never through the coordinator, so it answers even while the
/// coordinator is busy or stuck.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusQuery {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusFrame {
    pub status: StatusQuery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequest {
    pub role: WatchRole,
    pub action: WatchAction,
}

/// Starts, stops, enables, disables or restarts a watch role, or stops the
/// daemon. Replaces the file-drop `WriteRequest::WatchControl` (#155).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlFrame {
    pub control: ControlRequest,
}

/// The one reply frame `Status` and `Control` send. `Read` keeps its
/// streaming `ReadFrame`s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OpReply<T> {
    Ok(T),
    Err(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoleState {
    /// The daemon is up but its loop has not published yet (the endpoint
    /// binds seconds before the registry build finishes).
    Starting,
    Running,
    /// Stopped over control, or the loop ended by itself; policy still on.
    Stopped,
    /// The persisted policy is off, and the role is not running.
    Disabled,
    /// This daemon has no loop for the role.
    NotOwned,
}

impl std::fmt::Display for RoleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RoleState::Starting => "starting",
            RoleState::Running => "running",
            RoleState::Stopped => "stopped",
            RoleState::Disabled => "disabled",
            RoleState::NotOwned => "not owned by this daemon",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub pid: u32,
    pub build: String,
    pub leases: usize,
    /// `None` while any lease is held (`Liveness::idle_for`).
    pub idle_secs: Option<u64>,
    /// 0 = idle exit disabled.
    pub grace_secs: u64,
    /// How often the coordinator evaluates the idle exit.
    pub idle_check_secs: u64,
    /// A drain, full reindex or SCIP run is in flight; it defers the idle exit.
    pub work_in_flight: bool,
    pub code: RoleState,
    pub docs: RoleState,
}

impl std::fmt::Display for StatusReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Daemon PID {} (build {})", self.pid, self.build)?;
        writeln!(f, "  clients leasing: {}", self.leases)?;
        match (self.idle_secs, self.grace_secs) {
            (None, _) => writeln!(f, "  idle: no (leased)")?,
            (Some(idle), 0) => writeln!(f, "  idle: {idle}s (idle exit disabled)")?,
            (Some(_), _) if self.work_in_flight => {
                writeln!(f, "  idle: exit deferred by in-flight work")?
            }
            (Some(idle), grace) => {
                writeln!(f, "  idle: {idle}s, exits after {grace}s without a client")?
            }
        }
        writeln!(f, "  code watching: {}", self.code)?;
        write!(f, "  doc watching: {}", self.docs)
    }
}

/// An operation a client can send. The dispatcher, not each handler, applies
/// the liveness rule, so a new op cannot forget it (#155).
pub trait DaemonOp: Serialize + DeserializeOwned {
    type Reply: Serialize + DeserializeOwned;
    /// Whether serving this op counts as the daemon being used. No default:
    /// every op must answer.
    const KEEPS_ALIVE: bool;
}

impl DaemonOp for ReadRequest {
    // Streamed as `ReadFrame`s, not one `OpReply`; named for completeness.
    type Reply = Vec<Vec<String>>;
    const KEEPS_ALIVE: bool = true;
}

impl DaemonOp for StatusFrame {
    type Reply = StatusReport;
    const KEEPS_ALIVE: bool = false;
}

impl DaemonOp for ControlFrame {
    type Reply = ();
    const KEEPS_ALIVE: bool = false;
}

/// The first frame of every connection. `untagged` keeps a `ReadRequest`
/// byte-identical on the wire, so clients from before leases still parse;
/// `Attach`'s field is one no `ReadRequest` has, so the two never collide.
/// `Status` and `Control` are single-key objects whose key no other frame
/// has (#155).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientFrame {
    Attach(Attach),
    Read(ReadRequest),
    Status(StatusFrame),
    Control(ControlFrame),
}

impl ClientFrame {
    /// Whether serving this frame counts as activity. `None` for `Attach`,
    /// which is the lease itself and has its own accounting. Exhaustive on
    /// purpose: a new variant does not compile until it answers.
    pub fn keeps_alive(&self) -> Option<bool> {
        match self {
            ClientFrame::Attach(_) => None,
            ClientFrame::Read(_) => Some(ReadRequest::KEEPS_ALIVE),
            ClientFrame::Status(_) => Some(StatusFrame::KEEPS_ALIVE),
            ClientFrame::Control(_) => Some(ControlFrame::KEEPS_ALIVE),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReadFrame {
    Rows(Vec<Vec<String>>),
    End,
    Error(String),
}

fn write_len_prefixed<W: Write>(w: &mut W, bytes: &[u8]) -> Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

pub(crate) fn read_len_prefixed<R: Read>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .map_err(|_| anyhow::anyhow!("truncated frame: expected {len} bytes"))?;
    Ok(Some(body))
}

pub fn write_op<W: Write, O: DaemonOp>(w: &mut W, op: &O) -> Result<()> {
    write_len_prefixed(w, &serde_json::to_vec(op)?)
}

pub fn write_request<W: Write>(w: &mut W, req: &ReadRequest) -> Result<()> {
    write_op(w, req)
}

pub fn write_reply<W: Write, T: Serialize>(w: &mut W, reply: &OpReply<T>) -> Result<()> {
    write_len_prefixed(w, &serde_json::to_vec(reply)?)
}

/// `None` when the stream ends before any frame: the daemon closed without
/// answering, which a client reads as "could not parse what I sent".
pub fn read_reply<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<Option<OpReply<T>>> {
    match read_len_prefixed(r)? {
        None => Ok(None),
        Some(body) => Ok(Some(serde_json::from_slice(&body)?)),
    }
}

pub fn write_attach<W: Write>(w: &mut W, pid: u32) -> Result<()> {
    write_len_prefixed(w, &serde_json::to_vec(&Attach { attach_pid: pid })?)
}

pub fn read_client_frame<R: Read>(r: &mut R) -> Result<ClientFrame> {
    let body = read_len_prefixed(r)?.ok_or_else(|| anyhow::anyhow!("no request"))?;
    serde_json::from_slice(&body).map_err(|untagged| {
        // An untagged enum's own error ("did not match any variant") names
        // nothing. Most malformed frames are reads, so report the field-level
        // error a `ReadRequest` parse gives, when there is one (#203 M5).
        match serde_json::from_slice::<ReadRequest>(&body) {
            Err(field) => anyhow::anyhow!("malformed request frame: {field}"),
            Ok(_) => untagged.into(),
        }
    })
}

pub fn write_frame<W: Write>(w: &mut W, frame: &ReadFrame) -> Result<()> {
    write_len_prefixed(w, &serde_json::to_vec(frame)?)
}

pub fn read_frame<R: Read>(r: &mut R) -> Result<Option<ReadFrame>> {
    match read_len_prefixed(r)? {
        None => Ok(None),
        Some(body) => Ok(Some(serde_json::from_slice(&body)?)),
    }
}

/// Read frames until `End`. A stream that ends first is an error.
pub fn collect_rows<R: Read>(r: &mut R) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    loop {
        match read_frame(r)? {
            Some(ReadFrame::Rows(mut chunk)) => rows.append(&mut chunk),
            Some(ReadFrame::End) => return Ok(rows),
            Some(ReadFrame::Error(msg)) => anyhow::bail!("read service error: {msg}"),
            None => anyhow::bail!(
                "truncated result stream: the daemon closed the connection before sending \
                 an end-of-results frame. This is not an empty result set."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips() {
        let req = ReadRequest {
            store: Store::Graph,
            query: "MATCH (f:File) RETURN f.id".to_string(),
            params: vec![],
            chunk_size: 1024,
        };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let ClientFrame::Read(got) = read_client_frame(&mut buf.as_slice()).unwrap() else {
            panic!("a ReadRequest must parse as ClientFrame::Read");
        };
        assert_eq!(got.query, req.query);
        assert_eq!(got.store, Store::Graph);
    }

    #[test]
    fn an_attach_round_trips() {
        let mut buf = Vec::new();
        write_attach(&mut buf, 4242).unwrap();
        let ClientFrame::Attach(a) = read_client_frame(&mut buf.as_slice()).unwrap() else {
            panic!("expected Attach");
        };
        assert_eq!(a.attach_pid, 4242);
    }

    /// Wire compatibility: an old client's request JSON, written by hand, must
    /// still parse as a read. If this breaks, every pre-lease client breaks.
    #[test]
    fn an_old_client_request_still_parses_as_a_read() {
        let json = br#"{"store":"Graph","query":"RETURN 1","params":[],"chunk_size":8}"#;
        let mut buf = (json.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(json);
        assert!(matches!(
            read_client_frame(&mut buf.as_slice()).unwrap(),
            ClientFrame::Read(_)
        ));
    }

    /// A stream that ends without an `End` frame must be an error, never an
    /// empty result. Returning zero rows silently is the "0-symbol graph
    /// served as healthy" failure `Infigraph::init` already shipped once.
    #[test]
    fn a_truncated_stream_is_an_error_not_an_empty_result() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["a".to_string()]])).unwrap();
        // Deliberately no End frame, and cut mid-frame.
        buf.truncate(buf.len() - 2);

        let err = collect_rows(&mut buf.as_slice())
            .expect_err("a truncated stream must not read as a complete empty result");
        assert!(err.to_string().contains("truncated"), "unexpected: {err}");
    }

    #[test]
    fn a_complete_stream_collects_all_rows() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["a".to_string()]])).unwrap();
        write_frame(&mut buf, &ReadFrame::Rows(vec![vec!["b".to_string()]])).unwrap();
        write_frame(&mut buf, &ReadFrame::End).unwrap();
        let rows = collect_rows(&mut buf.as_slice()).unwrap();
        assert_eq!(rows, vec![vec!["a".to_string()], vec!["b".to_string()]]);
    }

    fn parse(bytes: &[u8]) -> ClientFrame {
        let mut framed = Vec::new();
        framed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        framed.extend_from_slice(bytes);
        read_client_frame(&mut framed.as_slice()).unwrap()
    }

    #[test]
    fn a_status_frame_round_trips_and_is_not_a_read_or_attach() {
        let mut buf = Vec::new();
        write_op(&mut buf, &StatusFrame::default()).unwrap();
        assert!(matches!(
            read_client_frame(&mut buf.as_slice()).unwrap(),
            ClientFrame::Status(_)
        ));
    }

    #[test]
    fn a_control_frame_round_trips_with_its_role_and_action() {
        let mut buf = Vec::new();
        let op = ControlFrame {
            control: ControlRequest {
                role: WatchRole::Docs,
                action: WatchAction::Restart,
            },
        };
        write_op(&mut buf, &op).unwrap();
        let ClientFrame::Control(got) = read_client_frame(&mut buf.as_slice()).unwrap() else {
            panic!("expected Control");
        };
        assert_eq!(got.control, op.control);
    }

    #[test]
    fn todays_read_and_attach_bytes_still_parse_as_before() {
        assert!(matches!(
            parse(br#"{"attach_pid":7}"#),
            ClientFrame::Attach(_)
        ));
        assert!(matches!(
            parse(br#"{"store":"Graph","query":"RETURN 1","params":[],"chunk_size":8}"#),
            ClientFrame::Read(_)
        ));
        assert!(matches!(parse(br#"{"status":{}}"#), ClientFrame::Status(_)));
        assert!(matches!(
            parse(br#"{"control":{"role":"Code","action":"Stop"}}"#),
            ClientFrame::Control(_)
        ));
    }

    #[test]
    fn only_reads_keep_the_daemon_alive() {
        assert_eq!(parse(br#"{"attach_pid":7}"#).keeps_alive(), None);
        assert_eq!(
            parse(br#"{"store":"Graph","query":"RETURN 1","params":[],"chunk_size":8}"#)
                .keeps_alive(),
            Some(true)
        );
        assert_eq!(parse(br#"{"status":{}}"#).keeps_alive(), Some(false));
        assert_eq!(
            parse(br#"{"control":{"role":"Code","action":"Stop"}}"#).keeps_alive(),
            Some(false)
        );
    }

    #[test]
    fn an_op_reply_round_trips_both_ways_and_eof_reads_as_none() {
        let report = StatusReport {
            pid: 1,
            build: "abc".into(),
            leases: 2,
            idle_secs: None,
            grace_secs: 1800,
            idle_check_secs: 60,
            work_in_flight: false,
            code: RoleState::Running,
            docs: RoleState::NotOwned,
        };
        let mut buf = Vec::new();
        write_reply(&mut buf, &OpReply::Ok(report.clone())).unwrap();
        let got: OpReply<StatusReport> = read_reply(&mut buf.as_slice()).unwrap().unwrap();
        assert!(matches!(got, OpReply::Ok(r) if r == report));

        let mut buf = Vec::new();
        write_reply::<_, ()>(&mut buf, &OpReply::Err("busy".into())).unwrap();
        let got: OpReply<()> = read_reply(&mut buf.as_slice()).unwrap().unwrap();
        assert!(matches!(got, OpReply::Err(m) if m == "busy"));

        let empty: &[u8] = &[];
        assert!(read_reply::<_, ()>(&mut &*empty).unwrap().is_none());
    }
}

#[cfg(test)]
mod malformed_frame_tests {
    use super::*;

    /// #203 M5: a malformed read reports serde's field-level error, not the
    /// untagged enum's "did not match any variant".
    #[test]
    fn a_malformed_read_reports_the_field_error() {
        let body = br#"{"store":"Graph","query":1,"params":[],"chunk_size":8}"#;
        let mut framed = (body.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(body);
        let err = read_client_frame(&mut framed.as_slice())
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid type"), "{err}");
        assert!(!err.contains("did not match any variant"), "{err}");
    }
}
