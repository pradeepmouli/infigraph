pub const MIGRATIONS: &[&str] = &[
    "ALTER TABLE Symbol ADD parameters STRING DEFAULT ''",
    "ALTER TABLE Symbol ADD return_type STRING DEFAULT ''",
    "ALTER TABLE Symbol ADD category STRING DEFAULT 'impl'",
    "ALTER TABLE Symbol ADD scip_id STRING DEFAULT ''",
    "CREATE NODE TABLE IF NOT EXISTS Statement(id STRING, kind STRING, condition STRING, start_line INT32, end_line INT32, depth INT32, parent_symbol STRING, PRIMARY KEY(id))",
    "CREATE REL TABLE IF NOT EXISTS HAS_STATEMENT(FROM Symbol TO Statement)",
    "CREATE NODE TABLE IF NOT EXISTS Concern(id STRING, kind STRING, detail STRING, PRIMARY KEY(id))",
    "CREATE REL TABLE IF NOT EXISTS HAS_CONCERN(FROM Symbol TO Concern)",
    "CREATE NODE TABLE IF NOT EXISTS ConfigBinding(id STRING, kind STRING, key STRING, value STRING, `profile` STRING, source_file STRING, PRIMARY KEY(id))",
    "CREATE REL TABLE IF NOT EXISTS HAS_CONFIG(FROM Symbol TO ConfigBinding)",
    "CREATE REL TABLE IF NOT EXISTS RESOLVES_TO(FROM Symbol TO Symbol, mechanism STRING, config_source STRING)",
    "CREATE REL TABLE IF NOT EXISTS TAINT_FLOW(FROM Symbol TO Symbol, source_kind STRING, sink_kind STRING, path STRING)",
    // AIF3X-331 #16: INJECTS_DEPENDENCY/REGISTERS_MIDDLEWARE start as
    // language-plugin custom edges (see python_pack's CustomEdgeDef) but
    // callers_of() now names them explicitly, so they must always exist — a
    // MATCH against an uncreated Kuzu rel table is a binder error, not an
    // empty result, so this can't be left to ensure_custom_edge_table's lazy
    // on-write creation. Named INJECTS_DEPENDENCY, not DEPENDS_ON, because
    // DEPENDS_ON already exists below as the unrelated Module->Dependency
    // package-manager-dependency table (a name collision on a Symbol->Symbol
    // table of the same name would clash with that existing table's schema).
    "CREATE REL TABLE IF NOT EXISTS INJECTS_DEPENDENCY(FROM Symbol TO Symbol)",
    "CREATE REL TABLE IF NOT EXISTS REGISTERS_MIDDLEWARE(FROM Symbol TO Symbol)",
    // R3.3.4 (docs/DESIGN-hardening.md §3.3.4): split the single R3.3.3
    // `generation` counter into two, so SCIP-enrichment staleness relative
    // to AST-only watcher reindexes is a distinguishable, surfaceable gap
    // rather than folded into one number. Existing rows from the R3.3.3-only
    // schema (shipped one commit earlier on this branch) get these columns
    // via ALTER ADD; the old `generation` column is left as harmless,
    // unused deadweight rather than attempting a Kuzu column rename/drop.
    "ALTER TABLE GraphMeta ADD ast_generation INT64 DEFAULT 0",
    "ALTER TABLE GraphMeta ADD scip_generation INT64 DEFAULT 0",
    // R8.1 (#85): the schema version this database was last written by.
    // 0 = predates versioning. Stamped to SCHEMA_VERSION after every
    // successful open's DDL pass; a stored version NEWER than this
    // binary's is refused at open ("built by newer infigraph") before any
    // DDL runs.
    "ALTER TABLE GraphMeta ADD schema_version INT64 DEFAULT 0",
    // A call whose receiver resolves to a real class/type name but that type
    // has no local Symbol (its source isn't indexed — a statically-linked
    // lib, a vendored dependency, an un-group-linked sibling repo) previously
    // vanished with zero trace at write time: not even counted as
    // "unresolved", since resolve_with_map's dangling-call bookkeeping only
    // tracks calls it *tried* to match against symbol_map, and a
    // receiver-qualified call with no symbol_map hit for target_name skips
    // straight past that path. ExternalRef is a lightweight node — never a
    // real Symbol, just a resolved qualifier+method string pair — so
    // `MATCH (a:Symbol)-[:EXTERNAL_CALL]->(e:ExternalRef) WHERE e.qualifier =
    // 'ITpsContext'` answers "what touches TPS" with zero cross-repo setup.
    // If the same repos later get group-linked and the real symbol becomes
    // resolvable, resolve_with_map's normal strategies take priority and this
    // fallback simply stops firing for that call site — the two coexist.
    "CREATE NODE TABLE IF NOT EXISTS ExternalRef(id STRING, qualifier STRING, method STRING, PRIMARY KEY(id))",
    "CREATE REL TABLE IF NOT EXISTS EXTERNAL_CALL(FROM Symbol TO ExternalRef)",
    // Cross-repo namespace-qualified C++ linking (multi::namespace_link) needs
    // to distinguish its static-lib edges from the pre-existing HTTP/gRPC
    // CALLS_SERVICE edges (method/path/target_service columns above), and to
    // carry the matched namespace qualifier (e.g. "tps") for traceability.
    // CALLS_SERVICE is a fixed-column Kuzu rel table, not a dynamic-property
    // one, so both columns must be added via ALTER rather than assumed.
    "ALTER TABLE CALLS_SERVICE ADD protocol STRING DEFAULT ''",
    "ALTER TABLE CALLS_SERVICE ADD qualifier STRING DEFAULT ''",
];

