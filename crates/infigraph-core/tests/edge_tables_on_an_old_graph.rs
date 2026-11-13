//! A graph built before `IMPLEMENTS` and `REFERENCES` existed has no such
//! tables, and a read-only open never creates them: the schema DDL runs only on
//! a writer's open. The dependency and hierarchy readers must answer from the
//! tables that exist, not fail on the ones that do not.

use infigraph_core::graph::edge_census::{
    census, present_keyless_tables, table_count_query, TableCount, CENSUS_BUDGET,
};
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

/// The doctor's census on the same graph: it counts the tables the graph holds
/// and leaves out the two it lacks, without an error or an unchecked table.
#[test]
fn the_census_counts_only_the_tables_an_old_graph_holds() {
    let (_dir, graph) = graph_without_the_newer_tables();
    let store = GraphStore::open_read_only(&graph).unwrap();
    let conn = store.connection().unwrap();
    let q = GraphQuery::new(&conn);
    let present =
        present_keyless_tables(|s| q.raw_query(s)).expect("the catalog read on an old graph");
    let c = census(CENSUS_BUDGET, &present, |t, s, d| {
        let rows = q.raw_query(&table_count_query(t, s, d))?;
        // An empty match returns no row: the table holds no edges, as in doctor.
        Ok(match rows.first() {
            Some(cells) => TableCount::from_cells(cells).expect("a count row"),
            None => TableCount {
                table: t.to_string(),
                rows: 0,
                pairs: 0,
            },
        })
    })
    .expect("the census on an old graph");
    assert!(c.counted.iter().any(|t| t.table == "CALLS"));
    assert!(c
        .counted
        .iter()
        .all(|t| t.table != "REFERENCES" && t.table != "IMPLEMENTS"));
    assert!(c.not_checked.is_empty(), "{:?}", c.not_checked);
}
