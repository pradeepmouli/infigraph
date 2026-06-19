use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct StructuredSchema {
    pub schema: SchemaMeta,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SchemaMeta {
    pub schema_id: String,
    pub name: String,
    pub node_table: String,
    #[serde(default)]
    pub columns: Vec<ColumnDef>,
    #[serde(default)]
    pub edges: Vec<EdgeDef>,
    #[serde(default)]
    pub searchable_fields: Vec<String>,
    #[serde(default)]
    pub id_template: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub col_type: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EdgeDef {
    pub name: String,
    pub from_table: String,
    pub to_table: String,
    #[serde(default)]
    pub properties: Vec<ColumnDef>,
    pub source_field: String,
    #[serde(default)]
    pub target_lookup: Option<String>,
}

const VALID_COL_TYPES: &[&str] = &["STRING", "INT64", "BOOL", "DOUBLE", "STRING[]"];

impl SchemaMeta {
    pub fn validate(&self) -> Result<()> {
        let id_re = regex::Regex::new(r"^[a-z][a-z0-9_]{0,31}$").unwrap();
        if !id_re.is_match(&self.schema_id) {
            bail!(
                "Invalid schema_id '{}': must match ^[a-z][a-z0-9_]{{0,31}}$",
                self.schema_id
            );
        }

        let col_re = regex::Regex::new(r"^[a-z][a-z0-9_]{0,63}$").unwrap();
        for col in &self.columns {
            if !col_re.is_match(&col.name) {
                bail!("Invalid column name '{}' in schema '{}'", col.name, self.schema_id);
            }
            if !VALID_COL_TYPES.contains(&col.col_type.as_str()) {
                bail!(
                    "Invalid col_type '{}' for column '{}': must be one of {:?}",
                    col.col_type, col.name, VALID_COL_TYPES
                );
            }
        }

        if self.node_table.is_empty() {
            bail!("node_table must not be empty");
        }

        Ok(())
    }

    pub fn generate_ddl(&self) -> Vec<String> {
        let mut stmts = Vec::new();

        let mut col_defs = vec!["id STRING".to_string()];
        for col in &self.columns {
            col_defs.push(format!("{} {}", col.name, col.col_type));
        }
        stmts.push(format!(
            "CREATE NODE TABLE IF NOT EXISTS {}({}, PRIMARY KEY(id))",
            self.node_table,
            col_defs.join(", ")
        ));

        for edge in &self.edges {
            let mut props = String::new();
            if !edge.properties.is_empty() {
                let p: Vec<String> = edge.properties.iter()
                    .map(|c| format!("{} {}", c.name, c.col_type))
                    .collect();
                props = format!(", {}", p.join(", "));
            }
            stmts.push(format!(
                "CREATE REL TABLE IF NOT EXISTS {}(FROM {} TO {}{})",
                edge.name, edge.from_table, edge.to_table, props
            ));
        }

        stmts
    }
}

pub fn discover_schemas(project_root: &Path) -> Result<Vec<(PathBuf, StructuredSchema)>> {
    let mut schemas = Vec::new();

    let search_dirs = [
        project_root.join(".infigraph/structured-schemas"),
        project_root.join(".terragraph/schemas"),
        dirs_next::home_dir()
            .unwrap_or_default()
            .join(".infigraph/structured-schemas"),
    ];

    for dir in &search_dirs {
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "toml").unwrap_or(false) {
                let content = std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read schema: {}", path.display()))?;
                let schema: StructuredSchema = toml::from_str(&content)
                    .with_context(|| format!("invalid schema TOML: {}", path.display()))?;
                schema.schema.validate()
                    .with_context(|| format!("schema validation failed: {}", path.display()))?;
                schemas.push((path, schema));
            }
        }
    }

    Ok(schemas)
}

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
        let obj = record.as_object()
            .with_context(|| format!("record {} is not an object", idx))?;

        let id = if let Some(tmpl) = &schema.id_template {
            interpolate_template(tmpl, obj)
        } else if let Some(v) = obj.get("id") {
            v.as_str().unwrap_or(&format!("{}_{}", schema.schema_id, idx)).to_string()
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

        let cypher = format!(
            "CREATE (:{} {{{}}})",
            schema.node_table,
            props.join(", ")
        );
        conn.query(&cypher)
            .map_err(|e| anyhow::anyhow!("failed to create node {}: {}", id, e))?;
        nodes_created += 1;

        for edge in &schema.edges {
            let targets = match obj.get(&edge.source_field) {
                Some(serde_json::Value::Array(arr)) => {
                    arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>()
                }
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
                    let p: Vec<String> = edge.properties.iter()
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
                    schema.node_table, escape(&id),
                    edge.to_table, escape(&target_id),
                    edge.name,
                    edge_prop_str,
                );
                let check_query = format!(
                    "MATCH (a:{} {{id: '{}'}}), (b:{} {{id: '{}'}}) RETURN count(*)",
                    schema.node_table, escape(&id),
                    edge.to_table, escape(&target_id),
                );
                let target_exists = conn.query(&check_query).ok().and_then(|mut qr| {
                    qr.next().map(|row| row[0].to_string().parse::<u64>().unwrap_or(0) > 0)
                }).unwrap_or(false);

                if target_exists {
                    match conn.query(&cypher) {
                        Ok(_) => edges_created += 1,
                        Err(_) => {}
                    }
                }
            }
        }
    }

    Ok(IngestResult { nodes_created, edges_created })
}

