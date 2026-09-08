//! The seam that lets `GraphQuery` run against either a local Kuzu
//! connection or the daemon's read service.
//!
//! `GraphQuery` holds every read query in the codebase along with its
//! row-parsing (1045 lines). Abstracting at the *connection* rather than at
//! the backend keeps all of that in one place and serving both paths --
//! re-implementing those queries for a remote backend would duplicate every
//! one of them.

use anyhow::Result;

/// Executes a read query and returns its rows as strings.
///
/// Stringly rows are not a simplification: `GraphQuery` already consumes
/// results this way (`row[0].to_string()`, `row[3].to_string().parse()`),
/// and `GraphBackend::raw_query` already returns `Vec<Vec<String>>`.
pub trait QueryExec {
    fn query_rows(&self, cypher: &str) -> Result<Vec<Vec<String>>>;
}

/// So a borrowed executor can be passed where an owned one is expected --
/// `GraphQuery` takes its executor by value, and callers often have only a
/// reference.
impl<T: QueryExec + ?Sized> QueryExec for &T {
    fn query_rows(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        (**self).query_rows(cypher)
    }
}

/// Runs queries on a Kuzu connection in this process.
pub struct LocalExec<'a, 'db> {
    conn: &'a kuzu::Connection<'db>,
}

impl<'a, 'db> LocalExec<'a, 'db> {
    pub fn new(conn: &'a kuzu::Connection<'db>) -> Self {
        Self { conn }
    }
}

impl QueryExec for LocalExec<'_, '_> {
    fn query_rows(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        let result = self
            .conn
            .query(cypher)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        Ok(result
            .map(|row| row.iter().map(|v| v.to_string()).collect())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `GraphQuery` built over `LocalExec` must return exactly what it
    /// returned when it borrowed the connection directly.
    #[test]
    fn local_exec_returns_the_same_rows_as_a_direct_connection() {
        let dir = tempfile::tempdir().unwrap();
        let graph = dir.path().join("graph");
        let store = crate::graph::GraphStore::open(&graph).unwrap();
        let conn = store.connection().unwrap();
        conn.query(
            "CREATE (:File {id: 'a.rs', name: 'a.rs', path: 'a.rs', \
             language: 'rust', symbol_count: 0})",
        )
        .unwrap();

        let exec = LocalExec::new(&conn);
        let rows = exec.query_rows("MATCH (f:File) RETURN f.id").unwrap();
        assert_eq!(rows, vec![vec!["a.rs".to_string()]]);
    }
}
