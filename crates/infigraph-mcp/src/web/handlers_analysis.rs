use serde_json::{json, Value};

use infigraph_core::clone_detect::find::{find_clones, Options};
use infigraph_core::graph::find_dead_code;
use infigraph_core::path_scope::PathScope;

use super::open_prism;

pub(crate) fn api_architecture(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => {
            let backend = match prism.backend() {
                Some(b) => b,
                None => return json!({"error": "not initialized"}),
            };
            let arch = match backend.get_architecture_stats() {
                Ok(a) => a,
                Err(e) => return json!({"error": e.to_string()}),
            };
            let stats = prism.stats().ok();

            let langs: Vec<Value> = arch
                .languages
                .iter()
                .map(|l| json!([l.language, l.count.to_string()]))
                .collect();
            let kinds: Vec<Value> = arch
                .kind_counts
                .iter()
                .map(|k| json!([k.kind, k.count.to_string()]))
                .collect();
            let hotspots: Vec<Value> = arch
                .hotspot_files
                .iter()
                .map(|h| json!([h.file, h.count.to_string()]))
                .collect();
            let hubs: Vec<Value> = arch
                .hub_functions
                .iter()
                .map(|h| json!([h.name, h.file, h.calls.to_string()]))
                .collect();

            json!({
                "languages": langs,
                "symbolKinds": kinds,
                "hotspots": hotspots,
                "hubs": hubs,
                "stats": stats.map(|s| json!({
                    "symbols": s.symbols,
                    "modules": s.modules,
                    "calls": s.calls,
                    "inherits": s.inherits,
                    "contains": s.contains,
                })),
            })
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(crate) fn api_dead_code(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => {
            let backend = match prism.backend() {
                Some(b) => b,
                None => return json!({"error": "not initialized"}),
            };
            let scope = crate::tools::helpers::path_scope(&prism, params);
            let rows = match find_dead_code(backend, &scope) {
                Ok(r) => r,
                Err(e) => return json!({"error": e.to_string()}),
            };
            let dead: Vec<Value> = rows
                .iter()
                .map(|r| json!({"name": r.name, "kind": r.kind, "file": r.file}))
                .collect();

            json!({"deadCode": dead, "count": dead.len()})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(crate) fn api_graph_data(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => {
            let backend = match prism.backend() {
                Some(b) => b,
                None => return json!({"error": "not initialized"}),
            };

            let nodes = backend
                .raw_query("MATCH (s:Symbol) RETURN s.id, s.name, s.kind, s.file")
                .unwrap_or_default();
            let calls = backend
                .raw_query("MATCH (a:Symbol)-[:CALLS]->(b:Symbol) RETURN a.id, b.id")
                .unwrap_or_default();
            let inherits = backend
                .raw_query("MATCH (a:Symbol)-[:INHERITS]->(b:Symbol) RETURN a.id, b.id")
                .unwrap_or_default();

            let node_items: Vec<Value> = nodes
                .iter()
                .map(|r| json!({"id": r[0], "name": r[1], "kind": r[2], "file": r[3]}))
                .collect();
            let edge_items: Vec<Value> = calls
                .iter()
                .map(|r| json!({"from": r[0], "to": r[1], "type": "CALLS"}))
                .chain(
                    inherits
                        .iter()
                        .map(|r| json!({"from": r[0], "to": r[1], "type": "INHERITS"})),
                )
                .collect();

            json!({"nodes": node_items, "edges": edge_items})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(crate) fn api_stats(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => match prism.stats() {
            Ok(s) => json!({
                "symbols": s.symbols,
                "modules": s.modules,
                "calls": s.calls,
                "inherits": s.inherits,
                "contains": s.contains,
            }),
            Err(e) => json!({"error": e.to_string()}),
        },
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(crate) fn api_cluster(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => {
            let backend = match prism.backend() {
                Some(b) => b,
                None => return json!({"error": "not initialized"}),
            };
            match infigraph_core::cluster::detect_clusters(backend) {
                Ok(stats) => json!({
                    "clusters": stats.num_clusters,
                    "modularity": stats.modularity,
                    "sizes": stats.cluster_sizes,
                }),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(crate) fn api_routes(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => {
            let backend = match prism.backend() {
                Some(b) => b,
                None => return json!({"error": "not initialized"}),
            };
            match infigraph_core::routes::detect_routes(backend) {
                Ok(routes) => {
                    let items: Vec<Value> = routes
                        .iter()
                        .map(|r| {
                            json!({
                                "method": r.method,
                                "path": r.path,
                                "handler": r.handler_id,
                                "file": r.file,
                            })
                        })
                        .collect();
                    json!({"routes": items, "count": items.len()})
                }
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(crate) fn api_groups(_params: &Value) -> Value {
    let registry_path = infigraph_core::multi::registry_path().unwrap_or_default();
    if !registry_path.exists() {
        return json!({"groups": [], "count": 0});
    }
    match std::fs::read_to_string(&registry_path) {
        Ok(content) => {
            let reg: Value = serde_json::from_str(&content).unwrap_or(json!({}));
            let groups = reg.get("groups").cloned().unwrap_or(json!({}));
            let group_list: Vec<Value> = if let Some(obj) = groups.as_object() {
                obj.iter()
                    .map(|(name, g)| {
                        let repos = g.get("repos").and_then(|r| r.as_array()).map(|a| a.len()).unwrap_or(0);
                        let contracts = g.get("contracts").and_then(|c| c.as_array()).map(|a| a.len()).unwrap_or(0);
                        json!({"name": name, "repoCount": repos, "contractCount": contracts, "repos": g.get("repos"), "contracts": g.get("contracts")})
                    })
                    .collect()
            } else {
                vec![]
            };
            json!({"groups": group_list, "count": group_list.len()})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(crate) fn api_contracts(params: &Value) -> Value {
    let group_name = params.get("group").and_then(|g| g.as_str()).unwrap_or("");
    let registry_path = infigraph_core::multi::registry_path().unwrap_or_default();
    if !registry_path.exists() {
        return json!({"contracts": [], "count": 0});
    }
    match std::fs::read_to_string(&registry_path) {
        Ok(content) => {
            let reg: Value = serde_json::from_str(&content).unwrap_or(json!({}));
            let contracts = if group_name.is_empty() {
                // Return all contracts from all groups
                let mut all = Vec::new();
                if let Some(groups) = reg.get("groups").and_then(|g| g.as_object()) {
                    for (_name, g) in groups {
                        if let Some(cs) = g.get("contracts").and_then(|c| c.as_array()) {
                            all.extend(cs.clone());
                        }
                    }
                }
                all
            } else {
                reg.get("groups")
                    .and_then(|g| g.get(group_name))
                    .and_then(|g| g.get("contracts"))
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default()
            };
            json!({"contracts": contracts, "count": contracts.len()})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(crate) fn api_complexity(params: &Value) -> Value {
    let threshold = params
        .get("threshold")
        .and_then(|v| v.as_u64())
        .unwrap_or(5) as i64;
    match open_prism(params) {
        Ok(prism) => {
            let backend = match prism.backend() {
                Some(b) => b,
                None => return json!({"error":"not initialized"}),
            };
            let rows = match backend.get_complexity_ranking(None) {
                Ok(r) => r,
                Err(e) => return json!({"error": e.to_string()}),
            };

            let items: Vec<Value> = rows
                .iter()
                .filter(|r| r.complexity > 0)
                .map(|r| {
                    json!({
                        "id": "", "name": r.name, "kind": "", "file": r.file,
                        "line": r.start_line,
                        "complexity": r.complexity as i64,
                    })
                })
                .collect();

            let hotspots: Vec<&Value> = items
                .iter()
                .filter(|v| v["complexity"].as_i64().unwrap_or(0) >= threshold)
                .collect();
            let avg = if items.is_empty() {
                0.0
            } else {
                items
                    .iter()
                    .map(|v| v["complexity"].as_f64().unwrap_or(1.0))
                    .sum::<f64>()
                    / items.len() as f64
            };
            json!({"symbols": items, "hotspots": hotspots, "avg": avg, "threshold": threshold})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(crate) fn api_security(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => match infigraph_core::security::scan_project(prism.root()) {
            Ok(stats) => {
                let findings: Vec<Value> = stats
                    .findings
                    .iter()
                    .map(|f| {
                        json!({
                            "file": f.file, "line": f.line, "col": f.col,
                            "severity": f.severity.to_string(),
                            "category": f.category.to_string(),
                            "rule_id": f.rule_id,
                            "message": f.message,
                            "snippet": f.snippet,
                        })
                    })
                    .collect();
                json!({
                    "findings": findings,
                    "total": findings.len(),
                    "critical": stats.critical_count(),
                    "high": stats.high_count(),
                    "medium": stats.medium_count(),
                    "low": stats.low_count(),
                })
            }
            Err(e) => json!({"error": e.to_string()}),
        },
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(crate) fn api_bridges(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => match infigraph_core::bridges::detect_bridges(prism.root()) {
            Ok(result) => {
                let items: Vec<Value> = result
                    .bridges
                    .iter()
                    .map(|b| {
                        json!({
                            "file": b.file, "line": b.line,
                            "kind": b.kind.as_str(),
                            "foreign_symbol": b.foreign_symbol,
                            "source_language": b.source_language,
                            "target_language": b.target_language,
                            "detail": b.detail,
                        })
                    })
                    .collect();
                json!({"bridges": items, "total": items.len()})
            }
            Err(e) => json!({"error": e.to_string()}),
        },
        Err(e) => json!({"error": e.to_string()}),
    }
}

pub(crate) fn api_clones(params: &Value) -> Value {
    let threshold = params
        .get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.92) as f32;
    let prism = match open_prism(params) {
        Ok(prism) => prism,
        Err(e) => return json!({"error": e.to_string()}),
    };
    let Some(backend) = prism.backend() else {
        return json!({"error":"not initialized"});
    };
    let report = Options::for_project(
        prism.root(),
        threshold,
        50,
        "Function,Method",
        PathScope::everything(),
    )
    .and_then(|options| find_clones(backend, prism.root(), &options));
    match report {
        Ok(report) => {
            let side = |s: &infigraph_core::clone_detect::find::Side| json!({"id": s.id, "name": s.name, "file": s.file});
            let pairs: Vec<Value> = report
                .pairs
                .iter()
                .map(|p| {
                    json!({
                        "score": p.score,
                        "signal": p.signal.label(),
                        "a": side(&p.a),
                        "b": side(&p.b),
                    })
                })
                .collect();
            json!({
                "pairs": pairs,
                "total": pairs.len(),
                "checked": report.symbols,
                "mode": report.mode.label(),
                "truncated": report.truncated,
            })
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}
