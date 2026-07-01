mod bridge;
pub mod combined;
mod cross_service;
pub mod grpc;

pub use bridge::*;
pub use cross_service::*;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::graph::GraphQuery;
use crate::lang::LanguageRegistry;
use crate::Infigraph;

/// Global registry stored at ~/.infigraph/registry.json
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    pub repos: HashMap<String, RepoEntry>,
    pub groups: HashMap<String, Group>,
}

/// A registered repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    pub name: String,
    pub path: PathBuf,
    pub languages: Vec<String>,
    pub symbol_count: u64,
    pub module_count: u64,
}

/// A group of repositories (e.g., microservice architecture).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub repos: Vec<String>,
    pub contracts: Vec<Contract>,
}

/// A contract extracted from a service (HTTP route, gRPC endpoint, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contract {
    pub kind: ContractKind,
    pub service: String,
    pub method: String,
    pub path: String,
    pub symbol_id: String,
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ContractKind {
    HttpRoute,
    GrpcService,
    EventPublish,
    EventSubscribe,
}

impl Registry {
    /// Load registry from ~/.infigraph/registry.json
    pub fn load() -> Result<Self> {
        let path = registry_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(&path)?;
        let registry: Registry = serde_json::from_str(&data)?;
        Ok(registry)
    }

    /// Save registry to disk.
    pub fn save(&self) -> Result<()> {
        let path = registry_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, data)?;
        Ok(())
    }

    /// Register a repository after indexing.
    pub fn register_repo(&mut self, name: &str, path: &Path, prism: &Infigraph) -> Result<()> {
        let stats = prism.stats()?;
        let langs: Vec<String> = prism
            .registry()
            .languages()
            .map(|p| p.name.clone())
            .collect();

        let abs_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

        self.repos.insert(
            name.to_string(),
            RepoEntry {
                name: name.to_string(),
                path: abs_path,
                languages: langs,
                symbol_count: stats.symbols,
                module_count: stats.modules,
            },
        );
        self.save()
    }

    /// Create a new group.
    pub fn create_group(&mut self, name: &str) -> Result<()> {
        if self.groups.contains_key(name) {
            anyhow::bail!("group '{}' already exists", name);
        }
        self.groups.insert(
            name.to_string(),
            Group {
                name: name.to_string(),
                repos: Vec::new(),
                contracts: Vec::new(),
            },
        );
        self.save()
    }

    /// Add a repo to a group.
    pub fn group_add(&mut self, group_name: &str, repo_name: &str) -> Result<()> {
        let group = self
            .groups
            .get_mut(group_name)
            .context(format!("group '{}' not found", group_name))?;
        if !self.repos.contains_key(repo_name) {
            anyhow::bail!("repo '{}' not registered. Run index first.", repo_name);
        }
        if !group.repos.contains(&repo_name.to_string()) {
            group.repos.push(repo_name.to_string());
        }
        self.save()
    }

    /// Remove a repo from a group.
    pub fn group_remove(&mut self, group_name: &str, repo_name: &str) -> Result<()> {
        let group = self
            .groups
            .get_mut(group_name)
            .context(format!("group '{}' not found", group_name))?;
        group.repos.retain(|r| r != repo_name);
        self.save()
    }

    /// Search across all repos in a group.
    pub fn group_query(
        &self,
        group_name: &str,
        cypher: &str,
        build_registry: impl Fn() -> Result<LanguageRegistry>,
    ) -> Result<Vec<(String, Vec<Vec<String>>)>> {
        let group = self
            .groups
            .get(group_name)
            .context(format!("group '{}' not found", group_name))?;

        let mut results = Vec::new();
        for repo_name in &group.repos {
            let entry = self
                .repos
                .get(repo_name)
                .context(format!("repo '{}' not in registry", repo_name))?;

            let registry = build_registry()?;
            let mut prism = Infigraph::open(&entry.path, registry)?;
            prism.init()?;

            let store = prism.store().context("graph not initialized")?;
            let conn = store.connection()?;
            let gq = GraphQuery::new(&conn);

            match gq.raw_query(cypher) {
                Ok(rows) => {
                    if !rows.is_empty() {
                        results.push((repo_name.clone(), rows));
                    }
                }
                Err(e) => {
                    eprintln!("warning: query failed for repo '{}': {}", repo_name, e);
                }
            }
        }
        Ok(results)
    }
}

