use std::path::PathBuf;

use anyhow::Result;
use infigraph_core::graph::{CozoStore, GraphStore};

fn main() -> Result<()> {
    let project_root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    let kuzu_path = project_root.join(".infigraph/graph");
    if !kuzu_path.exists() {
        anyhow::bail!("No Kuzu graph at {}", kuzu_path.display());
    }

    let cozo_path = project_root.join(".infigraph/graph.cozo");
    if cozo_path.exists() {
        std::fs::remove_file(&cozo_path)?;
    }

    eprintln!("Opening Kuzu graph at {}", kuzu_path.display());
    let kuzu = GraphStore::open(&kuzu_path)?;
    let conn = kuzu.connection()?;

    eprintln!("Creating CozoDB at {}", cozo_path.display());
    let cozo = CozoStore::open(&cozo_path)?;

    // ── 1. Symbols ─────────────────────────────────────────────────────
    eprintln!("Migrating symbols...");
    let mut result = conn
        .query(
            "MATCH (s:Symbol) RETURN s.id, s.name, s.kind, s.file, s.start_line, s.end_line, \
         s.signature_hash, s.language, s.visibility, s.parent, s.docstring, s.complexity, \
         s.parameters, s.return_type",
        )
        .map_err(|e| anyhow::anyhow!("query symbols: {e}"))?;

    let mut symbols = Vec::new();
    while let Some(row) = result.next() {
        if row.len() >= 14 {
            symbols.push((
                row[0].to_string(),  // id
                row[1].to_string(),  // name
                row[2].to_string(),  // kind
                row[3].to_string(),  // file
                parse_i64(&row[4]),  // start_line
                parse_i64(&row[5]),  // end_line
                row[6].to_string(),  // signature_hash
                row[7].to_string(),  // language
                row[8].to_string(),  // visibility
                row[9].to_string(),  // parent
                row[10].to_string(), // docstring
                parse_i64(&row[11]), // complexity
                row[12].to_string(), // parameters
                row[13].to_string(), // return_type
            ));
        }
    }
    let sym_count = symbols.len();
    cozo.import_symbols(&symbols)?;
    eprintln!("  {} symbols", sym_count);
    drop(symbols);

    // ── 2. Modules ─────────────────────────────────────────────────────
    eprintln!("Migrating modules...");
    let mut result = conn
        .query(
            "MATCH (m:Module) RETURN m.id, m.name, m.file, m.language, m.content_hash, m.summary",
        )
        .map_err(|e| anyhow::anyhow!("query modules: {e}"))?;

    let mut modules = Vec::new();
    while let Some(row) = result.next() {
        if row.len() >= 6 {
            modules.push((
                row[0].to_string(),
                row[1].to_string(),
                row[2].to_string(),
                row[3].to_string(),
                row[4].to_string(),
                row[5].to_string(),
            ));
        }
    }
    let mod_count = modules.len();
    cozo.import_modules(&modules)?;
    eprintln!("  {} modules", mod_count);
    drop(modules);

    // ── 3. Files ──────────────────────────────────────────────────────
    eprintln!("Migrating files...");
    let mut result = conn
        .query("MATCH (f:File) RETURN f.id, f.name, f.path, f.language, f.symbol_count")
        .map_err(|e| anyhow::anyhow!("query files: {e}"))?;

    let mut files = Vec::new();
    while let Some(row) = result.next() {
        if row.len() >= 5 {
            files.push((
                row[0].to_string(),
                row[1].to_string(),
                row[2].to_string(),
                row[3].to_string(),
                parse_i64(&row[4]),
            ));
        }
    }
    let file_count = files.len();
    cozo.import_files(&files)?;
    eprintln!("  {} files", file_count);
    drop(files);

    // ── 4. Statements ─────────────────────────────────────────────────
    eprintln!("Migrating statements...");
    let mut result = conn.query(
        "MATCH (st:Statement) RETURN st.id, st.kind, st.condition, st.start_line, st.end_line, st.depth, st.parent_symbol"
    ).map_err(|e| anyhow::anyhow!("query statements: {e}"))?;

    let mut stmts = Vec::new();
    while let Some(row) = result.next() {
        if row.len() >= 7 {
            stmts.push((
                row[0].to_string(),
                row[1].to_string(),
                row[2].to_string(),
                parse_i64(&row[3]),
                parse_i64(&row[4]),
                parse_i64(&row[5]),
                row[6].to_string(),
            ));
        }
    }
    let stmt_count = stmts.len();
    cozo.import_statements(&stmts)?;
    eprintln!("  {} statements", stmt_count);
    drop(stmts);

    // ── 5. Folders ─────────────────────────────────────────────────────
    eprintln!("Migrating folders...");
    let mut result = conn
        .query("MATCH (f:Folder) RETURN f.id, f.name, f.path")
        .map_err(|e| anyhow::anyhow!("query folders: {e}"))?;

    let mut folders = Vec::new();
    while let Some(row) = result.next() {
        if row.len() >= 3 {
            folders.push((row[0].to_string(), row[1].to_string(), row[2].to_string()));
        }
    }
    let folder_count = folders.len();
    cozo.import_folders(&folders)?;
    eprintln!("  {} folders", folder_count);
    drop(folders);

    // ── 6. Dependencies ──────────────────────────────────────────────
    eprintln!("Migrating dependencies...");
    let mut result = conn
        .query("MATCH (d:Dependency) RETURN d.id, d.name, d.version, d.ecosystem, d.is_dev")
        .map_err(|e| anyhow::anyhow!("query dependencies: {e}"))?;

    let mut deps = Vec::new();
    while let Some(row) = result.next() {
        if row.len() >= 5 {
            deps.push((
                row[0].to_string(),
                row[1].to_string(),
                row[2].to_string(),
                row[3].to_string(),
                parse_bool(&row[4]),
            ));
        }
    }
    let dep_count = deps.len();
    cozo.import_dependencies(&deps)?;
    eprintln!("  {} dependencies", dep_count);
    drop(deps);

    // ── 7. Clusters ──────────────────────────────────────────────────
    eprintln!("Migrating clusters...");
    let mut result = conn
        .query("MATCH (c:Cluster) RETURN c.id, c.name, c.description")
        .map_err(|e| anyhow::anyhow!("query clusters: {e}"))?;

    let mut clusters = Vec::new();
    while let Some(row) = result.next() {
        if row.len() >= 3 {
            clusters.push((row[0].to_string(), row[1].to_string(), row[2].to_string()));
        }
    }
    let cluster_count = clusters.len();
    cozo.import_clusters(&clusters)?;
    eprintln!("  {} clusters", cluster_count);
    drop(clusters);

    // ── 8. Simple edge relations (2 columns) ─────────────────────────
    let simple_edges = [
        (
            "CALLS",
            "calls",
            "(a:Symbol)-[r:CALLS]->(b:Symbol)",
            "a.id, b.id",
        ),
        (
            "DEFINES",
            "defines",
            "(a:File)-[r:DEFINES]->(b:Symbol)",
            "a.id, b.id",
        ),
        (
            "CONTAINS",
            "contains",
            "(a:Module)-[r:CONTAINS]->(b:Symbol)",
            "a.id, b.id",
        ),
        (
            "INHERITS",
            "inherits",
            "(a:Symbol)-[r:INHERITS]->(b:Symbol)",
            "a.id, b.id",
        ),
        (
            "TESTED_BY",
            "tested_by",
            "(a:Symbol)-[r:TESTED_BY]->(b:Symbol)",
            "a.id, b.id",
        ),
        (
            "IMPORTS",
            "imports",
            "(a:Module)-[r:IMPORTS]->(b:Module)",
            "a.id, b.id",
        ),
        (
            "READS",
            "reads_rel",
            "(a:Symbol)-[r:READS]->(b:Symbol)",
            "a.id, b.id",
        ),
        (
            "WRITES",
            "writes_rel",
            "(a:Symbol)-[r:WRITES]->(b:Symbol)",
            "a.id, b.id",
        ),
        (
            "HAS_STATEMENT",
            "has_statement",
            "(a:Symbol)-[r:HAS_STATEMENT]->(b:Statement)",
            "a.id, b.id",
        ),
        (
            "MEMBER_OF",
            "member_of",
            "(a:Symbol)-[r:MEMBER_OF]->(b:Cluster)",
            "a.id, b.id",
        ),
        (
            "CONTAINS_FILE",
            "contains_file",
            "(a:Folder)-[r:CONTAINS_FILE]->(b:File)",
            "a.id, b.id",
        ),
        (
            "CONTAINS_FOLDER",
            "contains_folder",
            "(a:Folder)-[r:CONTAINS_FOLDER]->(b:Folder)",
            "a.id, b.id",
        ),
        (
            "HAS_CONCERN",
            "has_concern",
            "(a:Symbol)-[r:HAS_CONCERN]->(b:Concern)",
            "a.id, b.id",
        ),
        (
            "HAS_CONFIG",
            "has_config",
            "(a:Symbol)-[r:HAS_CONFIG]->(b:ConfigBinding)",
            "a.id, b.id",
        ),
    ];

    for (label, relation, pattern, ret) in &simple_edges {
        eprint!("Migrating {label}...");
        let q = format!("MATCH {pattern} RETURN {ret}");
        let mut result = conn
            .query(&q)
            .map_err(|e| anyhow::anyhow!("query {label}: {e}"))?;

        let mut pairs = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 2 {
                pairs.push((row[0].to_string(), row[1].to_string()));
            }
        }
        let count = pairs.len();
        cozo.import_edges(relation, &pairs)?;
        eprintln!(" {} edges", count);
    }

    // ── 8b. Concern nodes ─────────────────────────────────────────────
    {
        eprint!("Migrating Concern nodes...");
        let mut result = conn
            .query("MATCH (c:Concern) RETURN c.id, c.kind, c.detail")
            .map_err(|e| anyhow::anyhow!("query Concern: {e}"))?;

        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 3 {
                rows.push((row[0].to_string(), row[1].to_string(), row[2].to_string()));
            }
        }
        let count = rows.len();
        cozo.import_concerns(&rows)?;
        eprintln!(" {} nodes", count);
    }

    // ── 8c. ConfigBinding nodes ──────────────────────────────────────
    {
        eprint!("Migrating ConfigBinding nodes...");
        let mut result = conn.query(
            "MATCH (c:ConfigBinding) RETURN c.id, c.kind, c.key, c.value, c.`profile`, c.source_file"
        ).map_err(|e| anyhow::anyhow!("query ConfigBinding: {e}"))?;

        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 6 {
                rows.push((
                    row[0].to_string(),
                    row[1].to_string(),
                    row[2].to_string(),
                    row[3].to_string(),
                    row[4].to_string(),
                    row[5].to_string(),
                ));
            }
        }
        let count = rows.len();
        cozo.import_config_bindings(&rows)?;
        eprintln!(" {} nodes", count);
    }

    // ── 9. Rich edge relations (extra columns) ──────────────────────
    // DEPENDS_ON: module_id, dep_id, is_dev
    {
        eprint!("Migrating DEPENDS_ON...");
        let mut result = conn
            .query("MATCH (a:Module)-[r:DEPENDS_ON]->(b:Dependency) RETURN a.id, b.id, r.is_dev")
            .map_err(|e| anyhow::anyhow!("query DEPENDS_ON: {e}"))?;

        let headers = vec!["module_id".into(), "dep_id".into(), "is_dev".into()];
        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 3 {
                rows.push(vec![
                    cozo::DataValue::Str(row[0].to_string().into()),
                    cozo::DataValue::Str(row[1].to_string().into()),
                    cozo::DataValue::Bool(parse_bool(&row[2])),
                ]);
            }
        }
        let count = rows.len();
        cozo.import_raw("depends_on", headers, rows)?;
        eprintln!(" {} edges", count);
    }

    // SIMILAR_TO: symbol_a, symbol_b, score
    {
        eprint!("Migrating SIMILAR_TO...");
        let mut result = conn
            .query("MATCH (a:Symbol)-[r:SIMILAR_TO]->(b:Symbol) RETURN a.id, b.id, r.score")
            .map_err(|e| anyhow::anyhow!("query SIMILAR_TO: {e}"))?;

        let headers = vec!["symbol_a".into(), "symbol_b".into(), "score".into()];
        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 3 {
                rows.push(vec![
                    cozo::DataValue::Str(row[0].to_string().into()),
                    cozo::DataValue::Str(row[1].to_string().into()),
                    cozo::DataValue::from(parse_f64(&row[2])),
                ]);
            }
        }
        let count = rows.len();
        cozo.import_raw("similar_to", headers, rows)?;
        eprintln!(" {} edges", count);
    }

    // BRIDGE_TO: source, target, bridge_kind, detail
    {
        eprint!("Migrating BRIDGE_TO...");
        let mut result = conn.query(
            "MATCH (a:Symbol)-[r:BRIDGE_TO]->(b:Symbol) RETURN a.id, b.id, r.bridge_kind, r.detail"
        ).map_err(|e| anyhow::anyhow!("query BRIDGE_TO: {e}"))?;

        let headers = vec![
            "source".into(),
            "target".into(),
            "bridge_kind".into(),
            "detail".into(),
        ];
        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 4 {
                rows.push(vec![
                    cozo::DataValue::Str(row[0].to_string().into()),
                    cozo::DataValue::Str(row[1].to_string().into()),
                    cozo::DataValue::Str(row[2].to_string().into()),
                    cozo::DataValue::Str(row[3].to_string().into()),
                ]);
            }
        }
        let count = rows.len();
        cozo.import_raw("bridge_to", headers, rows)?;
        eprintln!(" {} edges", count);
    }

    // CALLS_SERVICE: caller, target, method, path, target_service
    {
        eprint!("Migrating CALLS_SERVICE...");
        let mut result = conn.query(
            "MATCH (a:Symbol)-[r:CALLS_SERVICE]->(b:Symbol) RETURN a.id, b.id, r.method, r.path, r.target_service"
        ).map_err(|e| anyhow::anyhow!("query CALLS_SERVICE: {e}"))?;

        let headers = vec![
            "caller".into(),
            "target".into(),
            "method".into(),
            "path".into(),
            "target_service".into(),
        ];
        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 5 {
                rows.push(vec![
                    cozo::DataValue::Str(row[0].to_string().into()),
                    cozo::DataValue::Str(row[1].to_string().into()),
                    cozo::DataValue::Str(row[2].to_string().into()),
                    cozo::DataValue::Str(row[3].to_string().into()),
                    cozo::DataValue::Str(row[4].to_string().into()),
                ]);
            }
        }
        let count = rows.len();
        cozo.import_raw("calls_service", headers, rows)?;
        eprintln!(" {} edges", count);
    }

    // RESOLVES_TO: source, target, mechanism, config_source
    {
        eprint!("Migrating RESOLVES_TO...");
        let mut result = conn.query(
            "MATCH (a:Symbol)-[r:RESOLVES_TO]->(b:Symbol) RETURN a.id, b.id, r.mechanism, r.config_source"
        ).map_err(|e| anyhow::anyhow!("query RESOLVES_TO: {e}"))?;

        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 4 {
                rows.push((
                    row[0].to_string(),
                    row[1].to_string(),
                    row[2].to_string(),
                    row[3].to_string(),
                ));
            }
        }
        let count = rows.len();
        cozo.import_resolves_to(&rows)?;
        eprintln!(" {} edges", count);
    }

    // TAINT_FLOW: source, target, source_kind, sink_kind, path
    {
        eprint!("Migrating TAINT_FLOW...");
        let mut result = conn.query(
            "MATCH (a:Symbol)-[r:TAINT_FLOW]->(b:Symbol) RETURN a.id, b.id, r.source_kind, r.sink_kind, r.path"
        ).map_err(|e| anyhow::anyhow!("query TAINT_FLOW: {e}"))?;

        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 5 {
                rows.push((
                    row[0].to_string(),
                    row[1].to_string(),
                    row[2].to_string(),
                    row[3].to_string(),
                    row[4].to_string(),
                ));
            }
        }
        let count = rows.len();
        cozo.import_taint_flows(&rows)?;
        eprintln!(" {} edges", count);
    }

    // ── 10. Custom language edges (dynamic) ──────────────────────────
    {
        let known_edges: std::collections::HashSet<&str> = [
            "CALLS",
            "DEPENDS_ON",
            "IMPORTS",
            "CONTAINS",
            "INHERITS",
            "TESTED_BY",
            "READS",
            "WRITES",
            "MEMBER_OF",
            "SIMILAR_TO",
            "BRIDGE_TO",
            "CONTAINS_FILE",
            "CONTAINS_FOLDER",
            "DEFINES",
            "CALLS_SERVICE",
            "HAS_STATEMENT",
            "HAS_CONCERN",
            "HAS_CONFIG",
            "RESOLVES_TO",
            "TAINT_FLOW",
        ]
        .into_iter()
        .collect();

        let mut result = conn
            .query("CALL show_tables() RETURN *")
            .map_err(|e| anyhow::anyhow!("show_tables: {e}"))?;

        let mut custom_edges = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 2 {
                let name = row[0].to_string();
                let ttype = row[1].to_string();
                if ttype == "REL" && !known_edges.contains(name.as_str()) {
                    custom_edges.push(name);
                }
            }
        }

        for edge_name in &custom_edges {
            eprint!("Migrating custom edge {edge_name}...");
            let lower = edge_name.to_lowercase();
            let schema_ddl = format!(":create {lower} {{source: String, target: String}}");
            match cozo.create_custom_edge(&schema_ddl) {
                Ok(_) => {}
                Err(_) => {} // already exists
            }

            let q = format!("MATCH (a:Symbol)-[r:{edge_name}]->(b:Symbol) RETURN a.id, b.id");
            let mut result = conn
                .query(&q)
                .map_err(|e| anyhow::anyhow!("query {edge_name}: {e}"))?;

            let mut pairs = Vec::new();
            while let Some(row) = result.next() {
                if row.len() >= 2 {
                    pairs.push((row[0].to_string(), row[1].to_string()));
                }
            }
            let count = pairs.len();
            cozo.import_edges(&lower, &pairs)?;
            eprintln!(" {} edges", count);
        }
    }

    // ── 11. Verify all relation counts ─────────────────────────────────
    let cozo_counts = cozo.relation_counts()?;

    let kuzu_count_queries: &[(&str, &str, &str)] = &[
        ("symbol", "MATCH (n:Symbol) RETURN count(n)", "symbol"),
        ("module", "MATCH (n:Module) RETURN count(n)", "module"),
        ("cluster", "MATCH (n:Cluster) RETURN count(n)", "cluster"),
        ("file", "MATCH (n:File) RETURN count(n)", "file"),
        ("folder", "MATCH (n:Folder) RETURN count(n)", "folder"),
        (
            "dependency",
            "MATCH (n:Dependency) RETURN count(n)",
            "dependency",
        ),
        (
            "statement",
            "MATCH (n:Statement) RETURN count(n)",
            "statement",
        ),
        ("calls", "MATCH ()-[r:CALLS]->() RETURN count(r)", "calls"),
        (
            "depends_on",
            "MATCH ()-[r:DEPENDS_ON]->() RETURN count(r)",
            "depends_on",
        ),
        (
            "imports",
            "MATCH ()-[r:IMPORTS]->() RETURN count(r)",
            "imports",
        ),
        (
            "contains",
            "MATCH ()-[r:CONTAINS]->() RETURN count(r)",
            "contains",
        ),
        (
            "inherits",
            "MATCH ()-[r:INHERITS]->() RETURN count(r)",
            "inherits",
        ),
        (
            "tested_by",
            "MATCH ()-[r:TESTED_BY]->() RETURN count(r)",
            "tested_by",
        ),
        (
            "reads_rel",
            "MATCH ()-[r:READS]->() RETURN count(r)",
            "reads_rel",
        ),
        (
            "writes_rel",
            "MATCH ()-[r:WRITES]->() RETURN count(r)",
            "writes_rel",
        ),
        (
            "member_of",
            "MATCH ()-[r:MEMBER_OF]->() RETURN count(r)",
            "member_of",
        ),
        (
            "similar_to",
            "MATCH ()-[r:SIMILAR_TO]->() RETURN count(r)",
            "similar_to",
        ),
        (
            "bridge_to",
            "MATCH ()-[r:BRIDGE_TO]->() RETURN count(r)",
            "bridge_to",
        ),
        (
            "contains_file",
            "MATCH ()-[r:CONTAINS_FILE]->() RETURN count(r)",
            "contains_file",
        ),
        (
            "contains_folder",
            "MATCH ()-[r:CONTAINS_FOLDER]->() RETURN count(r)",
            "contains_folder",
        ),
        (
            "defines",
            "MATCH ()-[r:DEFINES]->() RETURN count(r)",
            "defines",
        ),
        (
            "calls_service",
            "MATCH ()-[r:CALLS_SERVICE]->() RETURN count(r)",
            "calls_service",
        ),
        (
            "has_statement",
            "MATCH ()-[r:HAS_STATEMENT]->() RETURN count(r)",
            "has_statement",
        ),
        ("concern", "MATCH (n:Concern) RETURN count(n)", "concern"),
        (
            "has_concern",
            "MATCH ()-[r:HAS_CONCERN]->() RETURN count(r)",
            "has_concern",
        ),
        (
            "config_binding",
            "MATCH (n:ConfigBinding) RETURN count(n)",
            "config_binding",
        ),
        (
            "has_config",
            "MATCH ()-[r:HAS_CONFIG]->() RETURN count(r)",
            "has_config",
        ),
        (
            "resolves_to",
            "MATCH ()-[r:RESOLVES_TO]->() RETURN count(r)",
            "resolves_to",
        ),
        (
            "taint_flow",
            "MATCH ()-[r:TAINT_FLOW]->() RETURN count(r)",
            "taint_flow",
        ),
    ];

    eprintln!("\n=== Migration Verification ===");
    eprintln!(
        "{:<20} {:>8} {:>8}  {}",
        "Relation", "Kuzu", "CozoDB", "Status"
    );
    eprintln!("{}", "-".repeat(55));

    let mut mismatches = Vec::new();
    for (label, query, cozo_key) in kuzu_count_queries {
        let mut result = conn
            .query(query)
            .map_err(|e| anyhow::anyhow!("count {label}: {e}"))?;
        let kuzu_count = result
            .next()
            .map(|row| row[0].to_string().parse::<u64>().unwrap_or(0))
            .unwrap_or(0);
        let cozo_count = cozo_counts.get(*cozo_key).copied().unwrap_or(0);

        let status = if kuzu_count == cozo_count {
            "✅"
        } else if *label == "calls" && kuzu_count > cozo_count {
            "⚠️  dedup"
        } else {
            mismatches.push((*label, kuzu_count, cozo_count));
            "❌ MISMATCH"
        };
        eprintln!(
            "{:<20} {:>8} {:>8}  {}",
            label, kuzu_count, cozo_count, status
        );
    }

    if !mismatches.is_empty() {
        for (label, kuzu, cozo) in &mismatches {
            eprintln!("ERROR: {label} count mismatch: Kuzu={kuzu}, CozoDB={cozo}");
        }
        anyhow::bail!("{} relation(s) have count mismatches", mismatches.len());
    }

    eprintln!("\nMigration complete! CozoDB at {}", cozo_path.display());
    Ok(())
}

fn parse_i64(v: &kuzu::Value) -> i64 {
    v.to_string().parse().unwrap_or(0)
}

fn parse_bool(v: &kuzu::Value) -> bool {
    let s = v.to_string();
    s == "True" || s == "true" || s == "1"
}

fn parse_f64(v: &kuzu::Value) -> f64 {
    v.to_string().parse().unwrap_or(0.0)
}