#[derive(Debug)]
pub struct IngestResult {
    pub nodes_created: usize,
    pub edges_created: usize,
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

fn format_value(col_type: &str, val: Option<&serde_json::Value>) -> String {
    match val {
        None => match col_type {
            "STRING" => "''".to_string(),
            "INT64" => "0".to_string(),
            "BOOL" => "false".to_string(),
            "DOUBLE" => "0.0".to_string(),
            "STRING[]" => "[]".to_string(),
            _ => "''".to_string(),
        },
        Some(v) => match col_type {
            "STRING" => format!("'{}'", escape(&v.to_string().trim_matches('"').to_string())),
            "INT64" => v.as_i64().unwrap_or(0).to_string(),
            "BOOL" => v.as_bool().unwrap_or(false).to_string(),
            "DOUBLE" => v.as_f64().unwrap_or(0.0).to_string(),
            "STRING[]" => {
                if let Some(arr) = v.as_array() {
                    let items: Vec<String> = arr.iter()
                        .filter_map(|i| i.as_str())
                        .map(|s| format!("'{}'", escape(s)))
                        .collect();
                    format!("[{}]", items.join(", "))
                } else {
                    "[]".to_string()
                }
            }
            _ => format!("'{}'", escape(&v.to_string())),
        },
    }
}

fn resolve_symbol(conn: &kuzu::Connection<'_>, reference: &str) -> Option<String> {
    let esc = reference.replace('\'', "\\'");
    let query = format!(
        "MATCH (s:Symbol) WHERE s.id = '{}' OR s.name = '{}' RETURN s.id LIMIT 1",
        esc, esc
    );
    conn.query(&query).ok().and_then(|mut result| {
        result.next().map(|row| row[0].to_string())
    })
}

fn interpolate_template(tmpl: &str, obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut result = tmpl.to_string();
    for (key, val) in obj {
        let placeholder = format!("{{{}}}", key);
        let replacement = match val {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string().trim_matches('"').to_string(),
        };
        result = result.replace(&placeholder, &replacement);
    }
    result
}

pub fn ingest_directory(
    conn: &kuzu::Connection<'_>,
    schema: &SchemaMeta,
    dir_path: &Path,
) -> Result<IngestResult> {
    if !dir_path.is_dir() {
        bail!("'{}' is not a directory", dir_path.display());
    }

    let mut total = IngestResult { nodes_created: 0, edges_created: 0 };

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
        _ => bail!("Unsupported data file format '{}' — use .json or .yaml/.yml", ext),
    };

