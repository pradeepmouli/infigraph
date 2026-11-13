//! A graph built before `IMPLEMENTS` and `REFERENCES` existed has no such
//! tables, and a read-only open never creates them: the schema DDL runs only on
//! a writer's open. The dependency and hierarchy readers must answer from the
//! tables that exist, not fail on the ones that do not.

use infigraph_core::graph::{names, GraphQuery, GraphStore};

/// A graph written by this build, then stripped of the two newer tables, which
/// is what an older build leaves on disk.
fn graph_without_the_newer_tables() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join(names::STEM);
    let store = GraphStore::open(&graph).unwrap();
    {
        let conn = store.connection().unwrap();
        for table in ["REFERENCES", "IMPLEMENTS"] {
            conn.query(&format!("DROP TABLE {table}")).unwrap();
        }
    }
    drop(store);
    (dir, graph)
}

#[test]
fn the_readers_answer_on_a_graph_without_the_newer_tables() {
    let (_dir, graph) = graph_without_the_newer_tables();
    let store = GraphStore::open_read_only(&graph).unwrap();
    let conn = store.connection().unwrap();
    let q = GraphQuery::new(&conn);
    q.find_all_references("a.rs::A")
        .expect("find_all_references on a graph with no REFERENCES table");
    q.transitive_impact("a.rs::A", 2)
        .expect("transitive_impact on a graph with no REFERENCES table");
    q.get_type_hierarchy("a.rs::A", 2)
        .expect("type hierarchy on a graph with no IMPLEMENTS table");
}
