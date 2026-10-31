//! Which scheme the ids in a graph were written under.
//!
//! An id is what every edge, sidecar, saved session and glossary key names a
//! symbol by, and an incremental index only re-extracts a file whose content
//! changed. A binary that names symbols differently therefore cannot adopt an
//! older graph a file at a time: the rest would keep the old names, and an edge
//! between the two kinds would dangle. A graph that holds symbols written under
//! an older scheme is rebuilt whole instead.
//!
//! The scheme is a number in `GraphMeta`, not `SCHEMA_VERSION`: the tables are
//! the same, an older binary opens the graph without complaint, and only the
//! meaning of an id has moved. It is stamped when a graph holds no symbols
//! (`GraphStore::init_schema`), so a graph built from scratch -- the daemon's
//! rebuild at a side path, `index --full` -- carries the current scheme and an
//! older graph keeps the 0 its new column defaults to.
//!
//! 1: a member only SCIP finds is `file::Owner::...::name`; an object-literal
//!    member is `file::Const::key`, or `file::key#N` when no name binds its
//!    literal; no parameter or type parameter is a symbol.
//! 2: a TypeScript `abstract class` is a class symbol and owns its members
//!    (`file::Base::name`, not a bare `file::name` or `#N`); an interface,
//!    type-literal or class property is a tree-sitter symbol `file::Owner::name`
//!    (through nested literals, as scip-typescript names it); a local variable
//!    is `file::fn::name`, owned by its nearest enclosing named function.
//! 3: a member's name is the name the source spells: a `$` in an identifier
//!    is part of it (`file::As::$kind`, as tree-sitter has it, not `As::kind`);
//!    a quoted member name is unquoted (`file::Flags::c-strings`, not
//!    `'c-strings'`); a TypeScript namespace is an owner, nested ones in order
//!    (`file::Pattern::Map`, `file::Literal::String::C`).
//! 4: an object-literal member with a quoted key is named by its key and owned
//!    by the literal it sits in (`file::ENRICH_SHAPES::unfielded-symbol-slot`),
//!    not by scip-typescript's `'unfielded-symbol-slot'0` with no owner; a
//!    member is a variable, not a function.
//! 5: an object literal under `as`, `satisfies` or parentheses (any nesting:
//!    `{ .. } as const satisfies Record<..>`) is still the literal the name is
//!    bound to, so its members are `file::Const::key`, and a literal nested as
//!    the value of a key under such wrappers is owned by that key too.

use std::path::Path;

use super::observe::ObservedGraph;
use super::query_exec::{LocalExec, QueryExec};
use super::GraphStore;

/// The scheme this binary writes.
pub const ID_SCHEME: i64 = 5;

/// The scheme a graph is stamped with: 0 for one that predates the stamp (the
/// row missing or NULL reads as 0). `None` when the query itself failed: that is
/// not knowing, and must not read as "predates the stamp" -- the answer decides
/// whether a healthy graph is wiped and rebuilt.
pub fn stored(exec: &dyn QueryExec) -> Option<i64> {
    let rows = exec
        .query_rows("MATCH (g:GraphMeta {id: 'singleton'}) RETURN g.id_scheme")
        .ok()?;
    Some(
        rows.first()
            .and_then(|row| row.first())
            .and_then(|v| v.trim_matches('"').parse().ok())
            .unwrap_or(0),
    )
}

/// True when the graph holds symbols written under an older scheme. A graph
/// that cannot be read says no: never act on a state that is not known.
pub fn outdated(exec: &dyn QueryExec) -> bool {
    stored(exec).is_some_and(|scheme| scheme < ID_SCHEME)
        && first_int(exec, "MATCH (s:Symbol) RETURN count(*)").is_some_and(|n| n > 0)
}

/// [`outdated`] for a graph this process holds open; `None` when it cannot be
/// queried.
pub fn outdated_in(store: &GraphStore) -> Option<bool> {
    let conn = store.connection().ok()?;
    let exec = LocalExec::new(&conn);
    first_int(&exec, "MATCH (s:Symbol) RETURN count(*)")?;
    Some(outdated(&exec))
}