    ingest_data(conn, schema, &data)
}

// ── CozoDB structured ingestion ──────────────────────────────────────

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

        let cols: Vec<String> = self.columns.iter()
            .map(|c| format!("{}: {} default {}", c.name, cozo_col_type(&c.col_type), cozo_col_default(&c.col_type)))
            .collect();
        let table_name = self.node_table.to_lowercase();
        if cols.is_empty() {
            stmts.push(format!(":create {table_name} {{id: String}}"));
        } else {
            stmts.push(format!(":create {table_name} {{id: String => {}}}", cols.join(", ")));
        }

        for edge in &self.edges {
            let edge_name = edge.name.to_lowercase();
            let prop_cols: Vec<String> = edge.properties.iter()
                .map(|c| format!(", {}: {} default {}", c.name, cozo_col_type(&c.col_type), cozo_col_default(&c.col_type)))
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
        match db.run_script(&ddl, std::collections::BTreeMap::new(), cozo::ScriptMutability::Mutable) {
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
        let obj = record.as_object()
            .with_context(|| format!("record {} is not an object", idx))?;

        let id = if let Some(tmpl) = &schema.id_template {
            interpolate_template(tmpl, obj)
        } else if let Some(v) = obj.get("id") {
            v.as_str().unwrap_or(&format!("{}_{}", schema.schema_id, idx)).to_string()
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
        db.run_script(&put_script, std::collections::BTreeMap::new(), cozo::ScriptMutability::Mutable)
            .map_err(|e| anyhow::anyhow!("failed to create node {}: {}", id, e))?;
        nodes_created += 1;

        for edge in &schema.edges {
            let targets = match obj.get(&edge.source_field) {
                Some(serde_json::Value::Array(arr)) => {
                    arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>()
                }
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
                let target_exists = db.run_script(
                    &check_script,
                    std::collections::BTreeMap::new(),
                    cozo::ScriptMutability::Immutable,
                ).ok().and_then(|r| {
                    r.rows.first().and_then(|row| row.first()).map(|v| {
                        match v {
                            cozo::DataValue::Num(cozo::Num::Int(i)) => *i > 0,
                            _ => false,
                        }
                    })
                }).unwrap_or(false);

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
                    match db.run_script(&put_edge, std::collections::BTreeMap::new(), cozo::ScriptMutability::Mutable) {
                        Ok(_) => edges_created += 1,
                        Err(_) => {}
                    }
                }
            }
        }
    }

    Ok(IngestResult { nodes_created, edges_created })
}

