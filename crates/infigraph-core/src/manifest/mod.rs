/// Manifest parser: reads package manifests and lockfiles, extracts dependencies,
/// stores them as Dependency nodes with DEPENDS_ON edges in the graph.
///
/// Supported: package.json, Cargo.toml, go.mod, pom.xml, build.gradle,
///            requirements.txt, pyproject.toml, Gemfile, composer.json,
///            packages.config, *.csproj, pubspec.yaml
use std::path::Path;

use anyhow::Result;

use crate::graph::GraphStore;

#[derive(Debug, Clone)]
pub struct DepEntry {
    pub name: String,
    pub version: String,
    pub ecosystem: String,
    pub is_dev: bool,
}

#[derive(Debug, Default)]
pub struct ManifestResult {
    pub ecosystem: String,
    pub manifest_file: String,
    pub deps: Vec<DepEntry>,
}

/// Scan a project root for manifests, parse them, store deps in graph.
pub fn index_manifests(root: &Path, store: &GraphStore) -> Result<Vec<ManifestResult>> {
    let mut results = Vec::new();

    let candidates = [
        "package.json",
        "Cargo.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "requirements.txt",
        "pyproject.toml",
        "Gemfile",
        "composer.json",
        "packages.config",
        "pubspec.yaml",
    ];

    for name in &candidates {
        let path = root.join(name);
        if path.exists() {
            if let Ok(result) = parse_manifest(&path) {
                store_manifest(store, &result)?;
                results.push(result);
            }
        }
    }

    // Also scan for *.csproj files (can be nested)
    scan_csproj(root, store, &mut results)?;

    Ok(results)
}

/// Query dependencies stored in graph for a project.
pub fn query_deps(store: &GraphStore) -> Result<Vec<DepEntry>> {
    let conn = store.connection()?;
    let q = "MATCH (d:Dependency) RETURN d.name, d.version, d.ecosystem, d.is_dev ORDER BY d.ecosystem, d.name";
    let result = conn
        .query(q)
        .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;

    let mut deps = Vec::new();
    for row in result {
        if row.len() >= 4 {
            deps.push(DepEntry {
                name: row[0].to_string().trim_matches('"').to_string(),
                version: row[1].to_string().trim_matches('"').to_string(),
                ecosystem: row[2].to_string().trim_matches('"').to_string(),
                is_dev: row[3].to_string() == "True" || row[3].to_string() == "true",
            });
        }
    }
    Ok(deps)
}

fn parse_manifest(path: &Path) -> Result<ManifestResult> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let content = std::fs::read_to_string(path)?;

    match name.as_ref() {
        "package.json" => parse_package_json(&content, path),
        "Cargo.toml" => parse_cargo_toml(&content, path),
        "go.mod" => parse_go_mod(&content, path),
        "pom.xml" => parse_pom_xml(&content, path),
        "build.gradle" | "build.gradle.kts" => parse_gradle(&content, path),
        "requirements.txt" => parse_requirements_txt(&content, path),
        "pyproject.toml" => parse_pyproject_toml(&content, path),
        "Gemfile" => parse_gemfile(&content, path),
        "composer.json" => parse_composer_json(&content, path),
        "packages.config" => parse_packages_config(&content, path),
        "pubspec.yaml" => parse_pubspec_yaml(&content, path),
        _ => anyhow::bail!("unknown manifest: {}", name),
    }
}

fn parse_package_json(content: &str, path: &Path) -> Result<ManifestResult> {
    let v: serde_json::Value = serde_json::from_str(content)?;
    let mut deps = Vec::new();

    if let Some(obj) = v.get("dependencies").and_then(|d| d.as_object()) {
        for (name, ver) in obj {
            deps.push(DepEntry {
                name: name.clone(),
                version: ver.as_str().unwrap_or("*").to_string(),
                ecosystem: "npm".to_string(),
                is_dev: false,
            });
        }
    }
    if let Some(obj) = v.get("devDependencies").and_then(|d| d.as_object()) {
        for (name, ver) in obj {
            deps.push(DepEntry {
                name: name.clone(),
                version: ver.as_str().unwrap_or("*").to_string(),
                ecosystem: "npm".to_string(),
                is_dev: true,
            });
        }
    }
    if let Some(obj) = v.get("peerDependencies").and_then(|d| d.as_object()) {
        for (name, ver) in obj {
            deps.push(DepEntry {
                name: name.clone(),
                version: ver.as_str().unwrap_or("*").to_string(),
                ecosystem: "npm".to_string(),
                is_dev: false,
            });
        }
    }

    Ok(ManifestResult {
        ecosystem: "npm".to_string(),
        manifest_file: path.to_string_lossy().replace('\\', "/"),
        deps,
    })
}