/// [`outdated`] for a project nobody holds open here: through its live daemon
/// or by a read-only open, never starting a daemon. `false` when there is no
/// graph or it cannot be read.
pub fn outdated_at(root: &Path) -> bool {
    let graph = root.join(".infigraph").join(crate::graph::names::STEM);
    if !graph.exists() {
        return false;
    }
    ObservedGraph::open(root, &graph)
        .and_then(|g| g.with_exec(outdated))
        .unwrap_or(false)
}

/// Stamp the current scheme on a graph with no symbols in it: whatever is
/// written into it from here on is written under this binary's scheme.
pub(crate) fn stamp_if_empty(conn: &kuzu::Connection<'_>) -> anyhow::Result<()> {
    let exec = LocalExec::new(conn);
    if first_int(&exec, "MATCH (s:Symbol) RETURN count(*)") == Some(0) {
        conn.query(&format!(
            "MATCH (g:GraphMeta {{id: 'singleton'}}) SET g.id_scheme = {ID_SCHEME}"
        ))
        .map_err(|e| anyhow::anyhow!("failed to stamp the id scheme: {e}"))?;
    }
    Ok(())
}

fn first_int(exec: &dyn QueryExec, cypher: &str) -> Option<i64> {
    exec.query_rows(cypher)
        .ok()?
        .first()?
        .first()?
        .trim_matches('"')
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join(crate::graph::names::STEM)).unwrap();
        (dir, store)
    }

    fn add_symbol(store: &GraphStore) {
        store
            .connection()
            .unwrap()
            .query(
                "CREATE (:Symbol {id: 'a.ts::f', name: 'f', kind: 'function', file: 'a.ts', \
                 start_line: 1, end_line: 1, signature_hash: '', language: 'typescript', \
                 visibility: '', parent: '', docstring: '', complexity: 0, parameters: '', \
                 return_type: ''})",
            )
            .unwrap();
    }

    fn set_scheme(store: &GraphStore, n: i64) {
        store
            .connection()
            .unwrap()
            .query(&format!(
                "MATCH (g:GraphMeta {{id: 'singleton'}}) SET g.id_scheme = {n}"
            ))
            .unwrap();
    }

    #[test]
    fn a_graph_made_from_scratch_is_stamped_and_never_outdated() {
        let (_dir, store) = store();
        assert_eq!(
            stored(&LocalExec::new(&store.connection().unwrap())),
            Some(ID_SCHEME)
        );
        add_symbol(&store);
        assert_eq!(outdated_in(&store), Some(false));
    }

    #[test]
    fn a_graph_with_symbols_and_no_stamp_is_outdated() {
        let (_dir, store) = store();
        add_symbol(&store);
        set_scheme(&store, 0);
        assert_eq!(outdated_in(&store), Some(true));
    }

    #[test]
    fn an_empty_graph_is_not_outdated_whatever_it_is_stamped() {
        let (_dir, store) = store();
        set_scheme(&store, 0);
        assert_eq!(outdated_in(&store), Some(false), "nothing in it to rename");
    }

    #[test]
    fn opening_an_unstamped_graph_with_symbols_leaves_it_unstamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::graph::names::STEM);
        {
            let store = GraphStore::open(&path).unwrap();
            add_symbol(&store);
            set_scheme(&store, 0);
        }
        let store = GraphStore::open(&path).unwrap();
        assert_eq!(
            outdated_in(&store),
            Some(true),
            "reopening must not claim the old ids as new"
        );
    }

    #[test]
    fn a_graph_written_before_abstract_class_members_were_owned_is_rebuilt_once() {
        // Scheme 1 named an abstract class's members bare; scheme 2 owns them.
        let (_dir, store) = store();
        add_symbol(&store);
        set_scheme(&store, 1);
        assert_eq!(outdated_in(&store), Some(true));
        set_scheme(&store, ID_SCHEME);
        assert_eq!(outdated_in(&store), Some(false), "stamped once rebuilt");
    }

    #[test]
    fn a_newer_scheme_is_not_outdated() {
        let (_dir, store) = store();
        add_symbol(&store);
        set_scheme(&store, ID_SCHEME + 1);
        assert_eq!(outdated_in(&store), Some(false));
    }

    /// A graph holding `n` symbols whose stamp query fails (a busy daemon, a
    /// query cut short) while its other queries answer.
    struct StampFails(u32);

    impl QueryExec for StampFails {
        fn query_rows(&self, cypher: &str) -> anyhow::Result<Vec<Vec<String>>> {
            if cypher.contains("id_scheme") {
                anyhow::bail!("daemon busy");
            }
            Ok(vec![vec![self.0.to_string()]])
        }
    }

    /// Answers the stamp query with no rows and the symbol count with `n`.
    struct NoStamp(u32);

    impl QueryExec for NoStamp {
        fn query_rows(&self, cypher: &str) -> anyhow::Result<Vec<Vec<String>>> {
            Ok(if cypher.contains("count(*)") {
                vec![vec![self.0.to_string()]]
            } else {
                vec![]
            })
        }
    }

    #[test]
    fn a_query_that_fails_is_not_an_outdated_graph() {
        assert_eq!(stored(&StampFails(5)), None);
        assert!(
            !outdated(&StampFails(5)),
            "a transient error must never wipe a graph"
        );
    }

    #[test]
    fn a_missing_stamp_row_reads_as_the_oldest_scheme() {
        assert_eq!(stored(&NoStamp(5)), Some(0));
        assert!(outdated(&NoStamp(5)));
        assert!(!outdated(&NoStamp(0)), "nothing in it to rename");
    }

    #[test]
    fn the_table_layout_is_unchanged_so_an_older_binary_still_opens_it() {
        assert_eq!(super::super::schema::SCHEMA_VERSION, 1);
    }

    #[test]
    fn a_project_on_disk_is_judged_without_a_daemon_or_a_write() {
        let root = tempfile::tempdir().unwrap();
        let graph = root
            .path()
            .join(".infigraph")
            .join(crate::graph::names::STEM);
        std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
        {
            let store = GraphStore::open(&graph).unwrap();
            add_symbol(&store);
            set_scheme(&store, 0);
        }
        assert!(outdated_at(root.path()));
        assert!(
            !outdated_at(&root.path().join("nowhere")),
            "no graph, nothing to rebuild"
        );
    }

    /// What an infigraph without this stamp (3.2.16) does on opening a graph:
    /// the unconditional DDL and migration pass minus the stamp's own column,
    /// then `schema_version`. The stamp must survive it and no symbol with it.
    #[test]
    fn a_binary_that_does_not_know_the_stamp_opens_the_graph_and_leaves_it_alone() {
        use super::super::schema::{CREATE_SCHEMA, MIGRATIONS, SCHEMA_VERSION};
        let (_dir, store) = store();
        add_symbol(&store);
        let conn = store.connection().unwrap();
        for ddl in CREATE_SCHEMA {
            conn.query(ddl).unwrap();
        }
        for migration in MIGRATIONS.iter().filter(|m| !m.contains("id_scheme")) {
            let _ = conn.query(migration);
        }
        conn.query(&format!(
            "MERGE (g:GraphMeta {{id: 'singleton'}}) \
             ON CREATE SET g.ast_generation = 0, g.scip_generation = 0, \
                           g.schema_version = {SCHEMA_VERSION} \
             ON MATCH SET g.schema_version = {SCHEMA_VERSION}"
        ))
        .unwrap();
        assert_eq!(
            first_int(&LocalExec::new(&conn), "MATCH (s:Symbol) RETURN count(*)"),
            Some(1)
        );
        assert_eq!(stored(&LocalExec::new(&conn)), Some(ID_SCHEME));
    }
}
