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
///
/// A missing store is not a failure: documents are opt-in, and opening one
/// would create it. So the source always registers, and each request checks
/// for the store, because `index-docs` creates it while the daemon runs.
pub fn daemon_row_source(root: &Path) -> Result<infigraph_core::daemon::read_service::RowSource> {
    // Fail fast on a store that exists but cannot be opened: the daemon
    // logs this once and serves the graph only.
    match DocStore::open_for_read(root) {
        Ok(store) => drop(store),
        Err(e) if e.is::<infigraph_core::docs_switch::DocsNotIndexed>() => {}
        Err(e) => return Err(e),
    }

    let root = root.to_path_buf();
    Ok(Arc::new(move |cypher: &str| {
        let store = DocStore::open_for_read(&root)?;
        let conn = store.connection()?;
        // The guard runs here, where the connection is, so the verdict
        // still comes from the database's own parser.
        infigraph_core::daemon::read_guard::ensure_read_only(&conn, cypher)?;
        DocQuery::new(&conn).raw_query(cypher)
    }))
}