/// The schema version THIS binary writes (R8.1, #85). Bump it whenever
/// `CREATE_SCHEMA`/`MIGRATIONS` change shape in a way an older binary
/// could misread; opening a database stamped with a NEWER version than
/// this refuses with a `Config`-style error instead of open-and-guess.
/// Older/unstamped databases are migrated forward by the unconditional
/// DDL pass exactly as before, then stamped.
pub const SCHEMA_VERSION: i64 = 1;

/// Kuzu schema DDL for the infigraph graph.
pub const CREATE_SCHEMA: &[&str] = &[
    // Node tables
    "CREATE NODE TABLE IF NOT EXISTS Symbol(
        id STRING,
        name STRING,
        kind STRING,
        file STRING,
        start_line INT32,
        end_line INT32,
        signature_hash STRING,
        language STRING,
        visibility STRING,
        parent STRING,
        docstring STRING,
        complexity INT32,
        parameters STRING,
        return_type STRING,
        category STRING,
        scip_id STRING,
        PRIMARY KEY(id)
    )",
    "CREATE NODE TABLE IF NOT EXISTS Module(
        id STRING,
        name STRING,
        file STRING,
        language STRING,
        content_hash STRING,
        summary STRING,
        PRIMARY KEY(id)
    )",
    "CREATE NODE TABLE IF NOT EXISTS Cluster(
        id STRING,
        name STRING,
        description STRING,
        PRIMARY KEY(id)
    )",
    "CREATE NODE TABLE IF NOT EXISTS File(
        id STRING,
        name STRING,
        path STRING,
        language STRING,
        symbol_count INT32,
        PRIMARY KEY(id)
    )",
    "CREATE NODE TABLE IF NOT EXISTS Folder(
        id STRING,
        name STRING,
        path STRING,
        PRIMARY KEY(id)
    )",
    "CREATE NODE TABLE IF NOT EXISTS Dependency(
        id STRING,
        name STRING,
        version STRING,
        ecosystem STRING,
        is_dev BOOLEAN,
        PRIMARY KEY(id)
    )",
    "CREATE NODE TABLE IF NOT EXISTS Statement(
        id STRING,
        kind STRING,
        condition STRING,
        start_line INT32,
        end_line INT32,
        depth INT32,
        parent_symbol STRING,
        PRIMARY KEY(id)
    )",
    // Relationship tables
    "CREATE REL TABLE IF NOT EXISTS CALLS(FROM Symbol TO Symbol)",
    "CREATE REL TABLE IF NOT EXISTS DEPENDS_ON(FROM Module TO Dependency, is_dev BOOLEAN)",
    "CREATE REL TABLE IF NOT EXISTS IMPORTS(FROM Module TO Module)",
    "CREATE REL TABLE IF NOT EXISTS CONTAINS(FROM Module TO Symbol)",
    "CREATE REL TABLE IF NOT EXISTS INHERITS(FROM Symbol TO Symbol)",
    "CREATE REL TABLE IF NOT EXISTS TESTED_BY(FROM Symbol TO Symbol)",
    "CREATE REL TABLE IF NOT EXISTS READS(FROM Symbol TO Symbol)",
    "CREATE REL TABLE IF NOT EXISTS WRITES(FROM Symbol TO Symbol)",
    "CREATE REL TABLE IF NOT EXISTS MEMBER_OF(FROM Symbol TO Cluster)",
    "CREATE REL TABLE IF NOT EXISTS SIMILAR_TO(FROM Symbol TO Symbol, score FLOAT)",
    "CREATE REL TABLE IF NOT EXISTS BRIDGE_TO(FROM Symbol TO Symbol, bridge_kind STRING, detail STRING)",
    "CREATE REL TABLE IF NOT EXISTS CONTAINS_FILE(FROM Folder TO File)",
    "CREATE REL TABLE IF NOT EXISTS CONTAINS_FOLDER(FROM Folder TO Folder)",
    "CREATE REL TABLE IF NOT EXISTS DEFINES(FROM File TO Symbol)",
    "CREATE REL TABLE IF NOT EXISTS CALLS_SERVICE(FROM Symbol TO Symbol, method STRING, path STRING, target_service STRING, protocol STRING DEFAULT '', qualifier STRING DEFAULT '')",
    "CREATE REL TABLE IF NOT EXISTS HAS_STATEMENT(FROM Symbol TO Statement)",
    "CREATE NODE TABLE IF NOT EXISTS Concern(id STRING, kind STRING, detail STRING, PRIMARY KEY(id))",
    "CREATE REL TABLE IF NOT EXISTS HAS_CONCERN(FROM Symbol TO Concern)",
    "CREATE NODE TABLE IF NOT EXISTS ConfigBinding(id STRING, kind STRING, key STRING, value STRING, `profile` STRING, source_file STRING, PRIMARY KEY(id))",
    "CREATE REL TABLE IF NOT EXISTS HAS_CONFIG(FROM Symbol TO ConfigBinding)",
    "CREATE REL TABLE IF NOT EXISTS RESOLVES_TO(FROM Symbol TO Symbol, mechanism STRING, config_source STRING)",
    "CREATE REL TABLE IF NOT EXISTS TAINT_FLOW(FROM Symbol TO Symbol, source_kind STRING, sink_kind STRING, path STRING)",
    // R3.3.3/R3.3.4 (docs/DESIGN-hardening.md §3.3.3-4): a single-row table
    // holding two monotonically incremented generation counters.
    // `ast_generation` is bumped once per completed write to the graph
    // (every reindex, including watcher batches) -- sidecars (embeddings.bin
    // etc.) record the generation they were built from, so a sidecar from a
    // stale generation is detectable rather than silently served.
    // `scip_generation` is bumped only by an explicit SCIP-enrichment run,
    // which the watcher's AST-only incremental reindex never triggers on its
    // own -- comparing the two surfaces that drift (R3.3.4) instead of
    // leaving INHERITS edges and other compiler-verified data silently out
    // of sync with a live-watched codebase.
    "CREATE NODE TABLE IF NOT EXISTS GraphMeta(id STRING, ast_generation INT64, scip_generation INT64, schema_version INT64, PRIMARY KEY(id))",
];

use kuzu::Connection;

pub fn ensure_custom_edge_table(conn: &Connection<'_>, edge_name: &str) -> anyhow::Result<()> {
    let ddl = format!(
        "CREATE REL TABLE IF NOT EXISTS {}(FROM Symbol TO Symbol)",
        edge_name
    );
    match conn.query(&ddl) {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("already exists") {
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "failed to create custom edge table '{}': {}",
                    edge_name,
                    e
                ))
            }
        }
    }
}
