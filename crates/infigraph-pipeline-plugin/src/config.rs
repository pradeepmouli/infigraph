use anyhow::{bail, Result};
use regex::Regex;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct PipelinePluginConfig {
    pub plugin: PluginMeta,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginMeta {
    pub name: String,
    pub plugin_id: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub schema: Vec<ColumnDef>,
    #[serde(default)]
    pub dependency_fields: Option<DependencyFields>,
    #[serde(default)]
    pub searchable_fields: Vec<String>,
    #[serde(default)]
    pub detect_patterns: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub col_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DependencyFields {
    pub inputs: String,
    pub outputs: String,
}

const VALID_COL_TYPES: &[&str] = &["STRING", "INT64", "BOOL", "DOUBLE", "STRING[]"];

impl PluginMeta {
    /// Validate the plugin metadata.
    pub fn validate(&self) -> Result<()> {
        let id_re = Regex::new(r"^[a-z][a-z0-9_]{0,31}$").unwrap();
        if !id_re.is_match(&self.plugin_id) {
            bail!(
                "Invalid plugin_id '{}': must match ^[a-z][a-z0-9_]{{0,31}}$",
                self.plugin_id
            );
        }

        let col_re = Regex::new(r"^[a-z][a-z0-9_]{0,63}$").unwrap();
        for col in &self.schema {
            if !col_re.is_match(&col.name) {
                bail!(
                    "Invalid column name '{}': must match ^[a-z][a-z0-9_]{{0,63}}$",
                    col.name
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

        if self.command.is_empty() {
            bail!("Plugin command must not be empty");
        }

        Ok(())
    }
}

impl ColumnDef {
    /// Map col_type string to Kuzu DDL type.
    pub fn to_kuzu_type(&self) -> &str {
        // All supported types map directly to Kuzu DDL types.
        match self.col_type.as_str() {
            "STRING" => "STRING",
            "INT64" => "INT64",
            "BOOL" => "BOOL",
            "DOUBLE" => "DOUBLE",
            "STRING[]" => "STRING[]",
            other => other,
        }
    }
}

/// Generate CREATE NODE TABLE DDL for a pipeline plugin.
pub fn generate_ddl(plugin_id: &str, columns: &[ColumnDef]) -> String {
    let table_name = format!("Pipeline_{}", plugin_id);
    let mut col_defs = vec!["id STRING".to_string()];
    for col in columns {
        col_defs.push(format!("{} {}", col.name, col.to_kuzu_type()));
    }
    format!(
        "CREATE NODE TABLE IF NOT EXISTS {}({}, PRIMARY KEY(id))",
        table_name,
        col_defs.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TOML: &str = r#"
[plugin]
name = "Intuit Pipeline"
plugin_id = "intuit"
command = ["python3", "extract.py"]
searchable_fields = ["stage_name"]
detect_patterns = ["stage\\s*\\("]

[[plugin.schema]]
name = "stage_name"
col_type = "STRING"

[[plugin.schema]]
name = "order"
col_type = "INT64"

[plugin.dependency_fields]
inputs = "depends_on"
outputs = "produces"
"#;

    #[test]
    fn test_valid_config() {
        let config: PipelinePluginConfig = toml::from_str(SAMPLE_TOML).unwrap();
        assert_eq!(config.plugin.name, "Intuit Pipeline");
        assert_eq!(config.plugin.plugin_id, "intuit");
        assert_eq!(config.plugin.command, vec!["python3", "extract.py"]);
        assert_eq!(config.plugin.schema.len(), 2);
        assert_eq!(config.plugin.schema[0].name, "stage_name");
        assert_eq!(config.plugin.schema[0].col_type, "STRING");
        assert_eq!(config.plugin.schema[1].col_type, "INT64");
        assert!(config.plugin.dependency_fields.is_some());
        let deps = config.plugin.dependency_fields.as_ref().unwrap();
        assert_eq!(deps.inputs, "depends_on");
        assert_eq!(deps.outputs, "produces");
        assert_eq!(config.plugin.searchable_fields, vec!["stage_name"]);
        config.plugin.validate().unwrap();
    }

    #[test]
    fn test_invalid_plugin_id() {
        // Uppercase
        let toml_str = r#"
[plugin]
name = "Bad"
plugin_id = "BadId"
command = ["echo"]
"#;
        let config: PipelinePluginConfig = toml::from_str(toml_str).unwrap();
        assert!(config.plugin.validate().is_err());

        // Spaces
        let toml_str = r#"
[plugin]
name = "Bad"
plugin_id = "bad id"
command = ["echo"]
"#;
        let config: PipelinePluginConfig = toml::from_str(toml_str).unwrap();
        assert!(config.plugin.validate().is_err());

        // Too long (33 chars)
        let toml_str = r#"
[plugin]
name = "Bad"
plugin_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
command = ["echo"]
"#;
        let config: PipelinePluginConfig = toml::from_str(toml_str).unwrap();
        assert!(config.plugin.validate().is_err());
    }

    #[test]
    fn test_invalid_col_type() {
        let toml_str = r#"
[plugin]
name = "Bad"
plugin_id = "test"
command = ["echo"]

[[plugin.schema]]
name = "col1"
col_type = "FLOAT"
"#;
        let config: PipelinePluginConfig = toml::from_str(toml_str).unwrap();
        assert!(config.plugin.validate().is_err());

        let toml_str = r#"
[plugin]
name = "Bad"
plugin_id = "test"
command = ["echo"]

[[plugin.schema]]
name = "col1"
col_type = "VARCHAR"
"#;
        let config: PipelinePluginConfig = toml::from_str(toml_str).unwrap();
        assert!(config.plugin.validate().is_err());
    }

    #[test]
    fn test_ddl_generation() {
        let columns = vec![
            ColumnDef {
                name: "stage_name".to_string(),
                col_type: "STRING".to_string(),
            },
            ColumnDef {
                name: "order".to_string(),
                col_type: "INT64".to_string(),
            },
        ];
        let ddl = generate_ddl("intuit", &columns);
        assert_eq!(
            ddl,
            "CREATE NODE TABLE IF NOT EXISTS Pipeline_intuit(id STRING, stage_name STRING, order INT64, PRIMARY KEY(id))"
        );
    }

    #[test]
    fn test_to_kuzu_type_all_variants() {
        let types = vec![
            ("STRING", "STRING"),
            ("INT64", "INT64"),
            ("BOOL", "BOOL"),
            ("DOUBLE", "DOUBLE"),
            ("STRING[]", "STRING[]"),
        ];
        for (input, expected) in types {
            let col = ColumnDef {
                name: "x".to_string(),
                col_type: input.to_string(),
            };
            assert_eq!(col.to_kuzu_type(), expected, "mismatch for {}", input);
        }
    }

    #[test]
    fn test_validate_boundary_plugin_id() {
        // Exactly 32 chars: 1 leading + 31 trailing = 32 total
        let id = "a".repeat(32);
        let toml_str = format!(
            r#"
[plugin]
name = "Boundary"
plugin_id = "{}"
command = ["echo"]
"#,
            id
        );
        let config: PipelinePluginConfig = toml::from_str(&toml_str).unwrap();
        config.plugin.validate().unwrap();
    }

    #[test]
    fn test_generate_ddl_no_columns() {
        let ddl = generate_ddl("empty", &[]);
        assert_eq!(
            ddl,
            "CREATE NODE TABLE IF NOT EXISTS Pipeline_empty(id STRING, PRIMARY KEY(id))"
        );
    }

    #[test]
    fn test_config_without_dependency_fields() {
        let toml_str = r#"
[plugin]
name = "NoDeps"
plugin_id = "nodeps"
command = ["echo"]
"#;
        let config: PipelinePluginConfig = toml::from_str(toml_str).unwrap();
        assert!(config.plugin.dependency_fields.is_none());
        config.plugin.validate().unwrap();
    }

    #[test]
    fn test_empty_command() {
        let toml_str = r#"
[plugin]
name = "Bad"
plugin_id = "test"
command = []
"#;
        let config: PipelinePluginConfig = toml::from_str(toml_str).unwrap();
        assert!(config.plugin.validate().is_err());
    }
}
