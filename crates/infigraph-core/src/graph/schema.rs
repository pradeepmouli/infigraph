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
        embedding FLOAT[],
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
    "CREATE REL TABLE IF NOT EXISTS CALLS_SERVICE(FROM Symbol TO Symbol, method STRING, path STRING, target_service STRING)",
];
