//! Connection-failure regression tests for the remote-mode backends.
//!
//! Neither test needs a live Neo4j/Postgres container — both point at a
//! closed local TCP port (127.0.0.1:1, nothing listening). No existing test
//! in this workspace covers this path: every other Neo4jBackend/
//! PostgresMetaStore test either connects to a live container or is skipped
//! when one isn't running — none assert on the failure itself.
//!
//! The two backends fail differently, confirmed empirically here:
//!   - Postgres: `connect()` itself returns `Err` promptly (tokio-postgres
//!     validates the connection eagerly).
//!   - Neo4j: `connect()` returns `Ok` even though nothing is listening —
//!     the neo4rs driver defers the handshake, so the failure only surfaces
//!     on the first real query (~30s, presumably an internal retry/backoff
//!     before giving up). A caller that only checks `connect()`'s result
//!     would believe it has a working connection when it does not.
//!
//! Run: `cargo test -p infigraph-core --features neo4j,postgres --test remote_connection_failure`

#![cfg(all(feature = "neo4j", feature = "postgres"))]

use infigraph_core::graph::{GraphBackend, Neo4jBackend};
use infigraph_core::meta::PostgresMetaStore;

#[test]
fn neo4j_connect_to_closed_port_fails_eventually() {
    let connect_result = Neo4jBackend::connect("127.0.0.1:1", "neo4j", "wrong-password");
    match connect_result {
        Err(_) => {}
        Ok(backend) => {
            let query_result = backend.raw_query("RETURN 1");
            assert!(
                query_result.is_err(),
                "connect() to an unreachable host returned Ok lazily (driver defers the \
                 handshake), but the first real query against it should still fail — got Ok \
                 instead, meaning a caller could believe it has a working connection when it \
                 does not"
            );
        }
    }
}

#[test]
fn postgres_connect_to_closed_port_returns_err_not_panic() {
    let result = PostgresMetaStore::connect(
        "host=127.0.0.1 port=1 user=infigraph password=infigraph dbname=infigraph connect_timeout=2",
    );
    assert!(
        result.is_err(),
        "connecting to a closed port should return Err, not Ok"
    );
}
