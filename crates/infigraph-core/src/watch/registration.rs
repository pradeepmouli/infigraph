//! Watcher registration that a stop never has to wait for.
//!
//! Registering a `notify` watcher blocks: on macOS every `watch()` call
//! restarts the FSEvents stream, an RPC to the system-wide `fseventsd`, and
//! with `fseventsd` backed up (dozens of watchers and a build storm on one
//! machine) a single one was seen stalling for over a minute. Run inline,
//! that stall sits between a stop and the watcher's exit -- the code
//! producer's `code_watch.stop()` held `watch.lock` for as long as
//! `fseventsd` took, and a doc watcher's stop (or the daemon's shutdown)
//! waited the same way.
//!
//! [`Registration::start`] therefore runs the registration on a thread of
//! its own. The async code producer waits for it with
//! [`Registration::unless_cancelled`], the synchronous doc watcher with
//! [`Registration::unless_stopped`]; either gives up as soon as it is told
//! to stop, and the thread drops whatever it built whenever it finishes.

use std::path::Path;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// How often [`Registration::unless_stopped`] checks for a stop.
const STOP_POLL: Duration = Duration::from_millis(20);

/// A registration running on its own thread.
pub struct Registration<T> {
    rx: oneshot::Receiver<anyhow::Result<T>>,
    spawn_err: Option<std::io::Error>,
}

impl<T: Send + 'static> Registration<T> {
    /// Runs `register` on a thread of its own. `root` is the watched root,
    /// used only by the test stall hook below.
    pub fn start(
        root: &Path,
        register: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
    ) -> Self {
        let (tx, rx) = oneshot::channel();
        // Test-only: hold registration while `root/<this>` exists, standing
        // in for a stalled `fseventsd`. Root-relative, so it cannot stall
        // another test's watcher. `<this>.stalled` tells the test the
        // registration has reached the stall.
        let stall = std::env::var_os("INFIGRAPH_TEST_WATCH_REGISTER_STALL_FILE")
            .map(|stall| root.join(stall));
        let spawned = std::thread::Builder::new()
            .name("infigraph-watch-register".into())
            .spawn(move || {
                if let Some(stall) = stall.filter(|s| s.exists()) {
                    let mut marker = stall.clone().into_os_string();
                    marker.push(".stalled");
                    let _ = std::fs::write(marker, "");
                    while stall.exists() {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
                let _ = tx.send(register());
            });
        Self {
            rx,
            spawn_err: spawned.err(),
        }
    }

    /// The registration's result, or `None` once `token` is cancelled.
    pub async fn unless_cancelled(self, token: &CancellationToken) -> Option<anyhow::Result<T>> {
        if let Some(e) = self.spawn_err {
            return Some(Err(e.into()));
        }
        tokio::select! {
            _ = token.cancelled() => None,
            built = self.rx => Some(built.unwrap_or_else(|_| Err(thread_lost()))),
        }
    }

    /// The registration's result, or `None` once `stopped` returns true.
    /// For a synchronous caller; blocks the calling thread.
    pub fn unless_stopped(
        mut self,
        mut stopped: impl FnMut() -> bool,
    ) -> Option<anyhow::Result<T>> {
        if let Some(e) = self.spawn_err {
            return Some(Err(e.into()));
        }
        loop {
            match self.rx.try_recv() {
                Ok(built) => return Some(built),
                Err(oneshot::error::TryRecvError::Closed) => return Some(Err(thread_lost())),
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
            if stopped() {
                return None;
            }
            std::thread::sleep(STOP_POLL);
        }
    }
}

fn thread_lost() -> anyhow::Error {
    anyhow::anyhow!("watcher registration thread exited without a result")
}
