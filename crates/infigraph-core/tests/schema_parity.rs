use std::collections::BTreeSet;

/// Extract node table and relation names from Kuzu CREATE_SCHEMA DDL strings.
fn extract_kuzu_tables(schema: &[&str]) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut nodes = BTreeSet::new();
    let mut rels = BTreeSet::new();
    for ddl in schema {
        let upper = ddl.to_uppercase();
        if upper.contains("CREATE NODE TABLE") {
            if let Some(name) = extract_after(&upper, "CREATE NODE TABLE IF NOT EXISTS ") {
                nodes.insert(name.to_lowercase());
            }
        } else if upper.contains("CREATE REL TABLE") {
            if let Some(name) = extract_after(&upper, "CREATE REL TABLE IF NOT EXISTS ") {
                rels.insert(name.to_lowercase());
            }
        }
    }
    (nodes, rels)
}

fn extract_after<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let idx = s.find(prefix)? + prefix.len();
    let rest = &s[idx..];
    let end = rest.find('(').unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// Extract relation names from CozoDB `:create` DDL strings.
fn extract_cozo_relations(schema: &[&str]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for ddl in schema {
        if ddl.starts_with(":create ") {
            let rest = &ddl[8..];
            let end = rest
                .find(|c: char| c == ' ' || c == '{')
                .unwrap_or(rest.len());
            names.insert(rest[..end].trim().to_string());
        }
    }
    names
}

// Kuzu schema is pub(crate), so we access it through the public GraphStore.
// We parse the DDL constants directly since they're in the schema module.
// For this test we duplicate the expected names — if schema changes, test breaks.

/// Canonical set of node tables that MUST exist in both backends.
const EXPECTED_NODE_TABLES: &[&str] = &[
    "symbol",
    "module",
    "cluster",
    "file",
    "folder",
    "dependency",
    "statement",
    "concern",
    "configbinding",
];

/// Canonical set of relation/edge tables that MUST exist in both backends.
/// CozoDB uses lowercase; Kuzu uses uppercase. We normalize to lowercase.
/// Note: CozoDB uses `reads_rel`/`writes_rel` instead of `reads`/`writes` (reserved words).
const EXPECTED_RELATIONS: &[&str] = &[
    "calls",
    "depends_on",
    "imports",
    "contains",
    "inherits",
    "tested_by",
    "member_of",
    "similar_to",
    "bridge_to",
    "contains_file",
    "contains_folder",
    "defines",
    "calls_service",
    "has_statement",
    "has_concern",
    "has_config",
    "resolves_to",
    "taint_flow",
];

/// Relations that exist in both but with different names due to reserved words.
const RENAMED_RELATIONS: &[(&str, &str)] = &[("reads", "reads_rel"), ("writes", "writes_rel")];

#[test]
fn test_kuzu_schema_has_all_expected_node_tables() {
    let schema = infigraph_core::graph::schema_ddl();
    let (nodes, _) = extract_kuzu_tables(&schema);
    for expected in EXPECTED_NODE_TABLES {
        assert!(
            nodes.contains(*expected),
            "Kuzu schema missing node table: {expected}. Found: {nodes:?}"
        );
    }
}

#[test]
fn test_kuzu_schema_has_all_expected_relations() {
    let schema = infigraph_core::graph::schema_ddl();
    let (_, rels) = extract_kuzu_tables(&schema);
    for expected in EXPECTED_RELATIONS {
        assert!(
            rels.contains(*expected),
            "Kuzu schema missing relation: {expected}. Found: {rels:?}"
        );
    }
    for (kuzu_name, _) in RENAMED_RELATIONS {
        assert!(
            rels.contains(*kuzu_name),
            "Kuzu schema missing relation: {kuzu_name}. Found: {rels:?}"
        );
    }
}

#[test]
fn test_cozo_schema_has_all_expected_relations() {
    let schema = infigraph_core::graph::cozo_schema_ddl();
    let cozo_rels = extract_cozo_relations(&schema);
    for expected in EXPECTED_RELATIONS {
        assert!(
            cozo_rels.contains(*expected),
            "CozoDB schema missing relation: {expected}. Found: {cozo_rels:?}"
        );
    }
    for (_, cozo_name) in RENAMED_RELATIONS {
        assert!(
            cozo_rels.contains(*cozo_name),
            "CozoDB schema missing relation: {cozo_name}. Found: {cozo_rels:?}"
        );
    }
}

