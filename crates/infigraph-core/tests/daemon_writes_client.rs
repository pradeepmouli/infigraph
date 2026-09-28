//! The write client (#204) against a real read service with a stub
//! coordinator, and against fake listeners on the real endpoint.

use std::sync::Arc;
use std::time::{Duration, Instant};

use infigraph_core::daemon::control::ControlError;
use infigraph_core::daemon::coordinator_port::{CoordinatorPort, PortMsg, WriteReply};
use infigraph_core::daemon::liveness::Liveness;
use infigraph_core::daemon::read_endpoint::{ReadEndpoint, ReadStream};
use infigraph_core::daemon::read_protocol::{read_client_frame, write_reply, ClientFrame, OpReply};
use infigraph_core::daemon::read_service::ReadService;
use infigraph_core::daemon::writes::{sidecar_path, submit, WriteOpts};
use infigraph_core::daemon_protocol::{WriteRequest, WriteRequestCancelled, WriteResult};

fn opts(timeout: Duration) -> WriteOpts<'static> {
    WriteOpts {
        timeout,
        cancel: None,
    }
}

fn service(root: &std::path::Path) -> (ReadService, std::sync::mpsc::Receiver<PortMsg>) {
    let (port, rx) = CoordinatorPort::new(1800, 60);
    let svc = ReadService::start_serving(
        root,
        Arc::new(|| None),
        None,
        2,
        Arc::new(Liveness::new()),
        Some(port),
    )
    .unwrap();
    (svc, rx)
}

fn next_write(rx: &std::sync::mpsc::Receiver<PortMsg>) -> WriteReply {
    match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        PortMsg::Write { reply, .. } => reply,
        PortMsg::Control(_) => panic!("expected a write"),
    }
}

#[test]
fn a_write_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, rx) = service(dir.path());
    let answer = std::thread::spawn(move || {
        next_write(&rx).send(WriteResult::Ok {
            total_files: 1,
            indexed_files: 1,
        });
    });
    let r = submit(
        dir.path(),
        &WriteRequest::UpsertRepo {
            namespace: "n".into(),
        },
        opts(Duration::from_secs(5)),
    )
    .unwrap();
    assert!(matches!(r, WriteResult::Ok { total_files: 1, .. }));
    answer.join().unwrap();
    svc.shutdown();
}

#[test]
fn a_cancelled_write_returns_promptly_and_its_waiter_goes() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, rx) = service(dir.path());
    let token = tokio_util::sync::CancellationToken::new();
    let t = {
        let (root, token) = (dir.path().to_path_buf(), token.clone());
        std::thread::spawn(move || {
            submit(
                &root,
                &WriteRequest::FullReindex,
                WriteOpts {
                    timeout: Duration::from_secs(30),
                    cancel: Some(&token),
                },
            )
        })
    };
    // Held to the end either way: dropping it would answer the write. Only
    // unix can see the client leave (`peer_closed`), so only unix reads it.
    #[cfg_attr(not(unix), allow(unused_variables))]
    let reply = next_write(&rx);
    let cancelled_at = Instant::now();
    token.cancel();
    let err = t.join().unwrap().unwrap_err();
    assert!(
        err.downcast_ref::<WriteRequestCancelled>().is_some(),
        "{err:#}"
    );
    assert!(cancelled_at.elapsed() < Duration::from_millis(500));
    #[cfg(unix)]
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !reply.is_gone() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(reply.is_gone(), "closing the connection is the withdrawal");
    }
    svc.shutdown();
}

#[test]
fn no_daemon_is_no_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let err = submit(
        dir.path(),
        &WriteRequest::FullReindex,
        opts(Duration::from_secs(1)),
    )
    .unwrap_err();
    assert_eq!(
        err.downcast_ref::<ControlError>(),
        Some(&ControlError::NoDaemon)
    );
}

/// A fake daemon that reads the write's frame, then runs `then` on its
/// stream. A client leases the daemon before writing (`lease::in_use`), so
/// lease connections are parked -- held open, as a daemon holds them --
/// rather than mistaken for the write.
fn fake(
    root: &std::path::Path,
    then: impl FnOnce(&mut ReadStream) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let listener = ReadEndpoint::for_root(root).bind().unwrap();
    std::thread::spawn(move || {
        let mut leases = Vec::new();
        loop {
            let mut s = listener.accept().unwrap();
            match read_client_frame(&mut s) {
                Ok(ClientFrame::Attach(_)) => leases.push(s),
                _ => {
                    then(&mut s);
                    return;
                }
            }
        }
    })
}

#[test]
fn eof_before_admission_is_incompatible() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), |_| {});
    let err = submit(
        dir.path(),
        &WriteRequest::FullReindex,
        opts(Duration::from_secs(5)),
    )
    .unwrap_err();
    assert_eq!(
        err.downcast_ref::<ControlError>(),
        Some(&ControlError::Incompatible)
    );
    f.join().unwrap();
}

#[test]
fn eof_after_admission_is_lost_not_incompatible() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), |s| {
        write_reply(s, &OpReply::Ok(())).unwrap();
    });
    let err = submit(
        dir.path(),
        &WriteRequest::FullReindex,
        opts(Duration::from_secs(5)),
    )
    .unwrap_err();
    assert_eq!(
        err.downcast_ref::<ControlError>(),
        Some(&ControlError::Lost)
    );
    f.join().unwrap();
}

#[test]
fn an_admission_refusal_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), |s| {
        write_reply::<_, ()>(s, &OpReply::Err("the daemon is shutting down".into())).unwrap();
    });
    let err = submit(
        dir.path(),
        &WriteRequest::FullReindex,
        opts(Duration::from_secs(5)),
    )
    .unwrap_err();
    assert!(
        matches!(err.downcast_ref::<ControlError>(), Some(ControlError::Refused(m)) if m.contains("shutting down")),
        "{err:#}"
    );
    f.join().unwrap();
}

#[test]
fn a_silent_daemon_times_out_at_the_callers_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), |s| {
        write_reply(s, &OpReply::Ok(())).unwrap();
        std::thread::sleep(Duration::from_secs(2));
    });
    let started = Instant::now();
    let err = submit(
        dir.path(),
        &WriteRequest::FullReindex,
        opts(Duration::from_millis(300)),
    )
    .unwrap_err();
    assert!(err.to_string().contains("FullReindex"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(1));
    f.join().unwrap();
}

#[test]
fn sidecars_live_under_write_tmp_and_are_unique() {
    let dir = tempfile::tempdir().unwrap();
    let a = sidecar_path(dir.path(), "extractions.json");
    let b = sidecar_path(dir.path(), "extractions.json");
    assert_ne!(a, b);
    assert!(a.starts_with(dir.path().join(".infigraph").join("write-tmp")));
    assert!(a.to_string_lossy().ends_with(".extractions.json"));
    assert!(a.parent().unwrap().is_dir(), "created on demand");
}
