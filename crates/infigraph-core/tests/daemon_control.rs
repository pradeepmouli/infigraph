//! #155: a real write coordinator served over its socket.

use std::time::{Duration, Instant};

use infigraph_core::daemon::control::{query_status, send_control, ControlError};
use infigraph_core::daemon::read_protocol::{RoleState, WatchAction, WatchRole};

mod common;
use common::daemon::{start, start_with_docs, stop, ENV_LOCK};

#[test]
fn daemon_stop_over_the_socket_replies_ok_then_exits() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    assert_eq!(
        send_control(dir.path(), WatchRole::Daemon, WatchAction::Stop),
        Ok(())
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !d.handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        d.handle.is_finished(),
        "Daemon Stop must end the coordinator loop"
    );
    assert!(
        d.token.is_cancelled(),
        "a daemon stop must cancel background work"
    );
    d.handle.join().unwrap().unwrap();
}

#[test]
fn daemon_start_is_refused_without_stopping_the_loop() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    assert!(matches!(
        send_control(dir.path(), WatchRole::Daemon, WatchAction::Start),
        Err(ControlError::Refused(m)) if m.contains("only supports Stop/Restart")
    ));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!d.handle.is_finished());
    stop(d);
}

#[test]
fn docs_without_a_handle_is_refused_and_reported_not_owned() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    assert!(matches!(
        send_control(dir.path(), WatchRole::Docs, WatchAction::Stop),
        Err(ControlError::Refused(_))
    ));
    assert_eq!(query_status(dir.path()).unwrap().docs, RoleState::NotOwned);
    stop(d);
}

#[test]
fn code_state_goes_running_stopped_disabled() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    assert_eq!(query_status(dir.path()).unwrap().code, RoleState::Running);
    send_control(dir.path(), WatchRole::Code, WatchAction::Stop).unwrap();
    assert_eq!(query_status(dir.path()).unwrap().code, RoleState::Stopped);
    // Disable's contract: the caller persists the policy first, then tells the daemon.
    infigraph_core::watch::config::write_watch_policy(dir.path(), WatchRole::Code, false).unwrap();
    send_control(dir.path(), WatchRole::Code, WatchAction::Disable).unwrap();
    assert_eq!(query_status(dir.path()).unwrap().code, RoleState::Disabled);
    stop(d);
}

#[test]
fn a_control_round_trip_on_an_idle_daemon_beats_one_tick() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    let mut samples: Vec<Duration> = (0..5)
        .map(|_| {
            let t = Instant::now();
            send_control(dir.path(), WatchRole::Code, WatchAction::Start).unwrap();
            t.elapsed()
        })
        .collect();
    samples.sort();
    let median = samples[2];
    stop(d);
    // COORDINATOR_TICK is 200ms. Before #155 a control waited out the tick.
    assert!(
        median < Duration::from_millis(200),
        "median {median:?} of {samples:?}"
    );
}

#[test]
fn a_stalled_coordinator_still_answers_status_and_refuses_control_when_full() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let stall = dir.path().join("stall");
    std::env::set_var("INFIGRAPH_TEST_COORDINATOR_STALL_FILE", &stall);
    let d = start(dir.path());
    std::fs::write(&stall, b"").unwrap();
    std::thread::sleep(Duration::from_millis(400)); // let the loop reach the stall
    let root = dir.path().to_path_buf();
    let queued: Vec<_> = (0..infigraph_core::daemon::coordinator_port::PORT_QUEUE)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || send_control(&root, WatchRole::Code, WatchAction::Start))
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    assert!(query_status(dir.path()).is_ok());
    assert!(t.elapsed() < Duration::from_millis(500));
    let t = Instant::now();
    assert!(matches!(
        send_control(dir.path(), WatchRole::Code, WatchAction::Start),
        Err(ControlError::Refused(m)) if m.contains("busy")
    ));
    assert!(t.elapsed() < Duration::from_millis(500));
    std::fs::remove_file(&stall).unwrap();
    for q in queued {
        assert_eq!(q.join().unwrap(), Ok(()));
    }
    std::env::remove_var("INFIGRAPH_TEST_COORDINATOR_STALL_FILE");
    stop(d);
}

/// A pre-#155 client drops a file-drop `WatchControl`. The daemon must answer
/// it promptly with an error -- not honour it, and not leave the client
/// waiting out its 30s timeout.
#[test]
fn a_legacy_watch_control_request_file_gets_a_prompt_error() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    let requests = dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&requests).unwrap();
    // Exactly what a pre-#155 client wrote.
    infigraph_core::daemon_protocol::write_atomic(
        &requests.join("legacy.request"),
        r#"{"WatchControl":{"role":"Daemon","action":"Stop"}}"#,
    )
    .unwrap();
    let result = requests.join("legacy.result");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !result.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let reply = std::fs::read_to_string(&result).expect("a prompt reply, not a 30s client timeout");
    assert!(reply.contains("Err"), "{reply}");
    assert!(
        reply.contains("no longer accepts file-drop requests"),
        "{reply}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !d.handle.is_finished(),
        "a legacy stop must not be honoured"
    );
    stop(d);
}

/// A pre-#204 client drops any write as a file. The daemon answers it within
/// one sweep with an error naming its build, and removes it.
#[test]
fn a_legacy_write_request_file_gets_a_prompt_error_naming_the_build() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    let requests = dir.path().join(".infigraph").join("requests");
    std::fs::create_dir_all(&requests).unwrap();
    infigraph_core::daemon_protocol::write_atomic(
        &requests.join("1-2-3.request"),
        r#""FullReindex""#,
    )
    .unwrap();
    let result = requests.join("1-2-3.result");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !result.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let reply = std::fs::read_to_string(&result).expect("a reply within one sweep");
    assert!(
        reply.contains("no longer accepts file-drop requests"),
        "{reply}"
    );
    assert!(reply.contains(infigraph_core::build_hash()), "{reply}");
    assert!(!requests.join("1-2-3.request").exists());
    stop(d);
}

/// Records every docs action it is asked to perform.
struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<WatchAction>>>);

impl infigraph_core::daemon::DocsHandle for Recorder {
    fn control(&self, action: WatchAction) -> Result<(), String> {
        self.0.lock().unwrap().push(action);
        Ok(())
    }
    fn is_running(&self) -> bool {
        false
    }
    fn is_busy(&self) -> bool {
        false
    }
    fn index_docs(
        &self,
        _full: bool,
    ) -> Result<infigraph_core::daemon_protocol::DocIndexStats, String> {
        Err("the Recorder test double does not index".to_string())
    }
}

#[test]
fn docs_control_reaches_the_registered_docs_handle() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let d = start_with_docs(
        dir.path(),
        Some(std::sync::Arc::new(Recorder(received.clone()))),
    );
    assert_eq!(
        send_control(dir.path(), WatchRole::Docs, WatchAction::Start),
        Ok(())
    );
    assert_eq!(*received.lock().unwrap(), vec![WatchAction::Start]);
    stop(d);
}