#[test]
fn test_cozo_schema_has_all_expected_node_tables() {
    let schema = infigraph_core::graph::cozo_schema_ddl();
    let cozo_rels = extract_cozo_relations(&schema);
    for expected in EXPECTED_NODE_TABLES {
        let name = if *expected == "configbinding" {
            "config_binding"
        } else {
            expected
        };
        assert!(
            cozo_rels.contains(name),
            "CozoDB schema missing node table: {name}. Found: {cozo_rels:?}"
        );
    }
}

#[test]
fn test_schema_parity_no_kuzu_only_tables() {
    let kuzu_schema = infigraph_core::graph::schema_ddl();
    let cozo_schema = infigraph_core::graph::cozo_schema_ddl();
    let (kuzu_nodes, kuzu_rels) = extract_kuzu_tables(&kuzu_schema);
    let cozo_rels_set = extract_cozo_relations(&cozo_schema);

    let rename_map: std::collections::HashMap<&str, &str> =
        RENAMED_RELATIONS.iter().copied().collect();

    for node in &kuzu_nodes {
        let cozo_name = if node == "configbinding" {
            "config_binding".to_string()
        } else {
            node.clone()
        };
        assert!(
            cozo_rels_set.contains(&cozo_name),
            "Kuzu node table '{node}' has no CozoDB equivalent (expected '{cozo_name}')"
        );
    }

    for rel in &kuzu_rels {
        let cozo_name = rename_map
            .get(rel.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| rel.clone());
        assert!(
            cozo_rels_set.contains(&cozo_name),
            "Kuzu relation '{rel}' has no CozoDB equivalent (expected '{cozo_name}')"
        );
    }
}

#[test]
fn test_symbol_schema_has_no_embedding_column() {
    let schema = infigraph_core::graph::schema_ddl();
    for ddl in &schema {
        if ddl.to_uppercase().contains("CREATE NODE TABLE") && ddl.to_uppercase().contains("SYMBOL")
        {
            assert!(
                !ddl.to_lowercase().contains("embedding"),
                "Symbol schema should NOT contain embedding column — embeddings are stored in sidecar embeddings.bin file, not in graph DB. Found: {ddl}"
            );
        }
    }
}

/// Extract column names from a Kuzu CREATE NODE TABLE DDL.
/// e.g. "CREATE NODE TABLE IF NOT EXISTS Symbol(id STRING, name STRING, ...)" -> ["id", "name", ...]
fn extract_kuzu_columns(ddl: &str) -> Vec<String> {
    let open = match ddl.find('(') {
        Some(i) => i + 1,
        None => return vec![],
    };
    let close = match ddl.rfind(')') {
        Some(i) => i,
        None => return vec![],
    };
    let body = &ddl[open..close];
    body.split(',')
        .filter_map(|part| {
            let trimmed = part.trim();
            if trimmed.to_uppercase().starts_with("PRIMARY KEY") {
                return None;
            }
            let col_name = trimmed.split_whitespace().next()?;
            Some(col_name.trim_matches('`').to_lowercase())
        })
        .collect()
}

/// Extract column names from a CozoDB `:create` DDL.
/// e.g. ":create symbol {id: String => name: String, kind: String}" -> ["id", "name", "kind"]
fn extract_cozo_columns(ddl: &str) -> Vec<String> {
    let open = match ddl.find('{') {
        Some(i) => i + 1,
        None => return vec![],
    };
    let close = match ddl.rfind('}') {
        Some(i) => i,
        None => return vec![],
    };
    let body = ddl[open..close].replace("=>", ",");
    body.split(',')
        .filter_map(|part| {
            let trimmed = part.trim();
            let col_name = trimmed.split(':').next()?.trim();
            if col_name.is_empty() {
                return None;
            }
            Some(col_name.to_lowercase())
        })
        .collect()
}

/// Extract table name from Kuzu DDL.
fn kuzu_table_name(ddl: &str) -> Option<String> {
    let upper = ddl.to_uppercase();
    if let Some(name) = extract_after(&upper, "CREATE NODE TABLE IF NOT EXISTS ") {
        return Some(name.to_lowercase());
    }
    None
}

