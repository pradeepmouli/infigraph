use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;

use infigraph_core::lang::CustomExtractor;
use infigraph_core::model::{Relation, RelationKind, Span, Symbol, SymbolKind};

use crate::driver::GrammarDriver;

#[derive(Debug, Clone, Deserialize)]
pub struct GrammarPluginConfig {
    pub language: LanguageMeta,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LanguageMeta {
    pub name: String,
    pub extensions: Vec<String>,
    pub entry_rule: String,
    pub lexer: String,
    pub parser: String,
    pub preprocessor: Option<String>,
    pub extractor: String,
    #[serde(default)]
    pub emit_referenced_form_imports: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectPreprocessorConfig {
    #[serde(default)]
    pub defines: Vec<String>,
    #[serde(default)]
    pub include_paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
    pub preprocessor: Option<ProjectPreprocessorConfig>,
}

pub struct GrammarPlugin {
    pub config: GrammarPluginConfig,
    pub plugin_dir: PathBuf,
    driver: Arc<GrammarDriver>,
    project_preprocessor: Option<ProjectPreprocessorConfig>,
}

impl GrammarPlugin {
    pub fn new(
        config: GrammarPluginConfig,
        plugin_dir: PathBuf,
        driver: Arc<GrammarDriver>,
        project_preprocessor: Option<ProjectPreprocessorConfig>,
    ) -> Self {
        Self {
            config,
            plugin_dir,
            driver,
            project_preprocessor,
        }
    }

    pub fn load(&self) -> Result<()> {
        let lexer_path = self.plugin_dir.join(&self.config.language.lexer);
        let parser_path = self.plugin_dir.join(&self.config.language.parser);
        self.driver.load_grammar(
            &self.config.language.name,
            lexer_path.to_str().context("Invalid lexer path")?,
            parser_path.to_str().context("Invalid parser path")?,
            &self.config.language.entry_rule,
            self.config.language.preprocessor.as_deref(),
            self.config.language.emit_referenced_form_imports,
        )?;

        self.driver
            .set_extractor(&self.config.language.name, &self.config.language.extractor)?;

        Ok(())
    }

    pub fn extract(&self, path: &str, source: &[u8]) -> Result<(Vec<Symbol>, Vec<Relation>)> {
        let source_str = std::str::from_utf8(source)?;

        let (defines, include_paths) = if self.config.language.preprocessor.is_some() {
            if let Some(ref pp_config) = self.project_preprocessor {
                (
                    Some(pp_config.defines.join(",")),
                    Some(pp_config.include_paths.join(",")),
                )
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        let resp = self.driver.extract(
            &self.config.language.name,
            path,
            source_str,
            defines.as_deref(),
            include_paths.as_deref(),
        )?;
        let language = &self.config.language.name;

        let symbols = resp
            .get("symbols")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| {
                        Some(Symbol {
                            id: s.get("id")?.as_str()?.to_string(),
                            name: s.get("name")?.as_str()?.to_string(),
                            kind: parse_symbol_kind(s.get("kind")?.as_str()?),
                            span: Span {
                                file: s.get("file")?.as_str()?.to_string(),
                                start_line: s.get("start_line")?.as_u64()? as u32,
                                start_col: s.get("start_col")?.as_u64()? as u32,
                                end_line: s.get("end_line")?.as_u64()? as u32,
                                end_col: s.get("end_col")?.as_u64()? as u32,
                            },
                            signature_hash: s
                                .get("signature_hash")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0000000000000000")
                                .to_string(),
                            parent: s
                                .get("parent")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            language: language.clone(),
                            visibility: None,
                            docstring: None,
                            complexity: 0,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let relations = resp
            .get("relations")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        Some(Relation {
                            source_id: r.get("source_id")?.as_str()?.to_string(),
                            target_id: r.get("target_id")?.as_str()?.to_string(),
                            kind: parse_relation_kind(r.get("kind")?.as_str()?),
                            span: Some(Span {
                                file: r.get("file")?.as_str()?.to_string(),
                                start_line: r.get("start_line")?.as_u64()? as u32,
                                start_col: r.get("start_col")?.as_u64()? as u32,
                                end_line: r.get("end_line")?.as_u64()? as u32,
                                end_col: r.get("end_col")?.as_u64()? as u32,
                            }),
                            receiver: r.get("receiver").and_then(|v| v.as_str()).map(|s| s.to_string()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok((symbols, relations))
    }
}

impl CustomExtractor for GrammarPlugin {
    fn extract(
        &self,
        path: &str,
        source: &[u8],
        _language: &str,
    ) -> Result<(Vec<Symbol>, Vec<Relation>)> {
        self.extract(path, source)
    }
}

pub fn discover_plugins(plugins_dir: &Path) -> Result<Vec<(GrammarPluginConfig, PathBuf)>> {
    let mut plugins = Vec::new();

    if !plugins_dir.exists() {
        return Ok(plugins);
    }

    for entry in std::fs::read_dir(plugins_dir)? {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let config_path = dir.join("plugin.toml");
        if !config_path.exists() {
            continue;
        }
        let config_str = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        let config: GrammarPluginConfig = toml::from_str(&config_str)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        let lexer_path = dir.join(&config.language.lexer);
        let parser_path = dir.join(&config.language.parser);
        if !lexer_path.exists() {
            eprintln!(
                "[infigraph] Plugin '{}': lexer grammar not found: {}",
                config.language.name,
                lexer_path.display()
            );
            continue;
        }
        if !parser_path.exists() {
            eprintln!(
                "[infigraph] Plugin '{}': parser grammar not found: {}",
                config.language.name,
                parser_path.display()
            );
            continue;
        }

        plugins.push((config, dir));
    }

    Ok(plugins)
}

fn parse_symbol_kind(s: &str) -> SymbolKind {
    match s {
        "Function" => SymbolKind::Function,
        "Method" => SymbolKind::Method,
        "Class" => SymbolKind::Class,
        "Struct" => SymbolKind::Struct,
        "Interface" => SymbolKind::Interface,
        "Trait" => SymbolKind::Trait,
        "Enum" => SymbolKind::Enum,
        "Module" => SymbolKind::Module,
        "Variable" => SymbolKind::Variable,
        "Constant" => SymbolKind::Constant,
        "Test" => SymbolKind::Test,
        "Section" => SymbolKind::Section,
        "Route" => SymbolKind::Route,
        "Field" => SymbolKind::Field,
        _ => SymbolKind::Function,
    }
}

fn parse_relation_kind(s: &str) -> RelationKind {
    match s {
        "Calls" => RelationKind::Calls,
        "Imports" => RelationKind::Imports,
        "Inherits" => RelationKind::Inherits,
        "Implements" => RelationKind::Implements,
        "Contains" => RelationKind::Contains,
        "Reads" => RelationKind::Reads,
        "Writes" => RelationKind::Writes,
        _ => RelationKind::Calls,
    }
}
