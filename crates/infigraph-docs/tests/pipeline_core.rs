use infigraph_docs::store::{DocStore, PipelineCoreRecord};

fn test_store() -> (tempfile::TempDir, DocStore) {
    let tmp = tempfile::tempdir().unwrap();
    let store = DocStore::open(&tmp.path().join("test.kuzu")).unwrap();
    (tmp, store)
}

fn w2_pipeline() -> PipelineCoreRecord {
    PipelineCoreRecord {
        id: "pipeline::w2".into(),
        name: "W2 Metrics".into(),
        doc_id: "doc::w2".into(),
        plugin_id: "intuit".into(),
        inputs: vec!["tax_src.raw_w2_data".into(), "ref_data.employer_dim".into()],
        outputs: vec!["tax_dm.fact_w2_metric".into()],
    }
}

fn marketing_pipeline() -> PipelineCoreRecord {
    PipelineCoreRecord {
        id: "pipeline::marketing".into(),
        name: "Marketing Attributes".into(),
        doc_id: "doc::marketing".into(),
        plugin_id: "intuit".into(),
        inputs: vec!["tax_dm.fact_w2_metric".into(), "commerce_profile".into()],
        outputs: vec!["tax_rpt.rpt_marketing_attributes".into()],
    }
}

fn deceased_pipeline() -> PipelineCoreRecord {
    PipelineCoreRecord {
        id: "pipeline::deceased".into(),
        name: "Deceased Taxpayer Filter".into(),
        doc_id: "doc::deceased".into(),
        plugin_id: "intuit".into(),
        inputs: vec!["tax_rpt.rpt_marketing_attributes".into()],
        outputs: vec!["tax_rpt.rpt_deceased_filter".into()],
    }
}

// ==================== upsert + get ====================

#[test]
fn test_upsert_pipeline_core_and_get() {
    let (_tmp, store) = test_store();
    let w2 = w2_pipeline();
    store.upsert_pipeline_core(&w2).unwrap();

    let fetched = store.get_pipeline_core("pipeline::w2").unwrap().unwrap();
    assert_eq!(fetched.name, "W2 Metrics");
    assert_eq!(fetched.plugin_id, "intuit");
    assert_eq!(
        fetched.inputs,
        vec!["tax_src.raw_w2_data", "ref_data.employer_dim"]
    );
    assert_eq!(fetched.outputs, vec!["tax_dm.fact_w2_metric"]);
}

#[test]
fn test_get_pipeline_core_missing() {
    let (_tmp, store) = test_store();
    assert!(store
        .get_pipeline_core("pipeline::nonexistent")
        .unwrap()
        .is_none());
}

#[test]
fn test_upsert_pipeline_core_overwrites() {
    let (_tmp, store) = test_store();
    let w2 = w2_pipeline();
    store.upsert_pipeline_core(&w2).unwrap();

    let updated = PipelineCoreRecord {
        name: "W2 Metrics v2".into(),
        ..w2
    };
    store.upsert_pipeline_core(&updated).unwrap();

    let fetched = store.get_pipeline_core("pipeline::w2").unwrap().unwrap();
    assert_eq!(fetched.name, "W2 Metrics v2");

    let all = store.get_all_pipeline_cores(None).unwrap();
    assert_eq!(all.len(), 1);
}

// ==================== get_all + filter ====================

