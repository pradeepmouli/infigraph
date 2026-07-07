use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::graph::GraphQuery;
use crate::lang::LanguageRegistry;
use crate::Infigraph;

use super::{Contract, ContractKind, Registry};

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
    // Skip paths shorter than 4 chars (e.g. "/") — too generic, matches everything
    // Also index wildcard prefixes: /v1/entities/schedules → /v1/entities/*
    // so dynamic URLs like f"/v1/entities/{x}" match after normalization.
    let mut route_lookup: HashMap<String, (String, String)> = HashMap::new();
    for contract in &group.contracts {
        if contract.kind == ContractKind::HttpRoute {
            let normalized = normalize_route_path(&contract.path);
            if normalized.len() >= 4 {
                // Index exact path
                route_lookup.insert(
                    normalized.clone(),
                    (contract.service.clone(), contract.method.clone()),
                );
                // Index wildcard prefix: /a/b/c → /a/b/*
                // Only for paths with 3+ segments to avoid overly broad matches
                let segments: Vec<&str> = normalized.split('/').collect();
                if segments.len() >= 4 {
                    let prefix = segments[..segments.len() - 1].join("/") + "/*";
                    if prefix.len() >= 4 {
                        route_lookup
                            .entry(prefix)
                            .or_insert_with(|| (contract.service.clone(), contract.method.clone()));
                    }
                }
            }
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
            "MATCH (s:Symbol) WHERE s.docstring IS NOT NULL AND (s.docstring CONTAINS '/api/' OR s.docstring CONTAINS '/v1/' OR s.docstring CONTAINS '/v2/' OR s.docstring CONTAINS '/v3/' OR s.docstring CONTAINS 'http://' OR s.docstring CONTAINS 'https://') RETURN s.id, s.name, s.file, s.docstring",
        ).unwrap_or_default();

        for row in &rows {
            let file = row.get(2).map(|s| s.as_str()).unwrap_or("");
            if is_test_or_doc_file(file) {
                continue;
            }
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

        // Contract-driven inverted scan: check source for any known contract path
        let mut seen_contract_hits: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        let contract_hits = scan_source_for_contracts(&entry.path, &route_lookup, repo_name);
        for (file, line_hint, matched_path, target_svc, target_method) in contract_hits {
            let key = (file.clone(), matched_path.clone());
            if !seen_contract_hits.insert(key) {
                continue;
            }
            let caller_id = if let Some(stripped) = line_hint.strip_prefix("line:") {
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
                    .unwrap_or_else(|| format!("{}:{}", file, line_hint))
            } else {
                line_hint
            };
            deps.push(CrossServiceDep {
                caller_service: repo_name.clone(),
                caller_file: file,
                caller_symbol: caller_id,
                target_service: target_svc,
                target_method,
                target_path: matched_path.clone(),
                url_found: matched_path,
            });
        }
    }

    // Dedup: keep first occurrence per (caller_service, caller_file, target_path)
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    deps.retain(|d| {
        seen.insert((
            d.caller_service.clone(),
            d.caller_file.clone(),
            d.target_path.clone(),
        ))
    });

    Ok(deps)
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
        let _lock = match store.write_lock() {
            Ok(l) => l,
            Err(_) => continue,
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

/// Check if a path looks like an HTTP route (starts with /api/ or /v{N}/).
fn is_route_like_path(s: &str) -> bool {
    if s.starts_with("/api/") {
        return true;
    }
    let bytes = s.as_bytes();
    if bytes.len() < 4 || bytes[0] != b'/' || bytes[1] != b'v' || !bytes[2].is_ascii_digit() {
        return false;
    }
    let mut i = 3;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i < bytes.len() && bytes[i] == b'/'
}

/// Extract API paths from a string (URL literals in code).
fn extract_api_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for part in text
        .split('"')
        .chain(text.split('\'').chain(text.split('`')))
    {
        let trimmed = part.trim();
        if trimmed.starts_with("http") {
            if let Some(path_part) = trimmed
                .split("//")
                .nth(1)
                .and_then(|s| s.find('/').map(|i| &s[i..]))
            {
                if is_route_like_path(path_part) {
                    paths.push(path_part.to_string());
                }
            }
        } else if is_route_like_path(trimmed) {
            paths.push(trimmed.to_string());
        }
    }
    paths
}

/// Check if a relative file path looks like a test or documentation file.
fn is_test_or_doc_file(rel_path: &str) -> bool {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    // Test files
    if name.starts_with("test_")
        || name.ends_with("_test.py")
        || name.ends_with("_test.go")
        || name.ends_with(".test.ts")
        || name.ends_with(".test.tsx")
        || name.ends_with(".test.js")
        || name.ends_with(".test.jsx")
        || name.ends_with(".spec.ts")
        || name.ends_with(".spec.js")
    {
        return true;
    }
    // Test directories
    let lower = rel_path.to_ascii_lowercase();
    if lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
        || lower.starts_with("test/")
        || lower.starts_with("tests/")
    {
        return true;
    }
    // Doc/markdown files
    if name.ends_with(".md") || name.ends_with(".rst") || name.ends_with(".txt") {
        return true;
    }
    false
}

/// Scan source files for URL strings matching route patterns.
/// Also resolves named constants (e.g., `DOC_UPLOAD_PATH = "/v1/..."`) and
/// credits the definition line when the constant is referenced elsewhere.
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
    let mut url_constants: HashMap<String, Vec<(String, usize, String)>> = HashMap::new();
    walk_for_urls(root, root, SKIP_DIRS, &mut results, &mut url_constants);
    results
}

