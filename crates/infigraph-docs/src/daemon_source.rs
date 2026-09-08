//! The document-store half of the daemon's read service.
//!
//! `infigraph-core` cannot name `DocStore` (this crate depends on core, not
//! the reverse), so the daemon holds a `RowSource` closure instead and this
//! module supplies it.

use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};

use anyhow::{anyhow, Result};

use crate::query::DocQuery;
use crate::store::DocStore;

type Reply = mpsc::Sender<Result<Vec<Vec<String>>>>;
type Job = (String, Reply);

/// Open `docs.kuzu` once and return a `RowSource` that reads through it.
///
/// The store lives on its own thread rather than behind an `Arc` shared
/// with the read service's worker pool, because `DocStore` holds the
/// process-wide `DB_LOCK` as a `MutexGuard` and is therefore `!Send`.
///
/// Opening a fresh `DocStore` per request would sidestep that, and would be
/// wrong for the same reason the graph path refuses it: a second `Database`
/// on one file cannot see the writer's uncommitted WAL, so it serves stale
/// or empty rows with no error (#149, and `tests/read_service.rs` pins the
/// graph-side equivalent). One store, one thread, one `Database`.
///
/// The cost is that document reads serialise on that thread. Acceptable:
/// documents are far lower volume than the code graph, and this is the
/// trade that keeps the invariant.
pub fn daemon_row_source(root: &Path) -> Result<infigraph_core::daemon::read_service::RowSource> {
    let path = root.join(".infigraph").join("docs.kuzu");
    let (tx, rx) = mpsc::channel::<Job>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

    std::thread::Builder::new()
        .name("infigraph-docs-reads".to_string())
        .spawn(move || {
            let store = match DocStore::open(&path) {
                Ok(store) => {
                    let _ = ready_tx.send(Ok(()));
                    store
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            // Ends when the last sender drops, i.e. when the daemon drops
            // the `RowSource`.
            for (cypher, reply) in rx {
                let _ = reply.send(run_one(&store, &cypher));
            }
        })?;

    ready_rx
        .recv()
        .map_err(|_| anyhow!("document read thread exited before reporting readiness"))??;

    // `Mutex` because a `RowSource` must be `Sync` and `mpsc::Sender` is not.
    let tx = Mutex::new(tx);
    Ok(Arc::new(move |cypher: &str| {
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.lock()
            .unwrap_or_else(|e| e.into_inner())
            .send((cypher.to_string(), reply_tx))
            .map_err(|_| anyhow!("the document read thread is gone"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("the document read thread dropped the reply"))?
    }))
}

/// The guard runs here, not in the read service: this side holds the
/// connection, and the verdict must come from the database's own parser.
fn run_one(store: &DocStore, cypher: &str) -> Result<Vec<Vec<String>>> {
    let conn = store.connection()?;
    infigraph_core::daemon::read_guard::ensure_read_only(&conn, cypher)?;
    DocQuery::new(&conn).raw_query(cypher)
}