#[test]
fn test_get_all_pipeline_cores() {
    let (_tmp, store) = test_store();
    store.upsert_pipeline_core(&w2_pipeline()).unwrap();
    store.upsert_pipeline_core(&marketing_pipeline()).unwrap();

    let all = store.get_all_pipeline_cores(None).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn test_get_all_pipeline_cores_empty() {
    let (_tmp, store) = test_store();
    let all = store.get_all_pipeline_cores(None).unwrap();
    assert!(all.is_empty());
}

#[test]
fn test_get_all_pipeline_cores_filter_by_plugin() {
    let (_tmp, store) = test_store();
    store.upsert_pipeline_core(&w2_pipeline()).unwrap();

    let dbt = PipelineCoreRecord {
        id: "pipeline::dbt_model".into(),
        name: "dbt staging".into(),
        doc_id: "doc::dbt".into(),
        plugin_id: "dbt".into(),
        inputs: vec!["raw.events".into()],
        outputs: vec!["stg.events".into()],
    };
    store.upsert_pipeline_core(&dbt).unwrap();

    let intuit_only = store.get_all_pipeline_cores(Some("intuit")).unwrap();
    assert_eq!(intuit_only.len(), 1);
    assert_eq!(intuit_only[0].name, "W2 Metrics");

    let dbt_only = store.get_all_pipeline_cores(Some("dbt")).unwrap();
    assert_eq!(dbt_only.len(), 1);
    assert_eq!(dbt_only[0].name, "dbt staging");
}

// ==================== link dependencies ====================

#[test]
fn test_link_pipeline_dependencies() {
    let (_tmp, store) = test_store();
    store.upsert_pipeline_core(&w2_pipeline()).unwrap();
    store.upsert_pipeline_core(&marketing_pipeline()).unwrap();
    store.upsert_pipeline_core(&deceased_pipeline()).unwrap();

    let count = store.link_pipeline_dependencies().unwrap();
    // marketing consumes w2's output → 1 edge
    // deceased consumes marketing's output → 1 edge
    assert_eq!(count, 2, "expected 2 dependency edges");

    let deps = store.get_pipeline_deps().unwrap();
    assert_eq!(deps.len(), 2);

    let dep_pairs: Vec<(String, String)> = deps
        .iter()
        .map(|(f, t, _)| (f.clone(), t.clone()))
        .collect();
    assert!(dep_pairs.contains(&("Marketing Attributes".into(), "W2 Metrics".into())));
    assert!(dep_pairs.contains(&(
        "Deceased Taxpayer Filter".into(),
        "Marketing Attributes".into()
    )));
}

#[test]
fn test_link_pipeline_dependencies_idempotent() {
    let (_tmp, store) = test_store();
    store.upsert_pipeline_core(&w2_pipeline()).unwrap();
    store.upsert_pipeline_core(&marketing_pipeline()).unwrap();

    store.link_pipeline_dependencies().unwrap();
    let count = store.link_pipeline_dependencies().unwrap();
    assert_eq!(
        count, 1,
        "re-linking should produce same count (old edges cleared first)"
    );
}

#[test]
fn test_get_pipeline_deps_empty() {
    let (_tmp, store) = test_store();
    let deps = store.get_pipeline_deps().unwrap();
    assert!(deps.is_empty());
}

// ==================== impact analysis ====================

#[test]
fn test_impact_analysis_direct() {
    let (_tmp, store) = test_store();
    store.upsert_pipeline_core(&w2_pipeline()).unwrap();
    store.upsert_pipeline_core(&marketing_pipeline()).unwrap();
    store.link_pipeline_dependencies().unwrap();

    let results = store.impact_analysis("tax_src.raw_w2_data", 1).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].pipeline_name, "W2 Metrics");
    assert_eq!(results[0].impact_type, "direct");
    assert_eq!(results[0].depth, 1);
}

#[test]
fn test_impact_analysis_transitive() {
    let (_tmp, store) = test_store();
    store.upsert_pipeline_core(&w2_pipeline()).unwrap();
    store.upsert_pipeline_core(&marketing_pipeline()).unwrap();
    store.upsert_pipeline_core(&deceased_pipeline()).unwrap();
    store.link_pipeline_dependencies().unwrap();

    let results = store.impact_analysis("tax_dm.fact_w2_metric", 3).unwrap();
    assert!(
        results.len() >= 2,
        "expected at least 2 impacted pipelines, got {}",
        results.len()
    );

    let names: Vec<&str> = results.iter().map(|r| r.pipeline_name.as_str()).collect();
    assert!(
        names.contains(&"Marketing Attributes"),
        "missing Marketing Attributes: {:?}",
        names
    );
    assert!(
        names.contains(&"Deceased Taxpayer Filter"),
        "missing Deceased: {:?}",
        names
    );

    let marketing = results
        .iter()
        .find(|r| r.pipeline_name == "Marketing Attributes")
        .unwrap();
    assert_eq!(marketing.impact_type, "direct");
    assert_eq!(marketing.depth, 1);

    let deceased = results
        .iter()
        .find(|r| r.pipeline_name == "Deceased Taxpayer Filter")
        .unwrap();
    assert_eq!(deceased.impact_type, "transitive");
}