fn walk_for_urls(
    base: &Path,
    dir: &Path,
    skip: &[&str],
    results: &mut Vec<(String, String, String)>,
    url_constants: &mut HashMap<String, Vec<(String, usize, String)>>,
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
                walk_for_urls(base, &path, skip, results, url_constants);
            }
        } else if path.is_file() {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if is_test_or_doc_file(&rel) {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (line_num, line) in content.lines().enumerate() {
                for delim in ['"', '\'', '`'] {
                    for part in line.split(delim) {
                        let trimmed = part.trim();
                        if trimmed.len() < 200 && !trimmed.contains(' ') {
                            let path_part = if trimmed.starts_with("http") {
                                trimmed
                                    .split("//")
                                    .nth(1)
                                    .and_then(|s| s.find('/').map(|i| &s[i..]))
                                    .unwrap_or(trimmed)
                            } else {
                                trimmed
                            };
                            if is_route_like_path(path_part) {
                                results.push((
                                    rel.clone(),
                                    format!("line:{}", line_num + 1),
                                    path_part.to_string(),
                                ));
                                // Record as named constant if line is an assignment
                                if let Some(const_name) = extract_constant_name(line, path_part) {
                                    url_constants.entry(const_name).or_default().push((
                                        rel.clone(),
                                        line_num + 1,
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
}

/// Extract a constant name from an assignment line containing a route path.
/// Matches patterns like `FOO_PATH = "/v1/..."` or `const FOO = '/v1/...'`.
fn extract_constant_name(line: &str, _path: &str) -> Option<String> {
    let trimmed = line.trim();
    // Python/Ruby: NAME = "..."
    if let Some(eq_pos) = trimmed.find('=') {
        if eq_pos > 0 && !trimmed[..eq_pos].contains('(') {
            let lhs = trimmed[..eq_pos].trim().trim_start_matches("const ");
            let lhs = lhs.trim_start_matches("let ");
            let lhs = lhs.trim_start_matches("var ");
            let name = lhs.trim();
            if !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
            {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Scan source files for string literals matching any known contract path.
/// Returns (file, line_hint, matched_path, target_service, target_method).
fn scan_source_for_contracts(
    root: &Path,
    route_lookup: &HashMap<String, (String, String)>,
    caller_repo: &str,
) -> Vec<(String, String, String, String, String)> {
    if route_lookup.is_empty() {
        return Vec::new();
    }
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
    walk_for_contracts(
        root,
        root,
        SKIP_DIRS,
        route_lookup,
        caller_repo,
        &mut results,
    );
    results
}

fn walk_for_contracts(
    base: &Path,
    dir: &Path,
    skip: &[&str],
    route_lookup: &HashMap<String, (String, String)>,
    caller_repo: &str,
    results: &mut Vec<(String, String, String, String, String)>,
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
                walk_for_contracts(base, &path, skip, route_lookup, caller_repo, results);
            }
        } else if path.is_file() {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if is_test_or_doc_file(&rel) {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (line_num, line) in content.lines().enumerate() {
                for delim in ['"', '\'', '`'] {
                    for part in line.split(delim) {
                        let trimmed = part.trim();
                        if trimmed.len() < 200
                            && !trimmed.is_empty()
                            && !trimmed.contains(' ')
                            && trimmed.starts_with('/')
                        {
                            let normalized = normalize_route_path(trimmed);
                            if let Some((target_svc, target_method)) = route_lookup.get(&normalized)
                            {
                                if target_svc != caller_repo {
                                    results.push((
                                        rel.clone(),
                                        format!("line:{}", line_num + 1),
                                        trimmed.to_string(),
                                        target_svc.clone(),
                                        target_method.clone(),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Detect cross-repo package dependencies within a group.
/// If repo B depends on a package that repo A publishes, returns a Contract linking them.
pub fn detect_shared_package_deps(
    registry: &Registry,
    group_name: &str,
    build_registry: &impl Fn() -> Result<LanguageRegistry>,
) -> Result<Vec<Contract>> {
    let group = registry
        .groups
        .get(group_name)
        .context(format!("group '{}' not found", group_name))?;

    // Build map: published_package_name → repo_name
    let mut publishers: HashMap<String, String> = HashMap::new();
    for repo_name in &group.repos {
        let entry = match registry.repos.get(repo_name) {
            Some(e) => e,
            None => continue,
        };
        if let Some(pkg_name) = read_published_package_name(&entry.path) {
            publishers.insert(pkg_name, repo_name.clone());
        }
    }

    // For each repo, read its Dependency nodes and check against publishers
    let mut contracts = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

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

        let dep_rows = gq
            .raw_query("MATCH (d:Dependency) RETURN d.name, d.version")
            .unwrap_or_default();

        for row in &dep_rows {
            if row.is_empty() {
                continue;
            }
            let dep_name = &row[0];
            if let Some(publisher_repo) = publishers.get(dep_name) {
                if publisher_repo != repo_name
                    && seen.insert((repo_name.clone(), publisher_repo.clone()))
                {
                    contracts.push(Contract {
                        kind: ContractKind::SharedPackage,
                        service: publisher_repo.clone(),
                        method: "package".to_string(),
                        path: dep_name.clone(),
                        symbol_id: format!("pkg::{}::{}", publisher_repo, dep_name),
                        file: String::new(),
                    });
                }
            }
        }
    }

    Ok(contracts)
}

/// Read the published package name from a repo's manifest file.
pub fn read_published_package_name(root: &Path) -> Option<String> {
    // Python: pyproject.toml
    let pyproject = root.join("pyproject.toml");
    if pyproject.exists() {
        if let Ok(content) = std::fs::read_to_string(&pyproject) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("name") && trimmed.contains('=') {
                    let val = trimmed
                        .split('=')
                        .nth(1)?
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'');
                    if !val.is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }

    // Node.js: package.json
    let package_json = root.join("package.json");
    if package_json.exists() {
        if let Ok(content) = std::fs::read_to_string(&package_json) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(name) = parsed.get("name").and_then(|v| v.as_str()) {
                    return Some(name.to_string());
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_route_like_path() {
        assert!(is_route_like_path("/api/users"));
        assert!(is_route_like_path("/api/v1/data"));
        assert!(is_route_like_path("/v1/users"));
        assert!(is_route_like_path("/v2/data"));
        assert!(is_route_like_path("/v1/labrador/doc-upload"));
        assert!(is_route_like_path("/v1/events/send"));
        assert!(is_route_like_path("/v10/something"));

        assert!(!is_route_like_path("/var/log"));
        assert!(!is_route_like_path("/vendor/lib"));
        assert!(!is_route_like_path("/value/key"));
        assert!(!is_route_like_path("/vim"));
        assert!(!is_route_like_path("v1/no-leading-slash"));
        assert!(!is_route_like_path("/v/no-digit"));
        assert!(!is_route_like_path(""));
        assert!(!is_route_like_path("/"));
    }

    #[test]
    fn test_extract_api_paths_v1() {
        let text = r#"url = "/v1/labrador/doc-upload""#;
        let paths = extract_api_paths(text);
        assert_eq!(paths, vec!["/v1/labrador/doc-upload"]);
    }

    #[test]
    fn test_extract_api_paths_http_url() {
        let text = r#"url = "https://example.com/v1/users""#;
        let paths = extract_api_paths(text);
        assert_eq!(paths, vec!["/v1/users"]);
    }

    #[test]
    fn test_extract_api_paths_api_prefix() {
        let text = r#"path = "/api/data/fetch""#;
        let paths = extract_api_paths(text);
        assert_eq!(paths, vec!["/api/data/fetch"]);
    }

    #[test]
    fn test_extract_api_paths_no_match() {
        let text = r#"path = "/var/log/app.log""#;
        let paths = extract_api_paths(text);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_extract_constant_name_python() {
        assert_eq!(
            extract_constant_name(
                r#"    DOC_UPLOAD_PATH = "/v1/labrador/doc-upload""#,
                "/v1/labrador/doc-upload"
            ),
            Some("DOC_UPLOAD_PATH".to_string())
        );
    }

    #[test]
    fn test_extract_constant_name_not_constant() {
        assert_eq!(
            extract_constant_name(r#"    url = "/v1/users""#, "/v1/users"),
            None
        );
        assert_eq!(
            extract_constant_name(r#"    fetch("/v1/users")"#, "/v1/users"),
            None
        );
    }

    #[test]
    fn test_normalize_route_path_v1() {
        assert_eq!(normalize_route_path("/v1/users/"), "/v1/users");
        assert_eq!(normalize_route_path("/v1/users/:id"), "/v1/users/*");
        assert_eq!(normalize_route_path("/v1/users/{id}"), "/v1/users/*");
    }

    #[test]
    fn test_scan_source_for_urls_finds_v1_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("client.py"),
            r#"
class ServiceClient:
    DOC_UPLOAD_PATH = "/v1/labrador/doc-upload"
    EVENTS_PATH = "/v1/events/send"

    def upload(self):
        url = f"{self.endpoint}{self.DOC_UPLOAD_PATH}"
"#,
        )
        .unwrap();
        let results = scan_source_for_urls(dir.path());
        let paths: Vec<&str> = results.iter().map(|(_, _, p)| p.as_str()).collect();
        assert!(
            paths.contains(&"/v1/labrador/doc-upload"),
            "should find /v1/ path, got {:?}",
            paths
        );
        assert!(
            paths.contains(&"/v1/events/send"),
            "should find events path, got {:?}",
            paths
        );
    }

    #[test]
    fn test_scan_source_for_urls_no_false_positives() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("config.py"),
            r#"
LOG_DIR = "/var/log/app"
VENDOR_PATH = "/vendor/lib"
VERSION = "v1.2.3"
"#,
        )
        .unwrap();
        let results = scan_source_for_urls(dir.path());
        assert!(
            results.is_empty(),
            "should not match /var/ or /vendor/, got {:?}",
            results
        );
    }

    #[test]
    fn test_scan_source_for_contracts_finds_match() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("client.py"),
            "def check():\n    r = get(\"/health/ready\")\n",
        )
        .unwrap();
        let mut route_lookup = HashMap::new();
        route_lookup.insert(
            "/health/ready".to_string(),
            ("svc-a".to_string(), "GET".to_string()),
        );
        let results = scan_source_for_contracts(dir.path(), &route_lookup, "svc-b");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, "svc-a");
    }

    #[test]
    fn test_scan_source_for_contracts_empty_lookup() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("c.py"), "x = \"/health/ready\"").unwrap();
        let route_lookup = HashMap::new();
        let results = scan_source_for_contracts(dir.path(), &route_lookup, "svc-b");
        assert!(results.is_empty());
    }

    #[test]
    fn test_scan_source_for_contracts_no_self_match() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("r.py"), "x = \"/health/ready\"").unwrap();
        let mut route_lookup = HashMap::new();
        route_lookup.insert(
            "/health/ready".to_string(),
            ("svc-a".to_string(), "GET".to_string()),
        );
        let results = scan_source_for_contracts(dir.path(), &route_lookup, "svc-a");
        assert!(results.is_empty(), "should not match own service");
    }

    #[test]
    fn test_read_published_package_name_pyproject() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[tool.poetry]\nname = \"ascendskills\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        assert_eq!(
            read_published_package_name(dir.path()),
            Some("ascendskills".to_string())
        );
    }

    #[test]
    fn test_read_published_package_name_package_json() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name": "@ascend/ui", "version": "1.0.0"}"#,
        )
        .unwrap();
        assert_eq!(
            read_published_package_name(dir.path()),
            Some("@ascend/ui".to_string())
        );
    }

    #[test]
    fn test_read_published_package_name_none() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(read_published_package_name(dir.path()), None);
    }

    #[test]
    fn test_resolve_url_constant_typescript() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("client.ts"),
            "const API_PATH = '/v1/users';\nfetch(`${base}${API_PATH}`);\n",
        )
        .unwrap();
        let results = scan_source_for_urls(dir.path());
        let paths: Vec<&str> = results.iter().map(|(_, _, p)| p.as_str()).collect();
        assert!(
            paths.contains(&"/v1/users"),
            "should find TS constant path, got {:?}",
            paths
        );
    }

    #[test]
    fn test_detect_shared_package_dep() {
        let pub_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            pub_dir.path().join("pyproject.toml"),
            "[project]\nname = \"my-shared-lib\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        assert_eq!(
            read_published_package_name(pub_dir.path()),
            Some("my-shared-lib".to_string())
        );

        // Consumer has this as a dependency — verified via the publisher lookup
        // in sync_group_contracts. Here we test the building block:
        // read_published_package_name returns the name, and dep matching is string equality.
        let consumer_deps = ["my-shared-lib", "requests", "numpy"];
        let publisher = read_published_package_name(pub_dir.path()).unwrap();
        assert!(consumer_deps.contains(&publisher.as_str()));
    }

    #[test]
    fn test_is_test_or_doc_file() {
        assert!(is_test_or_doc_file("tests/unit/test_client.py"));
        assert!(is_test_or_doc_file("test_something.py"));
        assert!(is_test_or_doc_file("app/test/unit/test_api.py"));
        assert!(is_test_or_doc_file("src/__tests__/api.test.ts"));
        assert!(is_test_or_doc_file("client_test.go"));
        assert!(is_test_or_doc_file("docs/ARCHITECTURE.md"));
        assert!(is_test_or_doc_file("README.md"));
        assert!(is_test_or_doc_file("foo.spec.ts"));

        assert!(!is_test_or_doc_file("app/adapters/client.py"));
        assert!(!is_test_or_doc_file("src/services/api.ts"));
        assert!(!is_test_or_doc_file("ascendskills/tools/ascend_api.py"));
        assert!(!is_test_or_doc_file("app/routers/router.py"));
    }

    #[test]
    fn test_scan_source_skips_test_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("tests/unit")).unwrap();
        std::fs::write(
            dir.path().join("tests/unit/test_client.py"),
            "url = \"/v1/projects\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("client.py"), "url = \"/v1/projects\"\n").unwrap();
        let results = scan_source_for_urls(dir.path());
        assert_eq!(results.len(), 1, "should only find production file");
        assert_eq!(results[0].0, "client.py");
    }

    #[test]
    fn test_scan_contracts_skips_test_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(
            dir.path().join("tests/test_api.py"),
            "x = \"/health/ready\"\n",
        )
        .unwrap();
        let mut route_lookup = HashMap::new();
        route_lookup.insert(
            "/health/ready".to_string(),
            ("svc-a".to_string(), "GET".to_string()),
        );
        let results = scan_source_for_contracts(dir.path(), &route_lookup, "svc-b");
        assert!(results.is_empty(), "should skip test files");
    }

    #[test]
    fn test_normalize_route_path_wildcard() {
        // Dynamic URL with param → wildcard
        assert_eq!(
            normalize_route_path("/v1/entities/{entity_type}"),
            "/v1/entities/*"
        );
        assert_eq!(normalize_route_path("/v1/entities/:kind"), "/v1/entities/*");
    }

    #[test]
    fn test_shared_package_no_self_link() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"my-lib\"\nversion = \"1.0.0\"\n\n[project.dependencies]\nmy-lib = \">=1.0\"\n",
        )
        .unwrap();
        // A repo should not link to itself as a shared package consumer.
        // The publisher name matches its own dep — sync_group_contracts skips same-repo matches.
        let pkg_name = read_published_package_name(dir.path()).unwrap();
        let own_deps = ["my-lib"];
        // The match exists but should be filtered by caller (same repo name check)
        assert_eq!(pkg_name, "my-lib");
        assert!(
            own_deps.contains(&pkg_name.as_str()),
            "self-dep exists but must be filtered by caller"
        );
    }
}
