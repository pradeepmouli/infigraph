//! Client-side `QueryExec` that runs queries on the daemon's read service.
//!
//! Deliberately implements the same trait `LocalExec` does, so `GraphQuery`
//! -- and therefore every read query in the codebase -- runs unchanged over
//! either. Re-implementing those queries for a remote backend would
//! duplicate all 1045 lines of them.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::daemon::read_endpoint::ReadEndpoint;
use crate::daemon::read_protocol::{collect_rows, write_request, ReadRequest, Store};
use crate::graph::query_exec::QueryExec;

/// How many rows the service batches into one frame. Chunking is what keeps
/// a large result off one enormous allocation on both sides; the exact size
/// is not load-bearing.
const CHUNK_SIZE: usize = 1024;

pub struct RemoteExec {
    root: PathBuf,
    store: Store,
}

impl RemoteExec {
    /// Read the code graph for `root` through its daemon.
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            store: Store::Graph,
        }
    }

    /// Read the document store for `root` through its daemon. `search` with
    /// `scope='all'` touches both stores in one call.
    pub fn for_docs(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            store: Store::Docs,
        }
    }
}

/// How long to keep trying while a daemon is demonstrably alive but has not
/// bound its read endpoint yet.
///
/// The CLI takes `watch.lock` -- every caller's "the daemon is ready" signal
/// -- before `run_write_coordinator` is entered, so a client that starts a
/// daemon and immediately reads can arrive first. Generous because the
/// coordinator builds the bundled language registry on the way, which costs
/// seconds in a debug build.
const DAEMON_STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

impl RemoteExec {
    /// Connect, tolerating a daemon that is starting but not yet listening.
    ///
    /// The grace period applies *only* while `watch.lock` says a daemon is
    /// alive. With no daemon there is nothing to wait for, so the error is
    /// immediate -- a CLI run with the daemon down must not hang for 30s
    /// before reporting it.
    fn connect_allowing_for_startup(&self) -> Result<crate::daemon::read_endpoint::ReadStream> {
        let endpoint = ReadEndpoint::for_root(&self.root);
        let mut last = match endpoint.connect() {
            Ok(stream) => return Ok(stream),
            Err(e) => e,
        };
        let lock = self.root.join(".infigraph").join("watch.lock");
        let deadline = std::time::Instant::now() + DAEMON_STARTUP_GRACE;
        while std::time::Instant::now() < deadline
            && crate::daemon::lifecycle::daemon_is_alive(&lock)
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
            match endpoint.connect() {
                Ok(stream) => return Ok(stream),
                Err(e) => last = e,
            }
        }
        Err(last).with_context(|| "no daemon read service is listening for this project")
    }
}

impl QueryExec for RemoteExec {
    fn query_rows(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        let mut stream = self.connect_allowing_for_startup()?;
        write_request(
            &mut stream,
            &ReadRequest {
                store: self.store,
                query: cypher.to_string(),
                params: vec![],
                chunk_size: CHUNK_SIZE,
            },
        )?;
        collect_rows(&mut stream)
    }
}