fn format_cozo_value(col_type: &str, val: Option<&serde_json::Value>) -> String {
    match val {
        None => match col_type {
            "STRING" | "STRING[]" => "\"\"".to_string(),
            "INT64" => "0".to_string(),
            "BOOL" => "false".to_string(),
            "DOUBLE" => "0.0".to_string(),
            _ => "\"\"".to_string(),
        },
        Some(v) => match col_type {
            "STRING" => format!("\"{}\"", escape(&v.as_str().unwrap_or_default().to_string())),
            "INT64" => v.as_i64().unwrap_or(0).to_string(),
            "BOOL" => v.as_bool().unwrap_or(false).to_string(),
            "DOUBLE" => v.as_f64().unwrap_or(0.0).to_string(),
            "STRING[]" => {
                if let Some(arr) = v.as_array() {
                    let items: Vec<String> = arr.iter()
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
    let script = format!(
        "?[id] := *symbol{{id, name}}, id = \"{esc}\" or name = \"{esc}\"\n:limit 1"
    );
    db.run_script(&script, std::collections::BTreeMap::new(), cozo::ScriptMutability::Immutable)
        .ok()
        .and_then(|r| r.rows.first().and_then(|row| row.first().map(|v| {
            match v {
                cozo::DataValue::Str(s) => s.to_string(),
                _ => reference.to_string(),
            }
        })))
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
        _ => bail!("Unsupported data file format '{}' — use .json or .yaml/.yml", ext),
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

    let mut total = IngestResult { nodes_created: 0, edges_created: 0 };

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
        obj.insert("req_id".to_string(), serde_json::Value::String("REQ-001".to_string()));
        obj.insert("category".to_string(), serde_json::Value::String("security".to_string()));
        let result = interpolate_template("ears_{req_id}_{category}", &obj);
        assert_eq!(result, "ears_REQ-001_security");
    }

    #[test]
    fn test_format_value() {
        assert_eq!(format_value("STRING", Some(&serde_json::json!("hello"))), "'hello'");
        assert_eq!(format_value("INT64", Some(&serde_json::json!(42))), "42");
        assert_eq!(format_value("BOOL", Some(&serde_json::json!(true))), "true");
        assert_eq!(format_value("STRING", None), "''");
        assert_eq!(format_value("INT64", None), "0");
    }

    fn simple_schema() -> SchemaMeta {
        SchemaMeta {
            schema_id: "test_items".to_string(),
            name: "Test Items".to_string(),
            node_table: "TestItem".to_string(),
            columns: vec![
                ColumnDef { name: "title".to_string(), col_type: "STRING".to_string(), required: true },
                ColumnDef { name: "priority".to_string(), col_type: "INT64".to_string(), required: false },
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

        let mut qr = conn.query("MATCH (t:TestItem) RETURN t.id ORDER BY t.id").unwrap();
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
        std::fs::write(tmp.path(), r#"[{"item_id":"J1","title":"JSON item","priority":5}]"#).unwrap();

        let result = ingest_file(&conn, &schema, tmp.path()).unwrap();
        assert_eq!(result.nodes_created, 1);
    }

    #[test]
    fn test_ingest_file_yaml() {
        let (_dir, store) = kuzu_conn();
        let conn = store.connection().unwrap();
        let schema = simple_schema();

        let tmp = tempfile::NamedTempFile::with_suffix(".yaml").unwrap();
        std::fs::write(tmp.path(), "- item_id: Y1\n  title: YAML item\n  priority: 10\n").unwrap();

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
        ).unwrap();
        std::fs::write(
            data_dir.path().join("batch2.json"),
            r#"[{"item_id":"D2","title":"Dir item 2","priority":2}]"#,
        ).unwrap();
        std::fs::write(
            data_dir.path().join("ignore.txt"),
            "not a data file",
        ).unwrap();

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
        assert!(err.to_string().contains("title"), "error should mention missing field 'title': {err}");
    }

    #[test]
    fn test_edge_creation_between_nodes() {
        let (_dir, store) = kuzu_conn();
        let conn = store.connection().unwrap();

        let schema = SchemaMeta {
            schema_id: "linked".to_string(),
            name: "Linked".to_string(),
            node_table: "LinkedNode".to_string(),
            columns: vec![
                ColumnDef { name: "label".to_string(), col_type: "STRING".to_string(), required: false },
            ],
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
        obj.insert("req_id".to_string(), serde_json::Value::String("REQ-001".to_string()));
        let result = interpolate_template("{req_id}_{category}", &obj);
        assert_eq!(result, "REQ-001_{category}", "missing field should remain as literal placeholder");
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

        let data = vec![
            serde_json::json!({"id": "exists", "refs": ["does_not_exist"]}),
        ];

        let result = ingest_data(&conn, &schema, &data).unwrap();
        assert_eq!(result.nodes_created, 1);
        assert_eq!(result.edges_created, 0, "edge to nonexistent target should silently fail");
    }

    #[test]
    fn test_unsupported_file_format() {
        let (_dir, store) = kuzu_conn();
        let conn = store.connection().unwrap();
        let schema = simple_schema();

        let tmp = tempfile::NamedTempFile::with_suffix(".csv").unwrap();
        std::fs::write(tmp.path(), "a,b\n1,2").unwrap();

        let err = ingest_file(&conn, &schema, tmp.path()).unwrap_err();
        assert!(err.to_string().contains("Unsupported"), "should mention unsupported format: {err}");
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
        ).unwrap();

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
        ).unwrap();

        let schemas = discover_schemas(dir.path()).unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].1.schema.schema_id, "tg_schema");
    }

    // ── Cozo structured ingestion tests ──────────────────────────────

    fn cozo_db() -> (tempfile::TempDir, cozo::DbInstance) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = cozo::DbInstance::new("sqlite", dir.path().join("cozo.db").to_str().unwrap(), Default::default()).unwrap();
        (dir, db)
    }

    #[test]
    fn test_cozo_generate_ddl() {
        let schema = simple_schema();
        let ddl = schema.generate_cozo_ddl();
        assert_eq!(ddl.len(), 1);
        assert!(ddl[0].contains("testitem"), "table name should be lowercased");
        assert!(ddl[0].contains("title: String"), "should have String column");
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

        let r = db.run_script(
            "?[id] := *testitem{id}\n:order id",
            std::collections::BTreeMap::new(),
            cozo::ScriptMutability::Immutable,
        ).unwrap();
        assert_eq!(r.rows.len(), 3);
    }

    #[test]
    fn test_cozo_ingest_file_json() {
        let (_dir, db) = cozo_db();
        let schema = simple_schema();

        let tmp = tempfile::NamedTempFile::with_suffix(".json").unwrap();
        std::fs::write(tmp.path(), r#"[{"item_id":"J1","title":"JSON item","priority":5}]"#).unwrap();

        let result = ingest_file_cozo(&db, &schema, tmp.path()).unwrap();
        assert_eq!(result.nodes_created, 1);
    }

    #[test]
    fn test_cozo_ingest_file_yaml() {
        let (_dir, db) = cozo_db();
        let schema = simple_schema();

        let tmp = tempfile::NamedTempFile::with_suffix(".yaml").unwrap();
        std::fs::write(tmp.path(), "- item_id: Y1\n  title: YAML item\n  priority: 10\n").unwrap();

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
        ).unwrap();
        std::fs::write(
            data_dir.path().join("batch2.json"),
            r#"[{"item_id":"D2","title":"Dir item 2","priority":2}]"#,
        ).unwrap();

        let result = ingest_directory_cozo(&db, &schema, data_dir.path()).unwrap();
        assert_eq!(result.nodes_created, 2);
    }

    #[test]
    fn test_cozo_required_field_missing() {
        let (_dir, db) = cozo_db();
        let schema = simple_schema();

        let data = vec![serde_json::json!({"item_id": "X1", "priority": 1})];
        let err = ingest_data_cozo(&db, &schema, &data).unwrap_err();
        assert!(err.to_string().contains("title"), "error should mention missing field: {err}");
    }

    #[test]
    fn test_cozo_edge_creation() {
        let (_dir, db) = cozo_db();

        let schema = SchemaMeta {
            schema_id: "linked".to_string(),
            name: "Linked".to_string(),
            node_table: "LinkedNode".to_string(),
            columns: vec![
                ColumnDef { name: "label".to_string(), col_type: "STRING".to_string(), required: false },
            ],
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

        let data = vec![
            serde_json::json!({"id": "exists", "refs": ["does_not_exist"]}),
        ];

        let result = ingest_data_cozo(&db, &schema, &data).unwrap();
        assert_eq!(result.nodes_created, 1);
        assert_eq!(result.edges_created, 0);
    }

    #[test]
    fn test_cozo_format_value() {
        assert_eq!(format_cozo_value("STRING", Some(&serde_json::json!("hello"))), "\"hello\"");
        assert_eq!(format_cozo_value("INT64", Some(&serde_json::json!(42))), "42");
        assert_eq!(format_cozo_value("BOOL", Some(&serde_json::json!(true))), "true");
        assert_eq!(format_cozo_value("STRING", None), "\"\"");
        assert_eq!(format_cozo_value("INT64", None), "0");
    }
}
