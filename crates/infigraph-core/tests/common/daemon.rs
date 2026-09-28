//! An in-process daemon (a real write coordinator serving its socket) for
//! integration tests: #155's control tests and #204's write tests.

use std::path::Path;
use std::time::{Duration, Instant};

use infigraph_core::daemon::control::send_control;
use infigraph_core::daemon::read_protocol::{WatchAction, WatchRole};

pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct Daemon {
    pub handle: std::thread::JoinHandle<anyhow::Result<()>>,
    pub token: tokio_util::sync::CancellationToken,
    pub stop_tx: std::sync::mpsc::Sender<()>,
}

pub fn start(root: &Path) -> Daemon {
    start_with_docs(root, None)
}

pub fn start_with_docs(
    root: &Path,
    docs: Option<std::sync::Arc<dyn infigraph_core::daemon::DocsHandle>>,
) -> Daemon {
    std::fs::write(root.join("main.py"), "def main():\n    pass\n").unwrap();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let token = tokio_util::sync::CancellationToken::new();
    let t = token.clone();
    let r = root.to_path_buf();
    let handle = std::thread::spawn(move || {
        infigraph_core::daemon::run_write_coordinator(
            &r,
            || Ok(infigraph_languages::bundled_registry().unwrap()),
            50,
            stop_rx,
            |_| {},
            0,
            None::<fn(&infigraph_core::IndexResult)>,
            true,
            None,
            &t,
            docs,
            None,
        )
    });
    // The endpoint binds before the registry build; control waits until the
    // loop is taking requests, which is after that build.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match send_control(root, WatchRole::Code, WatchAction::Start) {
            Ok(()) => break,
            Err(e) if Instant::now() > deadline => panic!("daemon never took control: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    Daemon {
        handle,
        token,
        stop_tx,
    }
}

pub fn stop(d: Daemon) {
    d.token.cancel();
    let _ = d.stop_tx.send(());
    let _ = d.handle.join();
}
