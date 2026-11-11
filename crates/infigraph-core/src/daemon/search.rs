//! Search operations use the existing read endpoint and its bounded pool.
//! Only the query and rendered matches cross the process boundary.

use super::control::{exchange, ControlError};
use super::read_protocol::{DocSearchFrame, SearchFrame};
use crate::search::service::{DocSearchRequest, DocSearchResult, SearchReply, SearchRequest};
use anyhow::Result;
use std::path::Path;
use std::time::{Duration, Instant};

fn request<O: super::read_protocol::DaemonOp>(root: &Path, op: &O) -> Result<O::Reply>
where
    O::Reply: Send + 'static,
{
    let _use = super::lease::in_use(root);
    let deadline = Instant::now() + super::read_endpoint::DAEMON_STARTUP_GRACE;
    loop {
        let stream = super::read_endpoint::connect_allowing_for_startup(root)?;
        let started = Instant::now();
        let result: Result<O::Reply, ControlError> = exchange(stream, op, || {
            (started.elapsed() >= Duration::from_secs(120)).then_some(ControlError::Unresponsive)
        });
        match result {
            Err(ControlError::Refused(ref error))
                if error.contains(super::read_service::NOT_READY)
                    && Instant::now() < deadline
                    && super::lifecycle::daemon_is_alive(&root.join(".infigraph/watch.lock")) =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            other => return other.map_err(Into::into),
        }
    }
}

pub fn symbols(root: &Path, search: SearchRequest) -> Result<SearchReply> {
    request(root, &SearchFrame { search })
}

pub fn docs(root: &Path, doc_search: DocSearchRequest) -> Result<Vec<DocSearchResult>> {
    request(root, &DocSearchFrame { doc_search })
}

/// Attached but unused clients get the idle TTL; the last detached client
/// releases state sooner. External indexers can run for minutes and must not
/// keep unused search data alive. Owned results and nonblocking cache locks
/// protect any background operation still using its own data.
pub fn maintain_caches(liveness: &super::liveness::Liveness, timeout: Duration) -> usize {
    if liveness.reads_in_flight() {
        return 0;
    }
    crate::search::service::maintain_caches(timeout, liveness.no_clients_or_reads())
}