/// Extract relation name from CozoDB DDL.
fn cozo_relation_name(ddl: &str) -> Option<String> {
    if ddl.starts_with(":create ") {
        let rest = &ddl[8..];
        let end = rest
            .find(|c: char| c == ' ' || c == '{')
            .unwrap_or(rest.len());
        return Some(rest[..end].trim().to_string());
    }
    None
}

/// Mapping from Kuzu node table names to their CozoDB equivalents.
fn kuzu_to_cozo_table_name(kuzu: &str) -> &str {
    match kuzu {
        "configbinding" => "config_binding",
        other => other,
    }
}

#[test]
fn test_node_table_column_parity() {
    let kuzu_schema = infigraph_core::graph::schema_ddl();
    let cozo_schema = infigraph_core::graph::cozo_schema_ddl();

    // Build map: cozo relation name -> columns
    let mut cozo_cols: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for ddl in &cozo_schema {
        if let Some(name) = cozo_relation_name(ddl) {
            cozo_cols.insert(name, extract_cozo_columns(ddl));
        }
    }

    // For each Kuzu node table, verify all columns exist in CozoDB equivalent
    for ddl in &kuzu_schema {
        let upper = ddl.to_uppercase();
        if !upper.contains("CREATE NODE TABLE") {
            continue;
        }
        let kuzu_name = match kuzu_table_name(ddl) {
            Some(n) => n,
            None => continue,
        };
        let cozo_name = kuzu_to_cozo_table_name(&kuzu_name);
        let kuzu_columns = extract_kuzu_columns(ddl);

        let cozo_columns = match cozo_cols.get(cozo_name) {
            Some(c) => c,
            None => panic!("Kuzu node table '{kuzu_name}' has no CozoDB equivalent '{cozo_name}'"),
        };

        for col in &kuzu_columns {
            assert!(
                cozo_columns.contains(col),
                "Kuzu table '{kuzu_name}' has column '{col}' missing from CozoDB '{cozo_name}'. \
                 Kuzu columns: {kuzu_columns:?}, CozoDB columns: {cozo_columns:?}"
            );
        }
    }
}

#[test]
fn test_edge_column_parity() {
    let kuzu_schema = infigraph_core::graph::schema_ddl();
    let cozo_schema = infigraph_core::graph::cozo_schema_ddl();

    // Build map: cozo relation name -> columns
    let mut cozo_cols: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for ddl in &cozo_schema {
        if let Some(name) = cozo_relation_name(ddl) {
            cozo_cols.insert(name, extract_cozo_columns(ddl));
        }
    }

    // Kuzu edge name -> CozoDB edge name
    let edge_rename: std::collections::HashMap<&str, &str> =
        [("reads", "reads_rel"), ("writes", "writes_rel")]
            .into_iter()
            .collect();

    for ddl in &kuzu_schema {
        let upper = ddl.to_uppercase();
        if !upper.contains("CREATE REL TABLE") {
            continue;
        }
        let kuzu_name = match extract_after(&upper, "CREATE REL TABLE IF NOT EXISTS ") {
            Some(n) => n.to_lowercase(),
            None => continue,
        };

        // Extract extra columns from Kuzu REL TABLE (beyond FROM/TO)
        // Format: CREATE REL TABLE IF NOT EXISTS FOO(FROM X TO Y, col1 TYPE, col2 TYPE)
        let open = match ddl.find('(') {
            Some(i) => i + 1,
            None => continue,
        };
        let close = match ddl.rfind(')') {
            Some(i) => i,
            None => continue,
        };
        let body = &ddl[open..close];
        let parts: Vec<&str> = body.split(',').collect();
        // First part is "FROM X TO Y", rest are extra columns
        let extra_cols: Vec<String> = parts[1..]
            .iter()
            .filter_map(|p| {
                let trimmed = p.trim();
                let col = trimmed.split_whitespace().next()?;
                let col_lower = col.to_lowercase();
                if col_lower == "from" || col_lower == "to" {
                    return None;
                }
                Some(col_lower)
            })
            .collect();

        if extra_cols.is_empty() {
            continue;
        }

        let cozo_name = edge_rename
            .get(kuzu_name.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| kuzu_name.clone());

        let cozo_columns = match cozo_cols.get(&cozo_name) {
            Some(c) => c,
            None => {
                panic!("Kuzu edge '{kuzu_name}' has no CozoDB equivalent '{cozo_name}'");
            }
        };

        for col in &extra_cols {
            // Kuzu uses backtick-quoted `profile` — strip backticks
            let clean = col.trim_matches('`');
            assert!(
                cozo_columns.contains(&clean.to_string()),
                "Kuzu edge '{kuzu_name}' has column '{clean}' missing from CozoDB '{cozo_name}'. \
                 Kuzu extra columns: {extra_cols:?}, CozoDB columns: {cozo_columns:?}"
            );
        }
    }
}