fn parse_cargo_toml(content: &str, path: &Path) -> Result<ManifestResult> {
    let v: toml::Value = content.parse()?;
    let mut deps = Vec::new();

    for (section, is_dev) in &[
        ("dependencies", false),
        ("dev-dependencies", true),
        ("build-dependencies", true),
    ] {
        if let Some(table) = v.get(section).and_then(|d| d.as_table()) {
            for (name, val) in table {
                let version = match val {
                    toml::Value::String(s) => s.clone(),
                    toml::Value::Table(t) => t
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("*")
                        .to_string(),
                    _ => "*".to_string(),
                };
                // Skip workspace = true entries (no version)
                if val.as_table().and_then(|t| t.get("workspace")).is_some() {
                    continue;
                }
                deps.push(DepEntry {
                    name: name.clone(),
                    version,
                    ecosystem: "cargo".to_string(),
                    is_dev: *is_dev,
                });
            }
        }
    }

    Ok(ManifestResult {
        ecosystem: "cargo".to_string(),
        manifest_file: path.to_string_lossy().replace('\\', "/"),
        deps,
    })
}

fn parse_go_mod(content: &str, path: &Path) -> Result<ManifestResult> {
    let mut deps = Vec::new();
    let mut in_require = false;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("require (") || line == "require (" {
            in_require = true;
            continue;
        }
        if in_require && line == ")" {
            in_require = false;
            continue;
        }
        // Single-line: require module v1.2.3
        let parts: Vec<&str> = if in_require {
            line.split_whitespace().collect()
        } else if let Some(stripped) = line.strip_prefix("require ") {
            stripped.split_whitespace().collect()
        } else {
            continue;
        };

        if parts.len() >= 2 {
            let is_indirect = parts
                .get(2)
                .map(|s| s.contains("indirect"))
                .unwrap_or(false);
            deps.push(DepEntry {
                name: parts[0].to_string(),
                version: parts[1].to_string(),
                ecosystem: "go".to_string(),
                is_dev: is_indirect,
            });
        }
    }

    Ok(ManifestResult {
        ecosystem: "go".to_string(),
        manifest_file: path.to_string_lossy().replace('\\', "/"),
        deps,
    })
}

fn parse_pom_xml(content: &str, path: &Path) -> Result<ManifestResult> {
    // Simple regex-based extraction (no full XML parse needed)
    let dep_re = regex::Regex::new(
        r"<dependency>\s*<groupId>([^<]+)</groupId>\s*<artifactId>([^<]+)</artifactId>\s*(?:<version>([^<]+)</version>\s*)?(?:<scope>([^<]+)</scope>\s*)?"
    ).unwrap();

    let mut deps = Vec::new();
    for cap in dep_re.captures_iter(content) {
        let group = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let artifact = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let version = cap.get(3).map(|m| m.as_str()).unwrap_or("*");
        let scope = cap.get(4).map(|m| m.as_str()).unwrap_or("compile");
        let is_dev = matches!(scope, "test" | "provided");
        deps.push(DepEntry {
            name: format!("{}:{}", group.trim(), artifact.trim()),
            version: version.trim().to_string(),
            ecosystem: "maven".to_string(),
            is_dev,
        });
    }

    Ok(ManifestResult {
        ecosystem: "maven".to_string(),
        manifest_file: path.to_string_lossy().replace('\\', "/"),
        deps,
    })
}

