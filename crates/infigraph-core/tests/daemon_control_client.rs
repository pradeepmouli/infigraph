//! The client half of #155's socket control, against fake listeners and a
//! real `ReadService` with a control port.

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use infigraph_core::daemon::control::{
    describe_status, query_status, query_status_many, send_control, ControlError, STATUS_DEADLINE,
};
use infigraph_core::daemon::control_port::ControlPort;
use infigraph_core::daemon::liveness::Liveness;
use infigraph_core::daemon::read_endpoint::{ReadEndpoint, ReadStream};
use infigraph_core::daemon::read_protocol::{WatchAction, WatchRole};
use infigraph_core::daemon::read_service::ReadService;

/// A listener on `root`'s real endpoint name that reads one frame and then
/// does whatever `then` says with the connection.
fn fake_daemon(
    root: &Path,
    then: impl Fn(ReadStream) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let listener = ReadEndpoint::for_root(root).bind().unwrap();
    std::thread::spawn(move || {
        while let Ok(Some(mut s)) = listener.accept_timeout(Duration::from_secs(5)) {
            let mut len = [0u8; 4];
            if s.read_exact(&mut len).is_ok() {
                let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
                let _ = s.read_exact(&mut body);
            }
            then(s);
        }
    })
}

#[test]
fn no_listener_and_no_watch_lock_is_no_daemon() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
    assert_eq!(query_status(dir.path()), Err(ControlError::NoDaemon));
    assert_eq!(
        send_control(dir.path(), WatchRole::Code, WatchAction::Stop),
        Err(ControlError::NoDaemon)
    );
}

#[test]
fn a_daemon_that_closes_without_replying_is_incompatible() {
    let dir = tempfile::tempdir().unwrap();
    let _fake = fake_daemon(dir.path(), drop);
    assert_eq!(query_status(dir.path()), Err(ControlError::Incompatible));
}

#[test]
fn a_daemon_that_never_replies_is_unresponsive_within_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let _fake = fake_daemon(dir.path(), |s| {
        std::thread::sleep(Duration::from_secs(3));
        drop(s);
    });
    let started = Instant::now();
    assert_eq!(query_status(dir.path()), Err(ControlError::Unresponsive));
    assert!(started.elapsed() < STATUS_DEADLINE + Duration::from_millis(500));
}

#[cfg(unix)]
#[test]
fn an_unresponsive_daemon_sees_the_client_hang_up() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let _fake = fake_daemon(dir.path(), move |mut s| {
        let mut buf = [0u8; 1];
        let started = Instant::now();
        let _ = s.read(&mut buf); // blocks until the client shuts the socket down
        tx.send(started.elapsed()).unwrap();
    });
    assert_eq!(query_status(dir.path()), Err(ControlError::Unresponsive));
    let waited = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the client must hang up");
    assert!(waited < Duration::from_secs(2));
}

#[test]
fn status_and_control_round_trip_against_a_real_service() {
    let dir = tempfile::tempdir().unwrap();
    let (port, rx) = ControlPort::new(1800, 60);
    let svc = ReadService::start_serving(
        dir.path(),
        Arc::new(|| None),
        None,
        2,
        Arc::new(Liveness::new()),
        Some(port),
    )
    .unwrap();
    let answer = std::thread::spawn(move || {
        rx.recv()
            .unwrap()
            .reply
            .send(Err("no doc-watch loop".into()))
            .unwrap();
    });
    let report = query_status(dir.path()).unwrap();
    assert_eq!(report.pid, std::process::id());
    assert!(describe_status(dir.path(), &Ok(report)).contains("clients leasing: 0"));
    assert_eq!(
        send_control(dir.path(), WatchRole::Docs, WatchAction::Stop),
        Err(ControlError::Refused("no doc-watch loop".into()))
    );
    answer.join().unwrap();
    svc.shutdown();
}

#[test]
fn many_unresponsive_daemons_cost_about_one_deadline_not_one_each() {
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let _fakes: Vec<_> = dirs
        .iter()
        .map(|d| {
            fake_daemon(d.path(), |s| {
                std::thread::sleep(Duration::from_secs(3));
                drop(s);
            })
        })
        .collect();
    let roots: Vec<_> = dirs.iter().map(|d| d.path().to_path_buf()).collect();
    let started = Instant::now();
    let results = query_status_many(&roots);
    assert!(results.iter().all(|r| *r == Err(ControlError::Unresponsive)));
    assert!(started.elapsed() < STATUS_DEADLINE * 2);
}

#[test]
fn describe_status_names_the_holder_of_an_unresponsive_daemons_lock() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
    let lock = dir.path().join(".infigraph").join("watch.lock");
    let _held = infigraph_core::lockfile::try_acquire(&lock, "test-daemon")
        .unwrap()
        .unwrap();
    let text = describe_status(dir.path(), &Err(ControlError::Unresponsive));
    assert!(text.contains("role: test-daemon"), "{text}");
    assert!(describe_status(dir.path(), &Err(ControlError::NoDaemon))
        .starts_with("No watcher running for"));
}
