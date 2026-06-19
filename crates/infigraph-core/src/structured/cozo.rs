use std::path::Path;

use anyhow::{bail, Context, Result};

use super::schema::{escape, interpolate_template, IngestResult, SchemaMeta};

fn cozo_col_type(col_type: &str) -> &str {
    match col_type {
        "STRING" => "String",
        "INT64" => "Int",
        "BOOL" => "Bool",
        "DOUBLE" => "Float",
        "STRING[]" => "String",
        _ => "String",
    }
}

fn cozo_col_default(col_type: &str) -> &str {
    match col_type {
        "STRING" | "STRING[]" => "\"\"",
        "INT64" => "0",
        "BOOL" => "false",
        "DOUBLE" => "0.0",
        _ => "\"\"",
    }
}

impl SchemaMeta {
    pub fn generate_cozo_ddl(&self) -> Vec<String> {
        let mut stmts = Vec::new();

        let cols: Vec<String> = self
            .columns
            .iter()
            .map(|c| {
                format!(
                    "{}: {} default {}",
                    c.name,
                    cozo_col_type(&c.col_type),
                    cozo_col_default(&c.col_type)
                )
            })
            .collect();
        let table_name = self.node_table.to_lowercase();
        if cols.is_empty() {
            stmts.push(format!(":create {table_name} {{id: String}}"));
        } else {
            stmts.push(format!(
                ":create {table_name} {{id: String => {}}}",
                cols.join(", ")
            ));
        }

        for edge in &self.edges {
            let edge_name = edge.name.to_lowercase();
            let prop_cols: Vec<String> = edge
                .properties
                .iter()
                .map(|c| {
                    format!(
                        ", {}: {} default {}",
                        c.name,
                        cozo_col_type(&c.col_type),
                        cozo_col_default(&c.col_type)
                    )
                })
                .collect();
            stmts.push(format!(
                ":create {edge_name} {{from_id: String, to_id: String{}}}",
                prop_cols.join("")
            ));
        }

        stmts
    }
}

