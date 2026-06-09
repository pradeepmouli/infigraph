pub mod combined;
pub mod grpc;

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

        self.repos.insert(
            name.to_string(),
            RepoEntry {
                name: name.to_string(),
                path: path.to_path_buf(),
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

/// A cross-service dependency: service A calls service B at a specific route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossServiceDep {
    pub caller_service: String,
    pub caller_file: String,
    pub caller_symbol: String,
    pub target_service: String,
    pub target_method: String,
    pub target_path: String,
    pub url_found: String,
}

/// Detect cross-service HTTP dependencies within a group.
/// Scans source files for URL strings (fetch, http.get, requests.get, etc.)
/// and matches them to known contracts/routes in other services.
pub fn detect_cross_service_deps(
    registry: &Registry,
    group_name: &str,
    build_registry: impl Fn() -> Result<LanguageRegistry>,
) -> Result<Vec<CrossServiceDep>> {
    let group = registry
        .groups
        .get(group_name)
        .context(format!("group '{}' not found", group_name))?;

    // Collect all contracts as lookup table: path → (service, method)
    let mut route_lookup: HashMap<String, (String, String)> = HashMap::new();
    for contract in &group.contracts {
        if contract.kind == ContractKind::HttpRoute {
            // Normalize path for matching (strip params)
            let normalized = normalize_route_path(&contract.path);
            route_lookup.insert(
                normalized,
                (contract.service.clone(), contract.method.clone()),
            );
        }
    }

    let mut deps = Vec::new();

    for repo_name in &group.repos {
        let entry = match registry.repos.get(repo_name) {
            Some(e) => e.clone(),
            None => continue,
        };

        let lang_registry = build_registry()?;
        let mut prism = Infigraph::open(&entry.path, lang_registry)?;
        prism.init()?;

        let store = match prism.store() {
            Some(s) => s,
            None => continue,
        };
        let conn = match store.connection() {
            Ok(c) => c,
            Err(_) => continue,
        };
        let gq = GraphQuery::new(&conn);

        // Find symbols with URL-like strings in docstrings or search source files
        let rows = gq.raw_query(
            "MATCH (s:Symbol) WHERE s.docstring IS NOT NULL AND (s.docstring CONTAINS '/api/' OR s.docstring CONTAINS 'http://' OR s.docstring CONTAINS 'https://') RETURN s.id, s.name, s.file, s.docstring",
        ).unwrap_or_default();

        for row in &rows {
            let doc = row.get(3).map(|s| s.as_str()).unwrap_or("");
            let urls = extract_api_paths(doc);
            for url in urls {
                let normalized = normalize_route_path(&url);
                if let Some((target_svc, target_method)) = route_lookup.get(&normalized) {
                    if target_svc != repo_name {
                        deps.push(CrossServiceDep {
                            caller_service: repo_name.clone(),
                            caller_file: row[2].clone(),
                            caller_symbol: row[0].clone(),
                            target_service: target_svc.clone(),
                            target_method: target_method.clone(),
                            target_path: url.clone(),
                            url_found: url,
                        });
                    }
                }
            }
        }

        // Also grep source files for URL patterns
        let source_urls = scan_source_for_urls(&entry.path);
        for (file, symbol_hint, url) in source_urls {
            let normalized = normalize_route_path(&url);
            if let Some((target_svc, target_method)) = route_lookup.get(&normalized) {
                if target_svc != repo_name {
                    // Try to resolve line hint to enclosing symbol ID
                    let caller_id = if let Some(stripped) = symbol_hint.strip_prefix("line:") {
                        let line_num: i32 = stripped.parse().unwrap_or(0);
                        let escaped_file = file.replace('\'', "\\'");
                        let q = format!(
                            "MATCH (s:Symbol) WHERE s.file = '{}' AND s.start_line <= {} AND s.end_line >= {} RETURN s.id ORDER BY (s.end_line - s.start_line) ASC LIMIT 1",
                            escaped_file, line_num, line_num
                        );
                        gq.raw_query(&q)
                            .ok()
                            .and_then(|rows| rows.into_iter().next())
                            .and_then(|row| row.into_iter().next())
                            .unwrap_or_else(|| format!("{}:{}", file, symbol_hint))
                    } else {
                        symbol_hint.clone()
                    };
                    deps.push(CrossServiceDep {
                        caller_service: repo_name.clone(),
                        caller_file: file,
                        caller_symbol: caller_id,
                        target_service: target_svc.clone(),
                        target_method: target_method.clone(),
                        target_path: url.clone(),
                        url_found: url,
                    });
                }
            }
        }
    }

    Ok(deps)
}

/// Normalize a route path for matching: strip trailing slash, remove param placeholders.
fn normalize_route_path(path: &str) -> String {
    let path = path.trim_end_matches('/');
    // Extract just the path portion from full URLs
    let path = if let Some(idx) = path.find("/api/") {
        &path[idx..]
    } else if path.starts_with("http") {
        path.split("//")
            .nth(1)
            .and_then(|s| s.find('/').map(|i| &s[i..]))
            .unwrap_or(path)
    } else {
        path
    };
    // Normalize path params: /users/:id → /users/{id} → /users/*
    let segments: Vec<&str> = path.split('/').collect();
    segments
        .iter()
        .map(|s| {
            if s.starts_with(':') || s.starts_with('{') || s.starts_with('<') {
                "*"
            } else {
                s
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Extract API paths from a string (URL literals in code).
fn extract_api_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for part in text
        .split('"')
        .chain(text.split('\'').chain(text.split('`')))
    {
        let trimmed = part.trim();
        if (trimmed.starts_with("/api/") || trimmed.starts_with("http"))
            && trimmed.contains("/api/")
        {
            paths.push(trimmed.to_string());
        }
    }
    paths
}

/// Scan source files for URL strings containing /api/ patterns.
fn scan_source_for_urls(root: &Path) -> Vec<(String, String, String)> {
    const SKIP_DIRS: &[&str] = &[
        ".infigraph",
        ".git",
        "node_modules",
        "target",
        "build",
        "dist",
        "__pycache__",
        ".venv",
    ];
    let mut results = Vec::new();
    walk_for_urls(root, root, SKIP_DIRS, &mut results);
    results
}

fn walk_for_urls(
    base: &Path,
    dir: &Path,
    skip: &[&str],
    results: &mut Vec<(String, String, String)>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if path.is_dir() {
            if !skip.contains(&name_str.as_ref()) && !name_str.starts_with('.') {
                walk_for_urls(base, &path, skip, results);
            }
        } else if path.is_file() {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (line_num, line) in content.lines().enumerate() {
                for delim in ['"', '\'', '`'] {
                    for part in line.split(delim) {
                        let trimmed = part.trim();
                        if trimmed.contains("/api/")
                            && trimmed.len() < 200
                            && !trimmed.contains(' ')
                        {
                            let path_part = if trimmed.starts_with("http") {
                                trimmed
                                    .split("//")
                                    .nth(1)
                                    .and_then(|s| s.find('/').map(|i| &s[i..]))
                                    .unwrap_or(trimmed)
                            } else {
                                trimmed
                            };
                            if path_part.starts_with("/api/") {
                                results.push((
                                    rel.clone(),
                                    format!("line:{}", line_num + 1),
                                    path_part.to_string(),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Link cross-service HTTP dependencies as CALLS_SERVICE edges in each caller's graph.
/// Returns number of edges created.
pub fn link_cross_service_calls(
    registry: &Registry,
    group_name: &str,
    build_registry: impl Fn() -> Result<LanguageRegistry>,
) -> Result<usize> {
    let deps = detect_cross_service_deps(registry, group_name, &build_registry)?;
    if deps.is_empty() {
        return Ok(0);
    }

    // Group deps by caller service
    let mut by_caller: HashMap<String, Vec<&CrossServiceDep>> = HashMap::new();
    for dep in &deps {
        by_caller
            .entry(dep.caller_service.clone())
            .or_default()
            .push(dep);
    }

    let mut total = 0;

    for (caller_svc, svc_deps) in &by_caller {
        let entry = match registry.repos.get(caller_svc) {
            Some(e) => e,
            None => continue,
        };

        let lang_registry = build_registry()?;
        let mut prism = Infigraph::open(&entry.path, lang_registry)?;
        prism.init()?;

        let store = match prism.store() {
            Some(s) => s,
            None => continue,
        };
        let conn = match store.connection() {
            Ok(c) => c,
            Err(_) => continue,
        };
        let gq = GraphQuery::new(&conn);

        for dep in svc_deps {
            let target_id = format!(
                "xsvc::{}::{}::{}",
                dep.target_service,
                dep.target_method,
                dep.target_path.replace('\'', "\\'")
            );
            let target_name = format!(
                "{} {} {}",
                dep.target_service, dep.target_method, dep.target_path
            )
            .replace('\'', "\\'");
            let caller_sym = dep.caller_symbol.replace('\'', "\\'");
            let target_svc = dep.target_service.replace('\'', "\\'");
            let target_method = dep.target_method.replace('\'', "\\'");
            let target_path = dep.target_path.replace('\'', "\\'");

            // Create ExternalService node — only use columns from Symbol schema.
            // Use MERGE for idempotency (safe to run group_link multiple times).
            let docstring = format!(
                "External service: {} {} {}",
                target_svc, target_method, target_path
            );
            let create_target = format!(
                "MERGE (t:Symbol {{id: '{}'}}) \
                 ON CREATE SET t.name = '{}', t.kind = 'ExternalService', \
                 t.file = '(external)', t.start_line = 0, t.end_line = 0, \
                 t.signature_hash = '', t.language = 'external', t.visibility = 'public', \
                 t.parent = '', t.docstring = '{}', t.complexity = 0",
                target_id, target_name, docstring,
            );
            let _ = gq.raw_query(&create_target);

            // Check if edge already exists before creating (idempotent)
            let check_edge = format!(
                "MATCH (caller:Symbol {{id: '{}'}})-[:CALLS_SERVICE]->(target:Symbol {{id: '{}'}}) RETURN caller.id",
                caller_sym, target_id,
            );
            let existing = gq.raw_query(&check_edge).unwrap_or_default();
            if !existing.is_empty() {
                continue;
            }

            let create_edge = format!(
                "MATCH (caller:Symbol {{id: '{}'}}), (target:Symbol {{id: '{}'}}) \
                 CREATE (caller)-[:CALLS_SERVICE {{method: '{}', path: '{}', target_service: '{}'}}]->(target)",
                caller_sym, target_id, target_method, target_path, target_svc,
            );
            if gq.raw_query(&create_edge).is_ok() {
                total += 1;
            }
        }
    }

    Ok(total)
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
    let home = dirs_next::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".infigraph").join("registry.json"))
}
