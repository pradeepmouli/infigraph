//! Read-only enforcement for the daemon's read service.
//!
//! The daemon holds a read-write `Database`, so nothing here may allow a
//! write through. The verdict comes from the database's own parser via
//! `PreparedStatement::is_read_only` (lbug-0.20.2 connection.rs:56), not
//! from inspecting the query text -- the 2026-08-01 DaemonKuzu design
//! rejected string classification as fragile in both directions: a false
//! negative silently reintroduces a direct write, a false positive breaks a
//! legitimate `MATCH` whose text happens to contain a keyword.

use anyhow::Result;

/// Prepare `cypher` and return the statement only if the database judges it
/// read-only.
pub fn ensure_read_only(conn: &kuzu::Connection, cypher: &str) -> Result<kuzu::PreparedStatement> {
    let stmt = conn
        .prepare(cypher)
        .map_err(|e| anyhow::anyhow!("failed to prepare query: {e}"))?;
    if !stmt.is_read_only() {
        anyhow::bail!(
            "refused: this is not a read query, and the read service will not execute writes"
        );
    }
    Ok(stmt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon holds a READ-WRITE database, so the read service must not
    /// become a write backdoor. The refusal must come from the database's
    /// own parser -- classifying Cypher by string was rejected in the
    /// 2026-08-01 design as fragile in both directions.
    #[test]
    fn a_write_statement_is_refused_by_the_database_not_by_string_matching() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::graph::GraphStore::open(&dir.path().join("graph")).unwrap();
        let conn = store.connection().unwrap();

        let err = ensure_read_only(
            &conn,
            "CREATE (:File {id: 'x', name: 'x', path: 'x', language: 'rust', symbol_count: 0})",
        )
        // `.map(|_| ())` because `PreparedStatement` is not `Debug`, which
        // `expect_err` requires of the Ok type.
        .map(|_| ())
        .expect_err("a CREATE must be refused");
        assert!(
            err.to_string().contains("not a read"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_read_statement_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::graph::GraphStore::open(&dir.path().join("graph")).unwrap();
        let conn = store.connection().unwrap();
        assert!(ensure_read_only(&conn, "MATCH (f:File) RETURN f.id").is_ok());
    }

    /// A MATCH whose *text* contains a write keyword must still be accepted.
    /// This is the false-positive half of why string classification was
    /// rejected.
    #[test]
    fn a_read_whose_text_contains_a_write_keyword_is_still_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::graph::GraphStore::open(&dir.path().join("graph")).unwrap();
        let conn = store.connection().unwrap();
        assert!(ensure_read_only(
            &conn,
            "MATCH (s:Symbol) WHERE s.name = 'CREATE' RETURN s.id"
        )
        .is_ok());
    }
}
