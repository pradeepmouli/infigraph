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
                bail!(
                    "Invalid column name '{}' in schema '{}'",
                    col.name,
                    self.schema_id
                );
            }
            if !VALID_COL_TYPES.contains(&col.col_type.as_str()) {
                bail!(
                    "Invalid col_type '{}' for column '{}': must be one of {:?}",
                    col.col_type,
                    col.name,
                    VALID_COL_TYPES
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
                let p: Vec<String> = edge
                    .properties
                    .iter()
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
                schema
                    .schema
                    .validate()
                    .with_context(|| format!("schema validation failed: {}", path.display()))?;
                schemas.push((path, schema));
            }
        }
    }

    Ok(schemas)
}

#[derive(Debug)]
pub struct IngestResult {
    pub nodes_created: usize,
    pub edges_created: usize,
}

pub(crate) fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

pub(crate) fn format_value(col_type: &str, val: Option<&serde_json::Value>) -> String {
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
                    let items: Vec<String> = arr
                        .iter()
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

pub(crate) fn interpolate_template(
    tmpl: &str,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> String {
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