#[test]
fn test_migrations_covered_in_cozo_base_schema() {
    let kuzu_schema = infigraph_core::graph::schema_ddl();
    let cozo_schema = infigraph_core::graph::cozo_schema_ddl();
    let cozo_rels = extract_cozo_relations(&cozo_schema);

    // Build column maps for CozoDB
    let mut cozo_cols: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for ddl in &cozo_schema {
        if let Some(name) = cozo_relation_name(ddl) {
            cozo_cols.insert(name, extract_cozo_columns(ddl));
        }
    }

    // Check each migration: ALTER TABLE ADD column -> column must exist in CozoDB base schema
    // CREATE NODE/REL TABLE -> table must exist in CozoDB base schema
    for ddl in &kuzu_schema {
        let upper = ddl.to_uppercase();

        if upper.contains("ALTER TABLE") && upper.contains("ADD") {
            // e.g. "ALTER TABLE Symbol ADD parameters STRING DEFAULT ''"
            let parts: Vec<&str> = ddl.split_whitespace().collect();
            if parts.len() >= 5 {
                let table = parts[2].to_lowercase();
                let column = parts[4].to_lowercase();
                let cozo_table = kuzu_to_cozo_table_name(&table);
                if let Some(cols) = cozo_cols.get(cozo_table) {
                    assert!(
                        cols.contains(&column),
                        "Kuzu migration adds column '{column}' to '{table}' but CozoDB '{cozo_table}' \
                         doesn't have it. CozoDB columns: {cols:?}"
                    );
                }
            }
        }

        if upper.starts_with("CREATE NODE TABLE") {
            if let Some(name) = extract_after(&upper, "CREATE NODE TABLE IF NOT EXISTS ") {
                let lower = name.to_lowercase();
                let cozo_name = kuzu_to_cozo_table_name(&lower);
                assert!(
                    cozo_rels.contains(cozo_name),
                    "Kuzu migration creates node table '{name}' with no CozoDB equivalent"
                );
            }
        }

        if upper.starts_with("CREATE REL TABLE") {
            if let Some(name) = extract_after(&upper, "CREATE REL TABLE IF NOT EXISTS ") {
                let lower = name.to_lowercase();
                let rename_map: std::collections::HashMap<&str, &str> =
                    RENAMED_RELATIONS.iter().copied().collect();
                let cozo_name = rename_map
                    .get(lower.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or(lower);
                assert!(
                    cozo_rels.contains(&cozo_name),
                    "Kuzu migration creates rel table '{name}' with no CozoDB equivalent '{cozo_name}'"
                );
            }
        }
    }
}

#[test]
fn test_embeddings_use_sidecar_file_not_graph_db() {
    let tmp = tempfile::tempdir().unwrap();
    let emb_path = tmp.path().join("embeddings.bin");

    let embeddings = vec![
        ("sym::foo".to_string(), vec![0.1f32, 0.2, 0.3]),
        ("sym::bar".to_string(), vec![0.4, 0.5, 0.6]),
    ];
    infigraph_core::embed::save_embeddings(&emb_path, &embeddings).unwrap();

    assert!(
        emb_path.exists(),
        "embeddings.bin should be created as sidecar file"
    );
    assert!(
        emb_path.metadata().unwrap().len() > 0,
        "embeddings.bin should have content"
    );

    let loaded = infigraph_core::embed::load_embeddings(&emb_path).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].0, "sym::foo");
    assert_eq!(loaded[1].0, "sym::bar");
    assert!((loaded[0].1[0] - 0.1).abs() < 1e-6);
}