pub fn ingest_data_cozo(
    db: &cozo::DbInstance,
    schema: &SchemaMeta,
    data: &[serde_json::Value],
) -> Result<IngestResult> {
    for ddl in schema.generate_cozo_ddl() {
        match db.run_script(
            &ddl,
            std::collections::BTreeMap::new(),
            cozo::ScriptMutability::Mutable,
        ) {
            Ok(_) => {}
            Err(e) => {
                let msg = format!("{e}");
                if !msg.contains("already exists") && !msg.contains("conflicts") {
                    bail!("DDL failed: {}", e);
                }
            }
        }
    }

    let table_name = schema.node_table.to_lowercase();
    let mut nodes_created = 0usize;
    let mut edges_created = 0usize;

    for (idx, record) in data.iter().enumerate() {
        let obj = record
            .as_object()
            .with_context(|| format!("record {} is not an object", idx))?;

        let id = if let Some(tmpl) = &schema.id_template {
            interpolate_template(tmpl, obj)
        } else if let Some(v) = obj.get("id") {
            v.as_str()
                .unwrap_or(&format!("{}_{}", schema.schema_id, idx))
                .to_string()
        } else {
            format!("{}_{}", schema.schema_id, idx)
        };

        let mut col_names = vec!["id".to_string()];
        let mut col_vals = vec![format!("\"{}\"", escape(&id))];
        for col in &schema.columns {
            let val = obj.get(&col.name);
            if col.required && val.is_none() {
                bail!("Record {}: missing required field '{}'", idx, col.name);
            }
            col_names.push(col.name.clone());
            col_vals.push(format_cozo_value(&col.col_type, val));
        }

        let put_script = format!(
            "?[{}] <- [[{}]]\n:put {table_name} {{{}}}",
            col_names.join(", "),
            col_vals.join(", "),
            col_names.join(", "),
        );
        db.run_script(
            &put_script,
            std::collections::BTreeMap::new(),
            cozo::ScriptMutability::Mutable,
        )
        .map_err(|e| anyhow::anyhow!("failed to create node {}: {}", id, e))?;
        nodes_created += 1;

        for edge in &schema.edges {
            let targets = match obj.get(&edge.source_field) {
                Some(serde_json::Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>(),
                Some(serde_json::Value::String(s)) => vec![s.clone()],
                _ => continue,
            };

            let edge_name = edge.name.to_lowercase();
            for target in &targets {
                let target_id = if edge.to_table == "Symbol" {
                    resolve_symbol_cozo(db, target).unwrap_or_else(|| {
                        eprintln!("[warn] unresolved symbol reference: '{}'", target);
                        target.clone()
                    })
                } else if let Some(lookup) = &edge.target_lookup {
                    format!("{}_{}", lookup, target)
                } else {
                    target.clone()
                };

                let to_table = edge.to_table.to_lowercase();
                let check_script = format!(
                    "?[count(id)] := *{to_table}{{id}}, id = \"{}\"",
                    escape(&target_id)
                );
                let target_exists = db
                    .run_script(
                        &check_script,
                        std::collections::BTreeMap::new(),
                        cozo::ScriptMutability::Immutable,
                    )
                    .ok()
                    .and_then(|r| {
                        r.rows.first().and_then(|row| row.first()).map(|v| match v {
                            cozo::DataValue::Num(cozo::Num::Int(i)) => *i > 0,
                            _ => false,
                        })
                    })
                    .unwrap_or(false);

                if target_exists {
                    let mut edge_col_names = vec!["from_id".to_string(), "to_id".to_string()];
                    let mut edge_col_vals = vec![
                        format!("\"{}\"", escape(&id)),
                        format!("\"{}\"", escape(&target_id)),
                    ];
                    for prop in &edge.properties {
                        edge_col_names.push(prop.name.clone());
                        edge_col_vals.push(format_cozo_value(&prop.col_type, obj.get(&prop.name)));
                    }

                    let put_edge = format!(
                        "?[{}] <- [[{}]]\n:put {edge_name} {{{}}}",
                        edge_col_names.join(", "),
                        edge_col_vals.join(", "),
                        edge_col_names.join(", "),
                    );
                    match db.run_script(
                        &put_edge,
                        std::collections::BTreeMap::new(),
                        cozo::ScriptMutability::Mutable,
                    ) {
                        Ok(_) => edges_created += 1,
                        Err(_) => {}
                    }
                }
            }
        }
    }

    Ok(IngestResult {
        nodes_created,
        edges_created,
    })
}

pub(crate) fn format_cozo_value(col_type: &str, val: Option<&serde_json::Value>) -> String {
    match val {
        None => match col_type {
            "STRING" | "STRING[]" => "\"\"".to_string(),
            "INT64" => "0".to_string(),
            "BOOL" => "false".to_string(),
            "DOUBLE" => "0.0".to_string(),
            _ => "\"\"".to_string(),
        },
        Some(v) => match col_type {
            "STRING" => format!(
                "\"{}\"",
                escape(&v.as_str().unwrap_or_default().to_string())
            ),
            "INT64" => v.as_i64().unwrap_or(0).to_string(),
            "BOOL" => v.as_bool().unwrap_or(false).to_string(),
            "DOUBLE" => v.as_f64().unwrap_or(0.0).to_string(),
            "STRING[]" => {
                if let Some(arr) = v.as_array() {
                    let items: Vec<String> = arr
                        .iter()
                        .filter_map(|s| s.as_str().map(|s| format!("\"{}\"", escape(s))))
                        .collect();
                    format!("[{}]", items.join(", "))
                } else {
                    "\"\"".to_string()
                }
            }
            _ => format!("\"{}\"", escape(&v.to_string())),
        },
    }
}

fn resolve_symbol_cozo(db: &cozo::DbInstance, reference: &str) -> Option<String> {
    let esc = reference.replace('"', "\\\"");
    let script =
        format!("?[id] := *symbol{{id, name}}, id = \"{esc}\" or name = \"{esc}\"\n:limit 1");
    db.run_script(
        &script,
        std::collections::BTreeMap::new(),
        cozo::ScriptMutability::Immutable,
    )
    .ok()
    .and_then(|r| {
        r.rows.first().and_then(|row| {
            row.first().map(|v| match v {
                cozo::DataValue::Str(s) => s.to_string(),
                _ => reference.to_string(),
            })
        })
    })
}

pub fn ingest_file_cozo(
    db: &cozo::DbInstance,
    schema: &SchemaMeta,
    data_path: &Path,
) -> Result<IngestResult> {
    let content = std::fs::read_to_string(data_path)
        .with_context(|| format!("failed to read data file: {}", data_path.display()))?;

    let ext = data_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let data: Vec<serde_json::Value> = match ext {
        "json" => {
            let parsed: serde_json::Value = serde_json::from_str(&content)
                .with_context(|| format!("invalid JSON: {}", data_path.display()))?;
            match parsed {
                serde_json::Value::Array(arr) => arr,
                obj @ serde_json::Value::Object(_) => vec![obj],
                _ => bail!("JSON must be an array or object"),
            }
        }
        "yaml" | "yml" => {
            let parsed: serde_json::Value = serde_yaml::from_str(&content)
                .with_context(|| format!("invalid YAML: {}", data_path.display()))?;
            match parsed {
                serde_json::Value::Array(arr) => arr,
                obj @ serde_json::Value::Object(_) => vec![obj],
                _ => bail!("YAML must be a sequence or mapping"),
            }
        }
        _ => bail!(
            "Unsupported data file format '{}' — use .json or .yaml/.yml",
            ext
        ),
    };

    ingest_data_cozo(db, schema, &data)
}

pub fn ingest_directory_cozo(
    db: &cozo::DbInstance,
    schema: &SchemaMeta,
    dir_path: &Path,
) -> Result<IngestResult> {
    if !dir_path.is_dir() {
        bail!("'{}' is not a directory", dir_path.display());
    }

    let mut total = IngestResult {
        nodes_created: 0,
        edges_created: 0,
    };

    for entry in std::fs::read_dir(dir_path)
        .with_context(|| format!("failed to read directory: {}", dir_path.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "json" | "yaml" | "yml") {
            continue;
        }
        let result = ingest_file_cozo(db, schema, &path)?;
        total.nodes_created += result.nodes_created;
        total.edges_created += result.edges_created;
    }

    Ok(total)
}