#[test]
fn test_impact_analysis_no_match() {
    let (_tmp, store) = test_store();
    store.upsert_pipeline_core(&w2_pipeline()).unwrap();
    let results = store.impact_analysis("nonexistent.table", 3).unwrap();
    assert!(results.is_empty());
}

// ==================== ensure_plugin_table + upsert_plugin_properties ====================

#[test]
fn test_ensure_plugin_table_and_query() {
    let (_tmp, store) = test_store();

    let columns = vec![
        ("scheduler_type".to_string(), "STRING".to_string()),
        ("compliance".to_string(), "STRING".to_string()),
    ];
    store.ensure_plugin_table("intuit", &columns).unwrap();

    let mut properties = serde_json::Map::new();
    properties.insert(
        "scheduler_type".into(),
        serde_json::Value::String("BPP".into()),
    );
    properties.insert(
        "compliance".into(),
        serde_json::Value::String("IRS 7216".into()),
    );

    store
        .upsert_plugin_properties("pipeline::w2", "intuit", &properties, &columns)
        .unwrap();

    let rows = store
        .query_plugin_table("intuit", "compliance", "7216")
        .unwrap();
    assert!(
        !rows.is_empty(),
        "expected compliance query to return results"
    );
}

#[test]
fn test_query_plugin_table_no_match() {
    let (_tmp, store) = test_store();

    let columns = vec![("compliance".to_string(), "STRING".to_string())];
    store.ensure_plugin_table("intuit", &columns).unwrap();

    let mut properties = serde_json::Map::new();
    properties.insert(
        "compliance".into(),
        serde_json::Value::String("IRS 7216".into()),
    );
    store
        .upsert_plugin_properties("pipeline::w2", "intuit", &properties, &columns)
        .unwrap();

    let rows = store
        .query_plugin_table("intuit", "compliance", "SOX")
        .unwrap();
    assert!(rows.is_empty(), "SOX should not match IRS 7216");
}

// ==================== link_pipeline_core_to_doc ====================

#[test]
fn test_link_pipeline_core_to_doc() {
    let (_tmp, store) = test_store();

    store.upsert_pipeline_core(&w2_pipeline()).unwrap();
    // Create a Document node first
    let conn = store.connection().unwrap();
    conn.query("CREATE (d:Document {id: 'doc::w2', file: 'confluence://TAXDATA/w2', title: 'W2 Pipeline', content_hash: 'abc123'})").unwrap();

    store
        .link_pipeline_core_to_doc("pipeline::w2", "doc::w2")
        .unwrap();

    // Verify edge exists
    let mut result = conn.query(
        "MATCH (p:PipelineCore)-[:DEFINED_IN]->(d:Document) WHERE p.id = 'pipeline::w2' RETURN d.id"
    ).unwrap();
    let row = result.next().expect("expected DEFINED_IN edge");
    assert_eq!(row[0].to_string(), "doc::w2");
}

// ==================== pipeline_core_count ====================

#[test]
fn test_pipeline_core_count() {
    let (_tmp, store) = test_store();
    assert_eq!(store.pipeline_core_count().unwrap(), 0);

    store.upsert_pipeline_core(&w2_pipeline()).unwrap();
    assert_eq!(store.pipeline_core_count().unwrap(), 1);

    store.upsert_pipeline_core(&marketing_pipeline()).unwrap();
    assert_eq!(store.pipeline_core_count().unwrap(), 2);
}

// ==================== cross-plugin dependencies ====================

