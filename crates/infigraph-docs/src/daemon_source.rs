//! The document-store half of the daemon's read service.
//!
//! `infigraph-core` cannot name `DocStore` (this crate depends on core, not
//! the reverse), so the daemon holds a `RowSource` closure instead and this
//! module supplies it.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::query::DocQuery;
use crate::store::DocStore;

/// A `RowSource` that opens `docs.kuzu` per request.
///
/// Per request, deliberately, and this is the opposite of the graph side --
/// which insists on one long-lived `Arc<GraphStore>` because a second
/// `Database` cannot see the live writer's uncommitted WAL (#149). The
/// asymmetry is real: on the docs side there *is* no long-lived writer. The
/// doc watcher opens a `DocIndex` per reindex and drops it (`watch.rs`), so
/// everything it wrote is committed by the time it lets go.
///
/// Holding one open here instead would deadlock the daemon. `DocStore::open`
/// takes the process-wide `DB_LOCK` and holds the guard for the store's
/// lifetime, so a store kept for the daemon's lifetime would block the doc
/// watcher's next `DocIndex::init()` forever.
///
/// Keeping the store inside the closure also means it never crosses a thread
/// boundary, which matters because that `MutexGuard` makes `DocStore`
/// `!Send`.
pub fn daemon_row_source(root: &Path) -> Result<infigraph_core::daemon::read_service::RowSource> {
    let path = root.join(".infigraph").join("docs.kuzu");
    // Fail fast if the store cannot be opened at all, rather than at the
    // first read: the daemon logs this once and serves the graph only.
    drop(DocStore::open(&path)?);

    Ok(Arc::new(move |cypher: &str| {
        let store = DocStore::open(&path)?;
        let conn = store.connection()?;
        // The guard runs here, where the connection is, so the verdict
        // still comes from the database's own parser.
        infigraph_core::daemon::read_guard::ensure_read_only(&conn, cypher)?;
        DocQuery::new(&conn).raw_query(cypher)
    }))
}
