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

    /// Identifier escaping must survive a backslash.
    ///
    /// `kuzu_backend` carried its own `escape` that replaced only `'`, while
    /// `crate::escape_str` replaces `\\` first and then `'`. The quote-only
    /// version produces malformed Cypher for any identifier containing a
    /// backslash -- a Windows path, or a raw-string literal in a symbol name
    /// -- so the two were unified on the stronger one. Order matters:
    /// escaping quotes first would re-escape the backslashes just inserted.
    #[test]
    fn an_identifier_containing_a_backslash_round_trips_through_a_literal() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::graph::GraphStore::open(&dir.path().join("graph")).unwrap();
        let conn = store.connection().unwrap();

        let id = r"src\\win\\a.rs";
        let escaped = crate::escape_str(id);
        conn.query(&format!(
            "CREATE (:File {{id: '{escaped}', name: 'a.rs', path: '{escaped}', \
             language: 'rust', symbol_count: 0}})"
        ))
        .expect("a backslash-bearing id must produce valid Cypher");

        let exec = LocalExec::new(&conn);
        let rows = exec
            .query_rows(&format!(
                "MATCH (f:File) WHERE f.id = '{escaped}' RETURN f.id"
            ))
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![id.to_string()]],
            "the escaped literal must match the value it was built from"
        );
    }

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
