mod cozo;
mod ingest;
mod schema;

pub use cozo::*;
pub use ingest::*;
pub use schema::*;

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_SCHEMA: &str = r#"
[schema]
schema_id = "ears"
name = "EARS Requirements"
node_table = "Requirement"
id_template = "ears_{req_id}"
searchable_fields = ["title", "requirement_text"]

[[schema.columns]]
name = "title"
col_type = "STRING"
required = true

[[schema.columns]]
name = "requirement_text"
col_type = "STRING"

[[schema.columns]]
name = "category"
col_type = "STRING"

[[schema.columns]]
name = "priority"
col_type = "INT64"

[[schema.edges]]
name = "TRACES_TO"
from_table = "Requirement"
to_table = "Symbol"
source_field = "traces_to"
"#;

    #[test]
    fn test_parse_schema() {
        let schema: StructuredSchema = toml::from_str(SAMPLE_SCHEMA).unwrap();
        assert_eq!(schema.schema.schema_id, "ears");
        assert_eq!(schema.schema.node_table, "Requirement");
        assert_eq!(schema.schema.columns.len(), 4);
        assert!(schema.schema.columns[0].required);
        assert_eq!(schema.schema.edges.len(), 1);
        assert_eq!(schema.schema.edges[0].name, "TRACES_TO");
        schema.schema.validate().unwrap();
    }

    #[test]
    fn test_generate_ddl() {
        let schema: StructuredSchema = toml::from_str(SAMPLE_SCHEMA).unwrap();
        let ddl = schema.schema.generate_ddl();
        assert_eq!(ddl.len(), 2);
        assert!(ddl[0].contains("Requirement"));
        assert!(ddl[0].contains("title STRING"));
        assert!(ddl[0].contains("priority INT64"));
        assert!(ddl[1].contains("TRACES_TO"));
        assert!(ddl[1].contains("FROM Requirement TO Symbol"));
    }

    #[test]
    fn test_invalid_schema_id() {
        let toml_str = r#"
[schema]
schema_id = "Bad"
name = "Bad"
node_table = "Bad"
"#;
        let schema: StructuredSchema = toml::from_str(toml_str).unwrap();
        assert!(schema.schema.validate().is_err());
    }

    #[test]
    fn test_id_template_interpolation() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "req_id".to_string(),
            serde_json::Value::String("REQ-001".to_string()),
        );
        obj.insert(
            "category".to_string(),
            serde_json::Value::String("security".to_string()),
        );
        let result = schema::interpolate_template("ears_{req_id}_{category}", &obj);
        assert_eq!(result, "ears_REQ-001_security");
    }

    #[test]
    fn test_format_value() {
        assert_eq!(
            schema::format_value("STRING", Some(&serde_json::json!("hello"))),
            "'hello'"
        );
        assert_eq!(
            schema::format_value("INT64", Some(&serde_json::json!(42))),
            "42"
        );
        assert_eq!(
            schema::format_value("BOOL", Some(&serde_json::json!(true))),
            "true"
        );
        assert_eq!(schema::format_value("STRING", None), "''");
        assert_eq!(schema::format_value("INT64", None), "0");
    }

    fn simple_schema() -> SchemaMeta {
        SchemaMeta {
            schema_id: "test_items".to_string(),
            name: "Test Items".to_string(),
            node_table: "TestItem".to_string(),
            columns: vec![
                ColumnDef {
                    name: "title".to_string(),
                    col_type: "STRING".to_string(),
                    required: true,
                },
                ColumnDef {
                    name: "priority".to_string(),
                    col_type: "INT64".to_string(),
                    required: false,
                },
            ],
            edges: vec![],
            searchable_fields: vec![],
            id_template: Some("item_{item_id}".to_string()),
        }
    }

    fn kuzu_conn() -> (tempfile::TempDir, crate::graph::GraphStore) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = crate::graph::GraphStore::open(&dir.path().join("graph")).unwrap();
        (dir, store)
    }

    #[test]
    fn test_ingest_data_with_kuzu() {
        let (_dir, store) = kuzu_conn();
        let conn = store.connection().unwrap();
        let schema = simple_schema();

        let data = vec![
            serde_json::json!({"item_id": "A1", "title": "First", "priority": 1}),
            serde_json::json!({"item_id": "A2", "title": "Second", "priority": 2}),
            serde_json::json!({"item_id": "A3", "title": "Third", "priority": 3}),
        ];

        let result = ingest_data(&conn, &schema, &data).unwrap();
        assert_eq!(result.nodes_created, 3);

        let mut qr = conn
            .query("MATCH (t:TestItem) RETURN t.id ORDER BY t.id")
            .unwrap();
        let mut ids = Vec::new();
        while let Some(row) = qr.next() {
            ids.push(row[0].to_string());
        }
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().any(|id| id.contains("item_A1")));
    }

    #[test]
    fn test_ingest_file_json() {
        let (_dir, store) = kuzu_conn();
        let conn = store.connection().unwrap();
        let schema = simple_schema();

        let tmp = tempfile::NamedTempFile::with_suffix(".json").unwrap();
        std::fs::write(
            tmp.path(),
            r#"[{"item_id":"J1","title":"JSON item","priority":5}]"#,
        )
        .unwrap();

        let result = ingest_file(&conn, &schema, tmp.path()).unwrap();
        assert_eq!(result.nodes_created, 1);
    }

    #[test]
    fn test_ingest_file_yaml() {
        let (_dir, store) = kuzu_conn();
        let conn = store.connection().unwrap();
        let schema = simple_schema();

        let tmp = tempfile::NamedTempFile::with_suffix(".yaml").unwrap();
        std::fs::write(
            tmp.path(),
            "- item_id: Y1\n  title: YAML item\n  priority: 10\n",
        )
        .unwrap();

        let result = ingest_file(&conn, &schema, tmp.path()).unwrap();
        assert_eq!(result.nodes_created, 1);
    }

    #[test]
    fn test_ingest_directory() {
        let (_dir, store) = kuzu_conn();
        let conn = store.connection().unwrap();
        let schema = simple_schema();

        let data_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            data_dir.path().join("batch1.json"),
            r#"[{"item_id":"D1","title":"Dir item 1","priority":1}]"#,
        )
        .unwrap();
        std::fs::write(
            data_dir.path().join("batch2.json"),
            r#"[{"item_id":"D2","title":"Dir item 2","priority":2}]"#,
        )
        .unwrap();
        std::fs::write(data_dir.path().join("ignore.txt"), "not a data file").unwrap();

        let result = ingest_directory(&conn, &schema, data_dir.path()).unwrap();
        assert_eq!(result.nodes_created, 2);
    }

    #[test]
    fn test_required_field_missing() {
        let (_dir, store) = kuzu_conn();
        let conn = store.connection().unwrap();
        let schema = simple_schema();

        let data = vec![serde_json::json!({"item_id": "X1", "priority": 1})];
        let err = ingest_data(&conn, &schema, &data).unwrap_err();
        assert!(
            err.to_string().contains("title"),
            "error should mention missing field 'title': {err}"
        );
    }

    #[test]
    fn test_edge_creation_between_nodes() {
        let (_dir, store) = kuzu_conn();
        let conn = store.connection().unwrap();

        let schema = SchemaMeta {
            schema_id: "linked".to_string(),
            name: "Linked".to_string(),
            node_table: "LinkedNode".to_string(),
            columns: vec![ColumnDef {
                name: "label".to_string(),
                col_type: "STRING".to_string(),
                required: false,
            }],
            edges: vec![EdgeDef {
                name: "LINKS_TO".to_string(),
                from_table: "LinkedNode".to_string(),
                to_table: "LinkedNode".to_string(),
                properties: vec![],
                source_field: "links".to_string(),
                target_lookup: None,
            }],
            searchable_fields: vec![],
            id_template: None,
        };

        let data = vec![
            serde_json::json!({"id": "n2", "label": "Node 2"}),
            serde_json::json!({"id": "n1", "label": "Node 1", "links": ["n2"]}),
        ];

        let result = ingest_data(&conn, &schema, &data).unwrap();
        assert_eq!(result.nodes_created, 2);
        assert_eq!(result.edges_created, 1);
    }

    #[test]
    fn test_id_template_with_missing_field() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "req_id".to_string(),
            serde_json::Value::String("REQ-001".to_string()),
        );
        let result = schema::interpolate_template("{req_id}_{category}", &obj);
        assert_eq!(
            result, "REQ-001_{category}",
            "missing field should remain as literal placeholder"
        );
    }

    #[test]
    fn test_edge_to_nonexistent_target() {
        let (_dir, store) = kuzu_conn();
        let conn = store.connection().unwrap();

        let schema = SchemaMeta {
            schema_id: "orphan".to_string(),
            name: "Orphan".to_string(),
            node_table: "OrphanNode".to_string(),
            columns: vec![],
            edges: vec![EdgeDef {
                name: "REFS".to_string(),
                from_table: "OrphanNode".to_string(),
                to_table: "OrphanNode".to_string(),
                properties: vec![],
                source_field: "refs".to_string(),
                target_lookup: None,
            }],
            searchable_fields: vec![],
            id_template: None,
        };

        let data = vec![serde_json::json!({"id": "exists", "refs": ["does_not_exist"]})];

        let result = ingest_data(&conn, &schema, &data).unwrap();
        assert_eq!(result.nodes_created, 1);
        assert_eq!(
            result.edges_created, 0,
            "edge to nonexistent target should silently fail"
        );
    }

    #[test]
    fn test_unsupported_file_format() {
        let (_dir, store) = kuzu_conn();
        let conn = store.connection().unwrap();
        let schema = simple_schema();

        let tmp = tempfile::NamedTempFile::with_suffix(".csv").unwrap();
        std::fs::write(tmp.path(), "a,b\n1,2").unwrap();

        let err = ingest_file(&conn, &schema, tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("Unsupported"),
            "should mention unsupported format: {err}"
        );
    }

    #[test]
    fn test_schema_discovery_project_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let schema_dir = dir.path().join(".infigraph/structured-schemas");
        std::fs::create_dir_all(&schema_dir).unwrap();
        std::fs::write(
            schema_dir.join("test.toml"),
            r#"
[schema]
schema_id = "found"
name = "Found"
node_table = "Found"
"#,
        )
        .unwrap();

        let schemas = discover_schemas(dir.path()).unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].1.schema.schema_id, "found");
    }

    #[test]
    fn test_schema_discovery_terragraph_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let schema_dir = dir.path().join(".terragraph/schemas");
        std::fs::create_dir_all(&schema_dir).unwrap();
        std::fs::write(
            schema_dir.join("tg.toml"),
            r#"
[schema]
schema_id = "tg_schema"
name = "TG Schema"
node_table = "TGNode"
"#,
        )
        .unwrap();

        let schemas = discover_schemas(dir.path()).unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].1.schema.schema_id, "tg_schema");
    }

    // ── Cozo structured ingestion tests ──────────────────────────────

    fn cozo_db() -> (tempfile::TempDir, ::cozo::DbInstance) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = ::cozo::DbInstance::new(
            "sqlite",
            dir.path().join("cozo.db").to_str().unwrap(),
            Default::default(),
        )
        .unwrap();
        (dir, db)
    }

    #[test]
    fn test_cozo_generate_ddl() {
        let schema = simple_schema();
        let ddl = schema.generate_cozo_ddl();
        assert_eq!(ddl.len(), 1);
        assert!(
            ddl[0].contains("testitem"),
            "table name should be lowercased"
        );
        assert!(
            ddl[0].contains("title: String"),
            "should have String column"
        );
        assert!(ddl[0].contains("priority: Int"), "should have Int column");
    }

    #[test]
    fn test_cozo_ingest_data() {
        let (_dir, db) = cozo_db();
        let schema = simple_schema();

        let data = vec![
            serde_json::json!({"item_id": "A1", "title": "First", "priority": 1}),
            serde_json::json!({"item_id": "A2", "title": "Second", "priority": 2}),
            serde_json::json!({"item_id": "A3", "title": "Third", "priority": 3}),
        ];

        let result = ingest_data_cozo(&db, &schema, &data).unwrap();
        assert_eq!(result.nodes_created, 3);

        let r = db
            .run_script(
                "?[id] := *testitem{id}\n:order id",
                std::collections::BTreeMap::new(),
                ::cozo::ScriptMutability::Immutable,
            )
            .unwrap();
        assert_eq!(r.rows.len(), 3);
    }

    #[test]
    fn test_cozo_ingest_file_json() {
        let (_dir, db) = cozo_db();
        let schema = simple_schema();

        let tmp = tempfile::NamedTempFile::with_suffix(".json").unwrap();
        std::fs::write(
            tmp.path(),
            r#"[{"item_id":"J1","title":"JSON item","priority":5}]"#,
        )
        .unwrap();

        let result = ingest_file_cozo(&db, &schema, tmp.path()).unwrap();
        assert_eq!(result.nodes_created, 1);
    }

    #[test]
    fn test_cozo_ingest_file_yaml() {
        let (_dir, db) = cozo_db();
        let schema = simple_schema();

        let tmp = tempfile::NamedTempFile::with_suffix(".yaml").unwrap();
        std::fs::write(
            tmp.path(),
            "- item_id: Y1\n  title: YAML item\n  priority: 10\n",
        )
        .unwrap();

        let result = ingest_file_cozo(&db, &schema, tmp.path()).unwrap();
        assert_eq!(result.nodes_created, 1);
    }

    #[test]
    fn test_cozo_ingest_directory() {
        let (_dir, db) = cozo_db();
        let schema = simple_schema();

        let data_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            data_dir.path().join("batch1.json"),
            r#"[{"item_id":"D1","title":"Dir item 1","priority":1}]"#,
        )
        .unwrap();
        std::fs::write(
            data_dir.path().join("batch2.json"),
            r#"[{"item_id":"D2","title":"Dir item 2","priority":2}]"#,
        )
        .unwrap();

        let result = ingest_directory_cozo(&db, &schema, data_dir.path()).unwrap();
        assert_eq!(result.nodes_created, 2);
    }

    #[test]
    fn test_cozo_required_field_missing() {
        let (_dir, db) = cozo_db();
        let schema = simple_schema();

        let data = vec![serde_json::json!({"item_id": "X1", "priority": 1})];
        let err = ingest_data_cozo(&db, &schema, &data).unwrap_err();
        assert!(
            err.to_string().contains("title"),
            "error should mention missing field: {err}"
        );
    }

    #[test]
    fn test_cozo_edge_creation() {
        let (_dir, db) = cozo_db();

        let schema = SchemaMeta {
            schema_id: "linked".to_string(),
            name: "Linked".to_string(),
            node_table: "LinkedNode".to_string(),
            columns: vec![ColumnDef {
                name: "label".to_string(),
                col_type: "STRING".to_string(),
                required: false,
            }],
            edges: vec![EdgeDef {
                name: "LINKS_TO".to_string(),
                from_table: "LinkedNode".to_string(),
                to_table: "LinkedNode".to_string(),
                properties: vec![],
                source_field: "links".to_string(),
                target_lookup: None,
            }],
            searchable_fields: vec![],
            id_template: None,
        };

        let data = vec![
            serde_json::json!({"id": "n2", "label": "Node 2"}),
            serde_json::json!({"id": "n1", "label": "Node 1", "links": ["n2"]}),
        ];

        let result = ingest_data_cozo(&db, &schema, &data).unwrap();
        assert_eq!(result.nodes_created, 2);
        assert_eq!(result.edges_created, 1);
    }

    #[test]
    fn test_cozo_edge_to_nonexistent_target() {
        let (_dir, db) = cozo_db();

        let schema = SchemaMeta {
            schema_id: "orphan".to_string(),
            name: "Orphan".to_string(),
            node_table: "OrphanNode".to_string(),
            columns: vec![],
            edges: vec![EdgeDef {
                name: "REFS".to_string(),
                from_table: "OrphanNode".to_string(),
                to_table: "OrphanNode".to_string(),
                properties: vec![],
                source_field: "refs".to_string(),
                target_lookup: None,
            }],
            searchable_fields: vec![],
            id_template: None,
        };

        let data = vec![serde_json::json!({"id": "exists", "refs": ["does_not_exist"]})];

        let result = ingest_data_cozo(&db, &schema, &data).unwrap();
        assert_eq!(result.nodes_created, 1);
        assert_eq!(result.edges_created, 0);
    }

    #[test]
    fn test_cozo_format_value() {
        use super::cozo as cozo_ingest;
        assert_eq!(
            cozo_ingest::format_cozo_value("STRING", Some(&serde_json::json!("hello"))),
            "\"hello\""
        );
        assert_eq!(
            cozo_ingest::format_cozo_value("INT64", Some(&serde_json::json!(42))),
            "42"
        );
        assert_eq!(
            cozo_ingest::format_cozo_value("BOOL", Some(&serde_json::json!(true))),
            "true"
        );
        assert_eq!(cozo_ingest::format_cozo_value("STRING", None), "\"\"");
        assert_eq!(cozo_ingest::format_cozo_value("INT64", None), "0");
    }
}
