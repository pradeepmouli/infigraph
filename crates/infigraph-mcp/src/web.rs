use std::path::PathBuf;
use std::thread;

use anyhow::Result;
use serde_json::{json, Value};
use tiny_http::{Header, Response, Server};

use infigraph_core::embed;
use infigraph_core::graph::GraphQuery;
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

/// Start the web UI server on the given port. Runs in a background thread.
pub fn start_ui_server(port: u16) -> bool {
    let addr = format!("0.0.0.0:{}", port);
    // Pre-check: try binding before spawning thread so caller knows outcome
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(_) => return false,
    };
    thread::spawn(move || {
        let server = server;

        for mut request in server.incoming_requests() {
            let url = request.url().to_string();
            let method = request.method().to_string();

            // Strip query string for route matching
            let route = url.split('?').next().unwrap_or(&url);

            let response = match (method.as_str(), route) {
                ("GET", "/") => serve_html(INDEX_HTML, "text/html"),
                ("GET", "/api/health") => serve_json(json!({"status": "ok"})),

                // API endpoints
                ("POST", "/api/index") => handle_api_post(&mut request, api_index),
                ("POST", "/api/search") => handle_api_post(&mut request, api_search),
                ("POST", "/api/query") => handle_api_post(&mut request, api_query),
                ("POST", "/api/architecture") => handle_api_post(&mut request, api_architecture),
                ("POST", "/api/dead-code") => handle_api_post(&mut request, api_dead_code),
                ("POST", "/api/symbols") => handle_api_post(&mut request, api_symbols),
                ("POST", "/api/symbol-context") => {
                    handle_api_post(&mut request, api_symbol_context)
                }
                ("POST", "/api/snippet") => handle_api_post(&mut request, api_snippet),
                ("POST", "/api/graph-data") => handle_api_post(&mut request, api_graph_data),
                ("POST", "/api/stats") => handle_api_post(&mut request, api_stats),
                ("POST", "/api/cluster") => handle_api_post(&mut request, api_cluster),
                ("POST", "/api/chat") => handle_api_post(&mut request, api_chat),
                ("POST", "/api/routes") => handle_api_post(&mut request, api_routes),
                ("POST", "/api/groups") => handle_api_post(&mut request, api_groups),
                ("POST", "/api/contracts") => handle_api_post(&mut request, api_contracts),
                ("POST", "/api/complexity") => handle_api_post(&mut request, api_complexity),
                ("POST", "/api/security") => handle_api_post(&mut request, api_security),
                ("POST", "/api/bridges") => handle_api_post(&mut request, api_bridges),
                ("POST", "/api/clones") => handle_api_post(&mut request, api_clones),
                ("POST", "/api/git-summary") => handle_api_post(&mut request, api_git_summary),

                _ => serve_html("404 Not Found", "text/plain"),
            };

            let _ = request.respond(response);
        }
    });
    true
}

fn serve_html(body: &str, content_type: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let data = body.as_bytes().to_vec();
    let header = Header::from_bytes("Content-Type", content_type).unwrap();
    Response::from_data(data).with_header(header)
}

fn serve_json(value: Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_string(&value).unwrap_or_default();
    let data = body.into_bytes();
    let ct = Header::from_bytes("Content-Type", "application/json").unwrap();
    let cors = Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap();
    Response::from_data(data).with_header(ct).with_header(cors)
}

fn handle_api_post(
    request: &mut tiny_http::Request,
    handler: fn(&Value) -> Value,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);

    let params: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let result = handler(&params);
    serve_json(result)
}

fn open_prism(params: &Value) -> Result<Infigraph> {
    let path = params.get("path").and_then(|p| p.as_str()).unwrap_or(".");
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(&PathBuf::from(path), registry)?;
    prism.init()?;
    Ok(prism)
}

fn api_index(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => match prism.index() {
            Ok(result) => json!({
                "success": true,
                "files": result.indexed_files,
                "total": result.total_files,
                "resolve": format!("{}", result.resolve_stats),
            }),
            Err(e) => json!({"error": e.to_string()}),
        },
        Err(e) => json!({"error": e.to_string()}),
    }
}

