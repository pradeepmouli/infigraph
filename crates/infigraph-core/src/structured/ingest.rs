use std::path::Path;

use anyhow::{bail, Context, Result};

use super::schema::{escape, format_value, interpolate_template, IngestResult, SchemaMeta};

pub fn ingest_data(
    conn: &kuzu::Connection<'_>,
    schema: &SchemaMeta,
    data: &[serde_json::Value],
) -> Result<IngestResult> {
    for ddl in schema.generate_ddl() {
        match conn.query(&ddl) {
            Ok(_) => {}
            Err(e) => {
                let msg = format!("{e}");
                if !msg.contains("already exists") {
                    bail!("DDL failed: {}", e);
                }
            }
        }
    }

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

        let mut props = vec![format!("id: '{}'", escape(&id))];
        for col in &schema.columns {
            let val = obj.get(&col.name);
            if col.required && val.is_none() {
                bail!("Record {}: missing required field '{}'", idx, col.name);
            }
            let formatted = format_value(&col.col_type, val);
            props.push(format!("{}: {}", col.name, formatted));
        }

        let cypher = format!("CREATE (:{} {{{}}})", schema.node_table, props.join(", "));
        conn.query(&cypher)
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

            for target in &targets {
                let target_id = if edge.to_table == "Symbol" {
                    resolve_symbol(conn, target).unwrap_or_else(|| {
                        eprintln!("[warn] unresolved symbol reference: '{}'", target);
                        target.clone()
                    })
                } else if let Some(lookup) = &edge.target_lookup {
                    format!("{}_{}", lookup, target)
                } else {
                    target.clone()
                };

                let mut edge_props = String::new();
                if !edge.properties.is_empty() {
                    let p: Vec<String> = edge
                        .properties
                        .iter()
                        .map(|c| {
                            let val = obj.get(&c.name);
                            format!("{}: {}", c.name, format_value(&c.col_type, val))
                        })
                        .collect();
                    edge_props = format!(", {}", p.join(", "));
                }

                let edge_prop_str = if edge_props.is_empty() {
                    String::new()
                } else {
                    format!("{{{}}}", edge_props.trim_start_matches(", "))
                };
                let cypher = format!(
                    "MATCH (a:{} {{id: '{}'}}), (b:{} {{id: '{}'}}) CREATE (a)-[:{}{}]->(b)",
                    schema.node_table,
                    escape(&id),
                    edge.to_table,
                    escape(&target_id),
                    edge.name,
                    edge_prop_str,
                );
                let check_query = format!(
                    "MATCH (a:{} {{id: '{}'}}), (b:{} {{id: '{}'}}) RETURN count(*)",
                    schema.node_table,
                    escape(&id),
                    edge.to_table,
                    escape(&target_id),
                );
                let target_exists = conn
                    .query(&check_query)
                    .ok()
                    .and_then(|mut qr| {
                        qr.next()
                            .map(|row| row[0].to_string().parse::<u64>().unwrap_or(0) > 0)
                    })
                    .unwrap_or(false);

                if target_exists {
                    match conn.query(&cypher) {
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

fn resolve_symbol(conn: &kuzu::Connection<'_>, reference: &str) -> Option<String> {
    let esc = reference.replace('\'', "\\'");
    let query = format!(
        "MATCH (s:Symbol) WHERE s.id = '{}' OR s.name = '{}' RETURN s.id LIMIT 1",
        esc, esc
    );
    conn.query(&query)
        .ok()
        .and_then(|mut result| result.next().map(|row| row[0].to_string()))
}

pub fn ingest_directory(
    conn: &kuzu::Connection<'_>,
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
        let result = ingest_file(conn, schema, &path)?;
        total.nodes_created += result.nodes_created;
        total.edges_created += result.edges_created;
    }

    Ok(total)
}

pub fn ingest_file(
    conn: &kuzu::Connection<'_>,
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

    ingest_data(conn, schema, &data)
}
