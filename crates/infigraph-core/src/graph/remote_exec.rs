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

impl QueryExec for RemoteExec {
    fn query_rows(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        let endpoint = ReadEndpoint::for_root(&self.root);
        let mut stream = endpoint
            .connect()
            .with_context(|| "no daemon read service is listening for this project")?;
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