#[test]
fn test_cross_plugin_dependency_linking() {
    let (_tmp, store) = test_store();

    let intuit_pipeline = PipelineCoreRecord {
        id: "pipeline::intuit_etl".into(),
        name: "Intuit ETL".into(),
        doc_id: "doc::intuit".into(),
        plugin_id: "intuit".into(),
        inputs: vec!["raw.source".into()],
        outputs: vec!["staging.processed".into()],
    };

    let dbt_pipeline = PipelineCoreRecord {
        id: "pipeline::dbt_transform".into(),
        name: "dbt Transform".into(),
        doc_id: "doc::dbt".into(),
        plugin_id: "dbt".into(),
        inputs: vec!["staging.processed".into()],
        outputs: vec!["analytics.metrics".into()],
    };

    store.upsert_pipeline_core(&intuit_pipeline).unwrap();
    store.upsert_pipeline_core(&dbt_pipeline).unwrap();

    let count = store.link_pipeline_dependencies().unwrap();
    assert_eq!(
        count, 1,
        "dbt consumes intuit's output → 1 cross-plugin edge"
    );

    let deps = store.get_pipeline_deps().unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].0, "dbt Transform");
    assert_eq!(deps[0].1, "Intuit ETL");
}

// ==================== deleting a document deletes its pipeline rows ====================

fn insert_document(store: &DocStore, id: &str) {
    store
        .connection()
        .unwrap()
        .query(&format!(
            "CREATE (d:Document {{id: '{id}', file: '{id}', title: 't', content_hash: 'h'}})"
        ))
        .unwrap();
}

fn compliance_columns() -> Vec<(String, String)> {
    vec![("compliance".to_string(), "STRING".to_string())]
}

fn compliance_props(value: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut properties = serde_json::Map::new();
    properties.insert("compliance".into(), serde_json::json!(value));
    properties
}

/// Deleting a document (a stale prune, or a changed file's replacement) used to
/// leave its `PipelineCore` and `Pipeline_<plugin_id>` rows behind, orphaned.
#[test]
fn test_deleting_a_document_deletes_its_pipeline_rows_and_only_its_own() {
    let (_tmp, store) = test_store();
    let columns = compliance_columns();
    store.ensure_plugin_table("intuit", &columns).unwrap();
    for (pipeline, doc, compliance) in [
        (w2_pipeline(), "doc::w2", "IRS 7216"),
        (marketing_pipeline(), "doc::marketing", "SOX"),
    ] {
        insert_document(&store, doc);
        store.upsert_pipeline_core(&pipeline).unwrap();
        store
            .upsert_plugin_properties(
                &pipeline.id,
                "intuit",
                &compliance_props(compliance),
                &columns,
            )
            .unwrap();
        store.link_pipeline_core_to_doc(&pipeline.id, doc).unwrap();
    }

    store.delete_docs_by_ids(&["doc::w2"]).unwrap();

    assert!(store.get_pipeline_core("pipeline::w2").unwrap().is_none());
    assert!(
        store
            .query_plugin_table("intuit", "compliance", "7216")
            .unwrap()
            .is_empty(),
        "the plugin table kept the deleted document's row"
    );
    assert!(
        store
            .get_pipeline_core("pipeline::marketing")
            .unwrap()
            .is_some(),
        "another document's pipeline was deleted"
    );
    assert_eq!(
        store
            .query_plugin_table("intuit", "compliance", "SOX")
            .unwrap()
            .len(),
        1
    );
}

/// A document with no pipeline rows, and a plugin whose table was never
/// created, are both fine.
#[test]
fn test_deleting_a_document_without_pipelines_is_a_no_op() {
    let (_tmp, store) = test_store();
    insert_document(&store, "doc::plain");
    store.delete_docs_by_ids(&["doc::plain"]).unwrap();
    store.upsert_pipeline_core(&w2_pipeline()).unwrap();
    store.delete_docs_by_ids(&["doc::w2"]).unwrap();
    assert!(store.get_pipeline_core("pipeline::w2").unwrap().is_none());
}