/// Extract HTTP route contracts from a project's graph.
///
/// Sources (in priority order):
/// 1. Route symbols (kind='Route') — from call-expression routing (Express, Gin, etc.)
/// 2. Decorated functions — docstring contains route decorator (@app.route, #[get], etc.)
/// 3. Heuristic detect_routes fallback
pub fn extract_contracts(prism: &Infigraph, service_name: &str) -> Result<Vec<Contract>> {
    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = GraphQuery::new(&conn);

    let mut contracts = Vec::new();
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 1. Route symbols (call-expression routes: Express, Gin, Django, etc.)
    let route_rows = gq.raw_query(
        "MATCH (s:Symbol) WHERE s.kind = 'Route' RETURN s.id, s.name, s.kind, s.file, s.docstring",
    )?;
    for row in &route_rows {
        let (method, path) = parse_route_name(&row[1]);
        let key = format!("{} {}", method, path);
        if seen_paths.insert(key) {
            contracts.push(Contract {
                kind: ContractKind::HttpRoute,
                service: service_name.to_string(),
                method,
                path,
                symbol_id: row[0].clone(),
                file: row[3].clone(),
            });
        }
    }

    // 2. Decorated functions with route info in docstring
    let decorated_rows = gq.raw_query(
        "MATCH (s:Symbol) WHERE s.kind IN ['Function', 'Method'] AND s.docstring IS NOT NULL AND (s.docstring CONTAINS '@app.route' OR s.docstring CONTAINS '@app.get' OR s.docstring CONTAINS '@app.post' OR s.docstring CONTAINS '#[get' OR s.docstring CONTAINS '#[post' OR s.docstring CONTAINS '@GetMapping' OR s.docstring CONTAINS '@PostMapping' OR s.docstring CONTAINS '@RequestMapping' OR s.docstring CONTAINS 'MapGet' OR s.docstring CONTAINS 'MapPost') RETURN s.id, s.name, s.kind, s.file, s.docstring",
    )?;
    for row in &decorated_rows {
        let doc = row.get(4).map(|s| s.as_str()).unwrap_or("");
        let (method, path) = parse_route_from_docstring(doc);
        if !path.is_empty() {
            let key = format!("{} {}", method, path);
            if seen_paths.insert(key) {
                contracts.push(Contract {
                    kind: ContractKind::HttpRoute,
                    service: service_name.to_string(),
                    method,
                    path,
                    symbol_id: row[0].clone(),
                    file: row[3].clone(),
                });
            }
        }
    }

    Ok(contracts)
}

/// Parse "GET /api/users" or "MAPGET /api/users" into (method, path).
fn parse_route_name(name: &str) -> (String, String) {
    let parts: Vec<&str> = name.splitn(2, ' ').collect();
    if parts.len() == 2 {
        let method = parts[0].trim().to_uppercase();
        // Normalize MapGet -> GET, MapPost -> POST, etc.
        let method = if method.starts_with("MAP") {
            method.trim_start_matches("MAP").to_string()
        } else {
            method
        };
        (method, parts[1].trim().to_string())
    } else {
        ("UNKNOWN".to_string(), name.to_string())
    }
}