fn parse_gradle(content: &str, path: &Path) -> Result<ManifestResult> {
    // Match: implementation 'group:artifact:version' or testImplementation("...")
    let re = regex::Regex::new(
        r#"(?:implementation|api|compileOnly|runtimeOnly|testImplementation|testCompileOnly|annotationProcessor)\s*[("']([^"'()]+)[)"']"#
    ).unwrap();

    let mut deps = Vec::new();
    for cap in re.captures_iter(content) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let is_dev = cap
            .get(0)
            .map(|m| m.as_str().starts_with("test"))
            .unwrap_or(false);
        let parts: Vec<&str> = spec.split(':').collect();
        let name = if parts.len() >= 2 {
            format!("{}:{}", parts[0], parts[1])
        } else {
            spec.to_string()
        };
        let version = parts.get(2).unwrap_or(&"*").to_string();
        deps.push(DepEntry {
            name,
            version,
            ecosystem: "gradle".to_string(),
            is_dev,
        });
    }

    Ok(ManifestResult {
        ecosystem: "gradle".to_string(),
        manifest_file: path.to_string_lossy().replace('\\', "/"),
        deps,
    })
}

fn parse_requirements_txt(content: &str, path: &Path) -> Result<ManifestResult> {
    let mut deps = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        // Handle: name==1.0, name>=1.0, name~=1.0, name
        let (name, version) = if let Some(idx) = line.find(['=', '>', '<', '~', '!']) {
            (
                line[..idx].trim().to_string(),
                line[idx..].trim().to_string(),
            )
        } else {
            (line.to_string(), "*".to_string())
        };
        if !name.is_empty() {
            deps.push(DepEntry {
                name,
                version,
                ecosystem: "pip".to_string(),
                is_dev: false,
            });
        }
    }

    Ok(ManifestResult {
        ecosystem: "pip".to_string(),
        manifest_file: path.to_string_lossy().replace('\\', "/"),
        deps,
    })
}

fn parse_pyproject_toml(content: &str, path: &Path) -> Result<ManifestResult> {
    let v: toml::Value = content.parse()?;
    let mut deps = Vec::new();

    // PEP 621: [project] dependencies
    if let Some(arr) = v
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for dep in arr {
            if let Some(s) = dep.as_str() {
                let (name, ver) = split_pep508(s);
                deps.push(DepEntry {
                    name,
                    version: ver,
                    ecosystem: "pip".to_string(),
                    is_dev: false,
                });
            }
        }
    }
    // Poetry: [tool.poetry.dependencies]
    if let Some(table) = v
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for (name, val) in table {
            if name == "python" {
                continue;
            }
            let version = match val {
                toml::Value::String(s) => s.clone(),
                toml::Value::Table(t) => t
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("*")
                    .to_string(),
                _ => "*".to_string(),
            };
            deps.push(DepEntry {
                name: name.clone(),
                version,
                ecosystem: "pip".to_string(),
                is_dev: false,
            });
        }
    }
    if let Some(table) = v
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dev-dependencies"))
        .and_then(|d| d.as_table())
    {
        for (name, val) in table {
            let version = match val {
                toml::Value::String(s) => s.clone(),
                _ => "*".to_string(),
            };
            deps.push(DepEntry {
                name: name.clone(),
                version,
                ecosystem: "pip".to_string(),
                is_dev: true,
            });
        }
    }

    Ok(ManifestResult {
        ecosystem: "pip".to_string(),
        manifest_file: path.to_string_lossy().replace('\\', "/"),
        deps,
    })
}

fn parse_gemfile(content: &str, path: &Path) -> Result<ManifestResult> {
    let re = regex::Regex::new(r#"gem\s+['"]([^'"]+)['"](?:\s*,\s*['"]([^'"]+)['"])?"#).unwrap();
    let mut deps = Vec::new();
    let mut in_test_group = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("group :test") || trimmed.starts_with("group :development") {
            in_test_group = true;
        }
        if trimmed == "end" {
            in_test_group = false;
        }
        if let Some(cap) = re.captures(trimmed) {
            let name = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let version = cap.get(2).map(|m| m.as_str()).unwrap_or("*").to_string();
            deps.push(DepEntry {
                name,
                version,
                ecosystem: "gem".to_string(),
                is_dev: in_test_group,
            });
        }
    }

    Ok(ManifestResult {
        ecosystem: "gem".to_string(),
        manifest_file: path.to_string_lossy().replace('\\', "/"),
        deps,
    })
}