// ==================== typed plugin columns ====================

/// `upsert_plugin_properties` writes every property as a quoted string, which
/// is only right for STRING columns. Measured here against every declared type.
#[test]
fn test_typed_plugin_columns_round_trip() {
    let (_tmp, store) = test_store();
    let columns: Vec<(String, String)> = [
        ("s", "STRING"),
        ("n", "INT64"),
        ("b", "BOOL"),
        ("d", "DOUBLE"),
        ("l", "STRING[]"),
    ]
    .iter()
    .map(|(n, t)| (n.to_string(), t.to_string()))
    .collect();
    store.ensure_plugin_table("typed", &columns).unwrap();

    let mut properties = serde_json::Map::new();
    properties.insert("s".into(), serde_json::json!("hello"));
    properties.insert("n".into(), serde_json::json!(42));
    properties.insert("b".into(), serde_json::json!(true));
    properties.insert("d".into(), serde_json::json!(2.5));
    properties.insert("l".into(), serde_json::json!(["x", "y"]));
    store
        .upsert_plugin_properties("pipeline::typed::one", "typed", &properties, &columns)
        .unwrap();

    let conn = store.connection().unwrap();
    let mut rows = conn
        .query("MATCH (p:Pipeline_typed) RETURN p.s, p.n, p.b, p.d, p.l")
        .unwrap();
    let row = rows.next().expect("the typed row was not written");
    let shown: Vec<String> = row.iter().map(|v| v.to_string()).collect();
    assert_eq!(shown[0], "hello");
    assert_eq!(shown[1], "42");
    assert_eq!(shown[2], "True");
    assert_eq!(shown[3], "2.5");
    assert_eq!(shown[4], "[x,y]");
}

// ==================== schema drift ====================

/// `CREATE ... IF NOT EXISTS` does not alter a table, so a plugin that changed
/// its columns would keep writing against the old shape. That is an error that
/// says what to do, not a silent partial write.
#[test]
fn test_ensure_plugin_table_reports_schema_drift() {
    let (_tmp, store) = test_store();
    let one = compliance_columns();
    store.ensure_plugin_table("intuit", &one).unwrap();
    store.ensure_plugin_table("intuit", &one).unwrap(); // same shape: fine

    let mut two = one.clone();
    two.push(("owner".to_string(), "STRING".to_string()));
    let err = store
        .ensure_plugin_table("intuit", &two)
        .expect_err("a changed schema must be reported");
    let msg = err.to_string();
    assert!(msg.contains("intuit"), "{msg}");
    assert!(msg.contains("reindex-docs"), "{msg}");
}

// ==================== idempotent link, plugin ids ====================

/// `upsert_docs` replaces a changed document's node, so a kept pipeline has to
/// be linked to the new node again; linking twice must not add a second edge.
#[test]
fn test_linking_a_pipeline_to_its_document_twice_keeps_one_edge() {
    let (_tmp, store) = test_store();
    insert_document(&store, "doc::w2");
    store.upsert_pipeline_core(&w2_pipeline()).unwrap();
    store
        .link_pipeline_core_to_doc("pipeline::w2", "doc::w2")
        .unwrap();
    store
        .link_pipeline_core_to_doc("pipeline::w2", "doc::w2")
        .unwrap();
    let edges: Vec<String> = store
        .connection()
        .unwrap()
        .query("MATCH (:PipelineCore)-[r:DEFINED_IN]->(:Document) RETURN count(r)")
        .unwrap()
        .map(|r| r[0].to_string())
        .collect();
    assert_eq!(edges, vec!["1"]);
}

#[test]
fn test_ensure_plugin_table_refuses_an_id_that_is_not_a_plugin_id() {
    let (_tmp, store) = test_store();
    for bad in ["Intuit", "x y", "a-b", "1abc", "", "a)}; DROP"] {
        assert!(
            store
                .ensure_plugin_table(bad, &compliance_columns())
                .is_err(),
            "{bad:?} reached the DDL"
        );
    }
}