fn api_search(params: &Value) -> Value {
    let query = params.get("query").and_then(|q| q.as_str()).unwrap_or("");
    let limit = params.get("limit").and_then(|l| l.as_u64()).unwrap_or(20) as usize;

    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error": "not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error": e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);

            let rows = match gq
                .raw_query("MATCH (s:Symbol) RETURN s.id, s.name, s.kind, s.file, s.docstring")
            {
                Ok(r) => r,
                Err(e) => return json!({"error": e.to_string()}),
            };

            let docs: Vec<(String, String)> = rows
                .iter()
                .map(|row| {
                    let id = row[0].clone();
                    let text = if row.get(4).is_some_and(|s| !s.is_empty()) {
                        format!("{} {}: {}", row[2], row[1], row[4])
                    } else {
                        format!("{} {}", row[2], row[1])
                    };
                    (id, text)
                })
                .collect();

            let bm25 = infigraph_core::search::BM25Index::build(docs.clone());
            let embedder = embed::best_embedder();
            let emb_path = prism.root().join(".infigraph").join("embeddings.bin");
            let embs: Vec<(String, Vec<f32>)> = if emb_path.exists() {
                match embed::load_embeddings_cached(&emb_path) {
                    Ok(e) => e,
                    Err(_) => docs
                        .iter()
                        .map(|(id, text)| (id.clone(), embedder.embed(text).unwrap_or_default()))
                        .collect(),
                }
            } else {
                docs.iter()
                    .map(|(id, text)| (id.clone(), embedder.embed(text).unwrap_or_default()))
                    .collect()
            };

            let hnsw_path = prism.root().join(".infigraph").join("hnsw_index.usearch");
            match infigraph_core::search::hybrid_search(
                query,
                &bm25,
                embedder.as_ref(),
                &embs,
                limit,
                0.3,
                Some(&hnsw_path),
                Some(&emb_path),
            ) {
                Ok(results) => {
                    let items: Vec<Value> = results
                        .iter()
                        .filter_map(|r| {
                            rows.iter().find(|row| row[0] == r.symbol_id).map(|row| {
                                json!({
                                    "id": row[0],
                                    "name": row[1],
                                    "kind": row[2],
                                    "file": row[3],
                                    "score": r.score,
                                    "bm25": r.bm25_score,
                                    "vector": r.vector_score,
                                })
                            })
                        })
                        .collect();
                    json!({"results": items})
                }
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

fn api_query(params: &Value) -> Value {
    let cypher = params.get("cypher").and_then(|c| c.as_str()).unwrap_or("");

    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error": "not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error": e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);
            match gq.raw_query(cypher) {
                Ok(rows) => json!({"rows": rows}),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

fn api_architecture(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error": "not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error": e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);

            let langs = gq
                .raw_query("MATCH (m:Module) RETURN m.language, count(m)")
                .unwrap_or_default();
            let kinds = gq
                .raw_query("MATCH (s:Symbol) RETURN s.kind, count(s)")
                .unwrap_or_default();
            let hotspots = gq
                .raw_query(
                    "MATCH (s:Symbol) RETURN s.file, count(s) AS cnt ORDER BY cnt DESC LIMIT 10",
                )
                .unwrap_or_default();
            let hubs = gq.raw_query("MATCH ()-[:CALLS]->(s:Symbol) RETURN s.name, s.file, count(*) AS calls ORDER BY calls DESC LIMIT 10").unwrap_or_default();
            let stats = prism.stats().ok();

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

fn api_dead_code(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error": "not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error": e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);
            let rows = gq.raw_query(
                "MATCH (s:Symbol) WHERE s.kind IN ['Function', 'Method'] AND NOT EXISTS { MATCH ()-[:CALLS]->(s) } RETURN s.name, s.kind, s.file ORDER BY s.file"
            ).unwrap_or_default();

            let entry_points = ["main", "__init__", "setUp", "tearDown"];
            let dead: Vec<Value> = rows
                .iter()
                .filter(|r| !entry_points.contains(&r[0].as_str()))
                .map(|r| json!({"name": r[0], "kind": r[1], "file": r[2]}))
                .collect();

            json!({"deadCode": dead, "count": dead.len()})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

fn api_symbols(params: &Value) -> Value {
    let file = params.get("file").and_then(|f| f.as_str()).unwrap_or("");

    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error": "not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error": e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);
            let symbols = gq.symbols_in_file(file).unwrap_or_default();

            let items: Vec<Value> = symbols
                .iter()
                .map(|s| json!({"id": s.id, "name": s.name, "kind": s.kind, "startLine": s.start_line, "endLine": s.end_line}))
                .collect();

            json!({"symbols": items})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

fn api_symbol_context(params: &Value) -> Value {
    let symbol_id = params
        .get("symbol_id")
        .and_then(|s| s.as_str())
        .unwrap_or("");

    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error": "not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error": e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);

            let detail = gq.find_symbol_by_id(symbol_id).ok().flatten();
            let callers = gq.callers_of(symbol_id).unwrap_or_default();
            let callees = gq.callees_of(symbol_id).unwrap_or_default();

            json!({
                "symbol": detail.map(|d| json!({
                    "id": d.id, "name": d.name, "kind": d.kind,
                    "file": d.file, "startLine": d.start_line, "endLine": d.end_line,
                })),
                "callers": callers,
                "callees": callees,
            })
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

fn api_snippet(params: &Value) -> Value {
    let symbol_id = params
        .get("symbol_id")
        .and_then(|s| s.as_str())
        .unwrap_or("");

    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error": "not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error": e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);

            match gq.find_symbol_by_id(symbol_id) {
                Ok(Some(detail)) => {
                    let file_path = prism.root().join(&detail.file);
                    let snippet = infigraph_core::search::read_lines_from_file(
                        &file_path,
                        detail.start_line,
                        detail.end_line,
                    )
                    .unwrap_or_else(|_| "(source not available)".to_string());

                    json!({
                        "symbol": detail.name,
                        "file": detail.file,
                        "startLine": detail.start_line,
                        "endLine": detail.end_line,
                        "code": snippet,
                    })
                }
                _ => json!({"error": format!("symbol '{}' not found", symbol_id)}),
            }
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

fn api_graph_data(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error": "not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error": e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);

            let nodes = gq
                .raw_query("MATCH (s:Symbol) RETURN s.id, s.name, s.kind, s.file")
                .unwrap_or_default();
            let calls = gq
                .raw_query("MATCH (a:Symbol)-[:CALLS]->(b:Symbol) RETURN a.id, b.id")
                .unwrap_or_default();
            let inherits = gq
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

fn api_stats(params: &Value) -> Value {
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

fn api_cluster(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error": "not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error": e.to_string()}),
            };
            match infigraph_core::cluster::detect_clusters(&conn) {
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

fn api_chat(params: &Value) -> Value {
    let message = params.get("message").and_then(|m| m.as_str()).unwrap_or("");
    let msg_lower = message.to_lowercase();

    // Simple intent detection — translate natural language to graph operations
    if msg_lower.contains("dead code") || msg_lower.contains("unused") {
        return api_dead_code(params);
    }
    if msg_lower.contains("architecture")
        || msg_lower.contains("overview")
        || msg_lower.contains("summary")
    {
        return api_architecture(params);
    }
    if msg_lower.contains("cluster")
        || msg_lower.contains("module")
        || msg_lower.contains("community")
    {
        return api_cluster(params);
    }
    if msg_lower.contains("who calls") || msg_lower.contains("callers of") {
        // Extract symbol name after "calls" or "of"
        let name = msg_lower
            .split("calls")
            .last()
            .or_else(|| msg_lower.split("of").last())
            .unwrap_or("")
            .trim();
        if !name.is_empty() {
            let mut p = params.clone();
            p["query"] = Value::String(name.to_string());
            return api_search(&p);
        }
    }
    if msg_lower.contains("stats") || msg_lower.contains("how many") || msg_lower.contains("count")
    {
        return api_stats(params);
    }

    // Default: treat as search query
    let mut p = params.clone();
    p["query"] = Value::String(message.to_string());
    api_search(&p)
}

fn api_routes(params: &Value) -> Value {
    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error": "not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error": e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);
            match infigraph_core::routes::detect_routes(&gq) {
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

fn api_groups(_params: &Value) -> Value {
    let registry_path = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".infigraph").join("registry.json"))
        .unwrap_or_default();
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

fn api_contracts(params: &Value) -> Value {
    let group_name = params.get("group").and_then(|g| g.as_str()).unwrap_or("");
    let registry_path = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".infigraph").join("registry.json"))
        .unwrap_or_default();
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

fn api_complexity(params: &Value) -> Value {
    let threshold = params
        .get("threshold")
        .and_then(|v| v.as_u64())
        .unwrap_or(5) as i64;
    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error":"not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error":e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);
            let rows = gq.raw_query(
                "MATCH (s:Symbol) WHERE s.kind IN ['Function','Method','Test'] AND s.complexity IS NOT NULL RETURN s.id, s.name, s.kind, s.file, s.start_line, s.complexity ORDER BY s.complexity DESC"
            ).unwrap_or_default();

            let items: Vec<Value> = rows
                .iter()
                .map(|r| {
                    json!({
                        "id": r[0], "name": r[1], "kind": r[2], "file": r[3],
                        "line": r[4].parse::<u32>().unwrap_or(0),
                        "complexity": r[5].parse::<i64>().unwrap_or(1),
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

fn api_security(params: &Value) -> Value {
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

fn api_bridges(params: &Value) -> Value {
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

fn api_clones(params: &Value) -> Value {
    let threshold = params
        .get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.92) as f32;
    match open_prism(params) {
        Ok(prism) => {
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error":"not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error":e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);

            let rows = gq.raw_query(
                "MATCH (s:Symbol) WHERE s.kind IN ['Function','Method'] RETURN s.id, s.name, s.kind, s.file, s.docstring"
            ).unwrap_or_default();

            if rows.len() < 2 {
                return json!({"pairs": [], "total": 0});
            }

            let embedder = embed::best_embedder();
            let docs: Vec<(String, String)> = rows
                .iter()
                .map(|row| {
                    let id = row[0].clone();
                    let text = if row.get(4).is_some_and(|s| !s.is_empty()) {
                        format!("{} {}: {}", row[2], row[1], row[4])
                    } else {
                        format!("{} {}", row[2], row[1])
                    };
                    (id, text)
                })
                .collect();

            let emb_path = prism.root().join(".infigraph").join("embeddings.bin");
            let cached: std::collections::HashMap<String, Vec<f32>> = if emb_path.exists() {
                infigraph_core::embed::load_embeddings_cached(&emb_path)
                    .unwrap_or_default()
                    .into_iter()
                    .collect()
            } else {
                std::collections::HashMap::new()
            };

            let vecs: Vec<(String, String, String, Vec<f32>)> = docs
                .iter()
                .map(|(id, text)| {
                    let emb = cached
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| embedder.embed(text).unwrap_or_default());
                    let row = rows.iter().find(|r| &r[0] == id).unwrap();
                    (id.clone(), row[1].clone(), row[3].clone(), emb)
                })
                .filter(|(_, _, _, e)| !e.is_empty())
                .collect();

            let n = vecs.len();
            let mut pairs: Vec<Value> = Vec::new();
            for i in 0..n {
                for j in (i + 1)..n {
                    if vecs[i].2 == vecs[j].2 {
                        continue;
                    }
                    let sim = infigraph_core::embed::cosine_similarity(&vecs[i].3, &vecs[j].3);
                    if sim >= threshold {
                        pairs.push(json!({
                            "score": sim,
                            "a": {"id": vecs[i].0, "name": vecs[i].1, "file": vecs[i].2},
                            "b": {"id": vecs[j].0, "name": vecs[j].1, "file": vecs[j].2},
                        }));
                    }
                }
            }
            pairs.sort_by(|a, b| {
                b["score"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .partial_cmp(&a["score"].as_f64().unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            pairs.truncate(50);
            json!({"pairs": pairs, "total": pairs.len(), "checked": n})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

fn api_git_summary(params: &Value) -> Value {
    let n = params
        .get("n_commits")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as usize;
    match open_prism(params) {
        Ok(prism) => {
            let root = prism.root().to_path_buf();
            let store = match prism.store() {
                Some(s) => s,
                None => return json!({"error":"not initialized"}),
            };
            let conn = match store.connection() {
                Ok(c) => c,
                Err(e) => return json!({"error":e.to_string()}),
            };
            let gq = GraphQuery::new(&conn);

            let n_arg = format!("-{}", n);
            let log_out = std::process::Command::new("git")
                .args(["log", "--format=%H\x1f%an\x1f%ai\x1f%s", &n_arg])
                .current_dir(&root)
                .output();

            let log_text = match log_out {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
                _ => return json!({"error": "git log failed"}),
            };

            let mut commits: Vec<Value> = Vec::new();
            for line in log_text.lines() {
                let parts: Vec<&str> = line.splitn(4, '\x1f').collect();
                if parts.len() < 4 {
                    continue;
                }
                let hash = parts[0];
                let author = parts[1];
                let date = &parts[2][..10.min(parts[2].len())];
                let subject = parts[3];
                let short = &hash[..8.min(hash.len())];

                let parent_ref = format!("{}^", hash);
                let diff_out = std::process::Command::new("git")
                    .args(["diff", "--unified=0", &parent_ref, hash])
                    .current_dir(&root)
                    .output();

                let hunks = match diff_out {
                    Ok(o) if o.status.success() => {
                        let text = String::from_utf8_lossy(&o.stdout).to_string();
                        parse_web_diff_hunks(&text)
                    }
                    _ => vec![],
                };

                let mut touched: Vec<Value> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for (file, start, end) in &hunks {
                    if let Ok(syms) = gq.symbols_in_range(file, *start, *end) {
                        for s in syms {
                            if seen.insert(s.id.clone()) {
                                touched.push(json!({"id":s.id,"name":s.name,"kind":s.kind,"file":s.file,"line":s.start_line}));
                            }
                        }
                    }
                }

                let files_ref = format!("{}^", hash);
                let files_out = std::process::Command::new("git")
                    .args(["diff", "--name-only", &files_ref, hash])
                    .current_dir(&root)
                    .output();
                let changed_files: Vec<String> = match files_out {
                    Ok(o) => String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(String::from)
                        .collect(),
                    _ => vec![],
                };

                commits.push(json!({
                    "hash": short, "author": author, "date": date,
                    "subject": subject, "files": changed_files, "symbols": touched,
                }));
            }
            json!({"commits": commits})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

fn parse_web_diff_hunks(diff: &str) -> Vec<(String, u32, u32)> {
    let mut hunks = Vec::new();
    let mut current_file = String::new();
    for line in diff.lines() {
        if let Some(stripped) = line.strip_prefix("+++ b/") {
            current_file = stripped.to_string();
        } else if line.starts_with("@@ ") {
            // @@ -old +new,count @@
            if let Some(plus_part) = line.split('+').nth(1) {
                let range = plus_part.split(' ').next().unwrap_or("");
                let (start_str, count_str) = if let Some(comma) = range.find(',') {
                    (&range[..comma], &range[comma + 1..])
                } else {
                    (range, "1")
                };
                let start: u32 = start_str.parse().unwrap_or(1);
                let count: u32 = count_str.parse().unwrap_or(1);
                if !current_file.is_empty() {
                    hunks.push((current_file.clone(), start, start + count));
                }
            }
        }
    }
    hunks
}

// The full HTML UI — embedded as a const string
const INDEX_HTML: &str = include_str!("ui.html");
