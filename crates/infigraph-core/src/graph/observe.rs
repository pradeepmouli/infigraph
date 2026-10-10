//! How an observer (`doctor`, `verify`) reads a project's graph.
//!
//! A second process opening a graph beside a live writer is the shape
//! `tests/concurrent_writer_reader.rs` crashes in (phase=open). A project
//! with a live daemon is therefore read through that daemon's read service,
//! the same path every ordinary read takes; only a project with none is
//! opened directly, read-only. An observer never starts a daemon (`doctor`
//! stays observe-only), and it never falls back to a direct open once it has
//! seen a live one: a failed routed read is reported as such, because the
//! fallback is exactly the open this type exists to avoid.

use std::path::Path;

use anyhow::Result;

use super::query_exec::{LocalExec, QueryExec};
use super::remote_exec::RemoteExec;
use super::{GraphQuery, GraphStore};

/// A project's graph as an observer sees it.
pub enum ObservedGraph {
    /// Served by the project's live daemon.
    Daemon(RemoteExec),
    /// No daemon: opened here, read-only.
    Direct(GraphStore),
}

impl ObservedGraph {
    /// The daemon-routed view when `root` has a live daemon, else `None`.
    /// Never starts one, and takes no lease on it: an observer must not keep
    /// a daemon alive (`doctor --global` would otherwise hold every live
    /// daemon for `client_release_secs`). A read still counts as activity on
    /// the daemon's idle clock.
    pub fn via_live_daemon(root: &Path) -> Option<Self> {
        crate::daemon::lifecycle::daemon_is_alive(&root.join(".infigraph").join("watch.lock"))
            .then(|| Self::Daemon(RemoteExec::observer(root)))
    }

    /// Observe `root`'s graph at `graph_path`: through its live daemon, or
    /// else by a direct read-only open (whose error is returned untouched, so
    /// callers keep classifying transient versus corrupt).
    pub fn open(root: &Path, graph_path: &Path) -> Result<Self> {
        match Self::via_live_daemon(root) {
            Some(routed) => Ok(routed),
            None => Ok(Self::Direct(GraphStore::open_read_only(graph_path)?)),
        }
    }

    pub fn is_daemon(&self) -> bool {
        matches!(self, Self::Daemon(_))
    }

    /// Run `f` over the one query surface both variants share.
    pub fn with_query<T>(&self, f: impl FnOnce(&GraphQuery<&dyn QueryExec>) -> T) -> Result<T> {
        match self {
            Self::Daemon(remote) => {
                let exec: &dyn QueryExec = remote;
                Ok(f(&GraphQuery::new_with(exec)))
            }
            Self::Direct(store) => {
                let conn = store.connection()?;
                let local = LocalExec::new(&conn);
                let exec: &dyn QueryExec = &local;
                Ok(f(&GraphQuery::new_with(exec)))
            }
        }
    }

    /// Run `f` over the raw executor (for measurements that are not
    /// `GraphQuery` methods, such as page accounting).
    pub fn with_exec<T>(&self, f: impl FnOnce(&dyn QueryExec) -> T) -> Result<T> {
        match self {
            Self::Daemon(remote) => Ok(f(remote)),
            Self::Direct(store) => {
                let conn = store.connection()?;
                Ok(f(&LocalExec::new(&conn)))
            }
        }
    }
}