/// Extract method and path from decorator docstrings.
fn parse_route_from_docstring(doc: &str) -> (String, String) {
    // @app.route("/api/users", methods=["GET"])
    // @app.get("/api/users")
    // #[get("/api/payments")]
    // @GetMapping("/api/users")
    let doc_lower = doc.to_lowercase();

    // Extract path from quotes
    let path = doc
        .split('"')
        .chain(doc.split('\''))
        .find(|s| s.starts_with('/'))
        .unwrap_or("")
        .to_string();

    // Extract method
    let method = if doc_lower.contains("methods") {
        // methods=["GET", "POST"] — take first
        if doc_lower.contains("\"get\"") || doc_lower.contains("'get'") {
            "GET"
        } else if doc_lower.contains("\"post\"") || doc_lower.contains("'post'") {
            "POST"
        } else if doc_lower.contains("\"put\"") || doc_lower.contains("'put'") {
            "PUT"
        } else if doc_lower.contains("\"delete\"") || doc_lower.contains("'delete'") {
            "DELETE"
        } else if doc_lower.contains("\"patch\"") || doc_lower.contains("'patch'") {
            "PATCH"
        } else {
            "UNKNOWN"
        }
    } else if doc_lower.contains("@app.get")
        || doc_lower.contains("#[get")
        || doc_lower.contains("getmapping")
        || doc_lower.contains("mapget")
    {
        "GET"
    } else if doc_lower.contains("@app.post")
        || doc_lower.contains("#[post")
        || doc_lower.contains("postmapping")
        || doc_lower.contains("mappost")
    {
        "POST"
    } else if doc_lower.contains("@app.put")
        || doc_lower.contains("#[put")
        || doc_lower.contains("putmapping")
        || doc_lower.contains("mapput")
    {
        "PUT"
    } else if doc_lower.contains("@app.delete")
        || doc_lower.contains("#[delete")
        || doc_lower.contains("deletemapping")
        || doc_lower.contains("mapdelete")
    {
        "DELETE"
    } else if doc_lower.contains("@app.patch")
        || doc_lower.contains("#[patch")
        || doc_lower.contains("patchmapping")
        || doc_lower.contains("mappatch")
    {
        "PATCH"
    } else {
        "UNKNOWN"
    };

    (method.to_string(), path)
}

/// Sync contracts for all repos in a group.
pub fn sync_group_contracts(
    registry: &mut Registry,
    group_name: &str,
    build_registry: impl Fn() -> Result<LanguageRegistry>,
) -> Result<usize> {
    let group = registry
        .groups
        .get(group_name)
        .context(format!("group '{}' not found", group_name))?
        .clone();

    let mut all_contracts = Vec::new();

    for repo_name in &group.repos {
        let entry = registry
            .repos
            .get(repo_name)
            .context(format!("repo '{}' not in registry", repo_name))?
            .clone();

        let lang_registry = build_registry()?;
        let mut prism = Infigraph::open(&entry.path, lang_registry)?;
        prism.init()?;

        let contracts = extract_contracts(&prism, repo_name)?;
        all_contracts.extend(contracts);
    }

    let count = all_contracts.len();
    let group = registry
        .groups
        .get_mut(group_name)
        .context("group not found")?;
    group.contracts = all_contracts;
    registry.save()?;

    Ok(count)
}

/// Index all repos in a group. Returns Vec of (repo_name, indexed_files, total_files).
pub fn index_group(
    registry: &mut Registry,
    group_name: &str,
    full: bool,
    build_registry: impl Fn() -> Result<LanguageRegistry>,
) -> Result<Vec<(String, usize, usize)>> {
    let group = registry
        .groups
        .get(group_name)
        .context(format!("group '{}' not found", group_name))?
        .clone();

    let mut results = Vec::new();

    for repo_name in &group.repos {
        let entry = registry
            .repos
            .get(repo_name)
            .context(format!("repo '{}' not in registry", repo_name))?
            .clone();

        if full {
            let tg_dir = entry.path.join(".infigraph");
            if tg_dir.exists() {
                std::fs::remove_dir_all(&tg_dir)?;
            }
        }

        let lang_registry = build_registry()?;
        let mut prism = Infigraph::open(&entry.path, lang_registry)?;
        prism.init()?;
        let result = prism.index()?;
        results.push((repo_name.clone(), result.indexed_files, result.total_files));

        registry.register_repo(repo_name, &entry.path, &prism)?;
    }

    Ok(results)
}

fn registry_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs_next::home_dir)
        .context("cannot determine home directory")?;
    Ok(home.join(".infigraph").join("registry.json"))
}