fn parse_composer_json(content: &str, path: &Path) -> Result<ManifestResult> {
    let v: serde_json::Value = serde_json::from_str(content)?;
    let mut deps = Vec::new();

    for (key, is_dev) in &[("require", false), ("require-dev", true)] {
        if let Some(obj) = v.get(*key).and_then(|d| d.as_object()) {
            for (name, ver) in obj {
                if name == "php" {
                    continue;
                }
                deps.push(DepEntry {
                    name: name.clone(),
                    version: ver.as_str().unwrap_or("*").to_string(),
                    ecosystem: "composer".to_string(),
                    is_dev: *is_dev,
                });
            }
        }
    }

    Ok(ManifestResult {
        ecosystem: "composer".to_string(),
        manifest_file: path.to_string_lossy().replace('\\', "/"),
        deps,
    })
}

fn parse_packages_config(content: &str, path: &Path) -> Result<ManifestResult> {
    let re = regex::Regex::new(r#"<package\s+id="([^"]+)"\s+version="([^"]+)""#).unwrap();
    let dev_re = regex::Regex::new(r#"developmentDependency="true""#).unwrap();
    let mut deps = Vec::new();

    for line in content.lines() {
        if let Some(cap) = re.captures(line) {
            let is_dev = dev_re.is_match(line);
            deps.push(DepEntry {
                name: cap[1].to_string(),
                version: cap[2].to_string(),
                ecosystem: "nuget".to_string(),
                is_dev,
            });
        }
    }

    Ok(ManifestResult {
        ecosystem: "nuget".to_string(),
        manifest_file: path.to_string_lossy().replace('\\', "/"),
        deps,
    })
}

fn parse_pubspec_yaml(content: &str, path: &Path) -> Result<ManifestResult> {
    // Simple line-based parse for pubspec.yaml dependencies sections
    let mut deps = Vec::new();
    let mut in_deps = false;
    let mut in_dev_deps = false;
    let dep_re = regex::Regex::new(r"^\s{2}(\w[\w_-]*):\s*(.*)$").unwrap();

    for line in content.lines() {
        if line.starts_with("dependencies:") {
            in_deps = true;
            in_dev_deps = false;
            continue;
        }
        if line.starts_with("dev_dependencies:") {
            in_dev_deps = true;
            in_deps = false;
            continue;
        }
        if !line.starts_with(' ') && !line.is_empty() {
            in_deps = false;
            in_dev_deps = false;
        }

        if in_deps || in_dev_deps {
            if let Some(cap) = dep_re.captures(line) {
                let name = cap[1].to_string();
                let raw_ver = cap[2].trim().to_string();
                let version = if raw_ver.is_empty() || raw_ver == "any" {
                    "*".to_string()
                } else {
                    raw_ver
                };
                if name != "flutter" && name != "sdk" {
                    deps.push(DepEntry {
                        name,
                        version,
                        ecosystem: "pub".to_string(),
                        is_dev: in_dev_deps,
                    });
                }
            }
        }
    }

    Ok(ManifestResult {
        ecosystem: "pub".to_string(),
        manifest_file: path.to_string_lossy().replace('\\', "/"),
        deps,
    })
}

fn scan_csproj(root: &Path, store: &GraphStore, results: &mut Vec<ManifestResult>) -> Result<()> {
    let re =
        regex::Regex::new(r#"<PackageReference\s+Include="([^"]+)"\s+Version="([^"]+)""#).unwrap();
    scan_csproj_dir(root, &re, store, results)
}

fn scan_csproj_dir(
    dir: &Path,
    re: &regex::Regex,
    store: &GraphStore,
    results: &mut Vec<ManifestResult>,
) -> Result<()> {
    let ignore = [".git", "node_modules", "target", "bin", "obj"];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() && !ignore.contains(&name_str.as_ref()) {
            scan_csproj_dir(&path, re, store, results)?;
        } else if path
            .extension()
            .map(|e| e == "csproj" || e == "fsproj" || e == "vbproj")
            .unwrap_or(false)
        {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let mut deps = Vec::new();
                for cap in re.captures_iter(&content) {
                    deps.push(DepEntry {
                        name: cap[1].to_string(),
                        version: cap[2].to_string(),
                        ecosystem: "nuget".to_string(),
                        is_dev: false,
                    });
                }
                if !deps.is_empty() {
                    let result = ManifestResult {
                        ecosystem: "nuget".to_string(),
                        manifest_file: path.to_string_lossy().replace('\\', "/"),
                        deps,
                    };
                    let _ = store_manifest(store, &result);
                    results.push(result);
                }
            }
        }
    }
    Ok(())
}

fn store_manifest(store: &GraphStore, result: &ManifestResult) -> Result<()> {
    let _lock = store.write_lock()?;
    let conn = store.connection()?;

    for dep in &result.deps {
        let id = format!("{}::{}", dep.ecosystem, dep.name);
        // Upsert Dependency node
        let check = format!(
            "MATCH (d:Dependency) WHERE d.id = '{}' RETURN d.id",
            escape(&id)
        );
        let mut r = conn.query(&check).map_err(|e| anyhow::anyhow!("{e}"))?;
        if r.next().is_none() {
            let insert = format!(
                "CREATE (d:Dependency {{id: '{}', name: '{}', version: '{}', ecosystem: '{}', is_dev: {}}})",
                escape(&id), escape(&dep.name), escape(&dep.version), escape(&dep.ecosystem), dep.is_dev
            );
            let _ = conn.query(&insert);
        } else {
            let update = format!(
                "MATCH (d:Dependency) WHERE d.id = '{}' SET d.version = '{}', d.is_dev = {}",
                escape(&id),
                escape(&dep.version),
                dep.is_dev
            );
            let _ = conn.query(&update);
        }

        // Wire DEPENDS_ON from the manifest's Module (or first Module in project)
        let manifest_mod_id = &result.manifest_file;
        let rel = format!(
            "MATCH (m:Module), (d:Dependency) WHERE m.file CONTAINS '{}' AND d.id = '{}' \
             CREATE (m)-[:DEPENDS_ON {{is_dev: {}}}]->(d)",
            escape(result.manifest_file.rsplit('/').next().unwrap_or("")),
            escape(&id),
            dep.is_dev
        );
        let _ = conn.query(&rel);
        let _ = manifest_mod_id;
    }
    Ok(())
}

fn split_pep508(s: &str) -> (String, String) {
    if let Some(idx) = s.find(['=', '>', '<', '~', '!', '[', ';']) {
        (s[..idx].trim().to_string(), s[idx..].trim().to_string())
    } else {
        (s.trim().to_string(), "*".to_string())
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_upsert_updates_version() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("graph");
        let store = crate::graph::GraphStore::open(&db_path).unwrap();
        let result1 = ManifestResult {
            manifest_file: "pyproject.toml".to_string(),
            ecosystem: "pypi".to_string(),
            deps: vec![DepEntry {
                name: "requests".to_string(),
                version: "1.0".to_string(),
                ecosystem: "pypi".to_string(),
                is_dev: false,
            }],
        };
        store_manifest(&store, &result1).unwrap();

        let conn = store.connection().unwrap();
        let gq = crate::graph::GraphQuery::new(&conn);
        let rows = gq
            .raw_query("MATCH (d:Dependency) WHERE d.id = 'pypi::requests' RETURN d.version")
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "1.0");

        let result2 = ManifestResult {
            manifest_file: "pyproject.toml".to_string(),
            ecosystem: "pypi".to_string(),
            deps: vec![DepEntry {
                name: "requests".to_string(),
                version: "2.0".to_string(),
                ecosystem: "pypi".to_string(),
                is_dev: false,
            }],
        };
        store_manifest(&store, &result2).unwrap();

        let rows2 = gq
            .raw_query("MATCH (d:Dependency) WHERE d.id = 'pypi::requests' RETURN d.version")
            .unwrap();
        assert_eq!(rows2.len(), 1);
        assert_eq!(rows2[0][0], "2.0");
    }
}
