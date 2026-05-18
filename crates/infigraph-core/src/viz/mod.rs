use std::path::Path;

use anyhow::{Context, Result};

use crate::graph::GraphQuery;

/// Node data for the visualization.
struct VizNode {
    id: String,
    label: String,
    kind: String,
    file: String,
    start_line: String,
    end_line: String,
}

/// Edge data for the visualization.
struct VizEdge {
    from: String,
    to: String,
    rel_type: String,
}

/// Generate a self-contained HTML visualization of the code graph and write it to `output_path`.
///
/// The HTML uses vis.js loaded from a CDN. Nodes are colored by kind, edges by relationship type.
/// Features a left sidebar with search, filters, and file tree; a right panel with node details
/// including callers/callees; and professional dark-themed styling.
pub fn generate_html(gq: &GraphQuery, output_path: &Path) -> Result<String> {
    let nodes = query_nodes(gq)?;
    let edges = query_edges(gq)?;

    let nodes_json = build_nodes_json(&nodes);
    let edges_json = build_edges_json(&edges);

    let html = HTML_TEMPLATE
        .replace("/*__NODES_DATA__*/", &nodes_json)
        .replace("/*__EDGES_DATA__*/", &edges_json);

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output_path, html.as_bytes())
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    Ok(output_path.to_string_lossy().to_string())
}

/// Generate a focused subgraph HTML visualization centered on a single symbol.
///
/// Traverses `depth` hops of CALLS/INHERITS/CONTAINS edges in both directions,
/// collecting only the reachable nodes and edges. The root symbol is highlighted.
pub fn generate_symbol_html(
    gq: &GraphQuery,
    symbol_id: &str,
    depth: u32,
    output_path: &Path,
) -> Result<String> {
    let (nodes, edges) = query_symbol_subgraph(gq, symbol_id, depth)?;

    if nodes.is_empty() {
        anyhow::bail!("symbol not found: {symbol_id}");
    }

    let nodes_json = build_nodes_json_with_focus(&nodes, symbol_id);
    let edges_json = build_edges_json(&edges);

    let html = HTML_TEMPLATE
        .replace("/*__NODES_DATA__*/", &nodes_json)
        .replace("/*__EDGES_DATA__*/", &edges_json);

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output_path, html.as_bytes())
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    Ok(output_path.to_string_lossy().to_string())
}

fn query_nodes(gq: &GraphQuery) -> Result<Vec<VizNode>> {
    let rows = gq.raw_query(
        "MATCH (s:Symbol) RETURN s.id, s.name, s.kind, s.file, s.start_line, s.end_line",
    )?;

    let mut nodes = Vec::with_capacity(rows.len());
    for row in &rows {
        if row.len() >= 6 {
            nodes.push(VizNode {
                id: row[0].clone(),
                label: row[1].clone(),
                kind: row[2].clone(),
                file: row[3].clone(),
                start_line: row[4].clone(),
                end_line: row[5].clone(),
            });
        }
    }
    Ok(nodes)
}

fn query_edges(gq: &GraphQuery) -> Result<Vec<VizEdge>> {
    let mut edges = Vec::new();

    // CALLS edges
    let call_rows = gq.raw_query("MATCH (a:Symbol)-[:CALLS]->(b:Symbol) RETURN a.id, b.id")?;
    for row in &call_rows {
        if row.len() >= 2 {
            edges.push(VizEdge {
                from: row[0].clone(),
                to: row[1].clone(),
                rel_type: "CALLS".to_string(),
            });
        }
    }

    // INHERITS edges
    let inherit_rows =
        gq.raw_query("MATCH (a:Symbol)-[:INHERITS]->(b:Symbol) RETURN a.id, b.id")?;
    for row in &inherit_rows {
        if row.len() >= 2 {
            edges.push(VizEdge {
                from: row[0].clone(),
                to: row[1].clone(),
                rel_type: "INHERITS".to_string(),
            });
        }
    }

    // CONTAINS edges (Module -> Symbol)
    let contains_rows =
        gq.raw_query("MATCH (m:Module)-[:CONTAINS]->(s:Symbol) RETURN m.id, s.id")?;
    for row in &contains_rows {
        if row.len() >= 2 {
            edges.push(VizEdge {
                from: row[0].clone(),
                to: row[1].clone(),
                rel_type: "CONTAINS".to_string(),
            });
        }
    }

    Ok(edges)
}

/// BFS from `symbol_id` up to `depth` hops, following CALLS/INHERITS in both directions
/// and CONTAINS outward. Returns only reachable nodes + edges between them.
fn query_symbol_subgraph(
    gq: &GraphQuery,
    symbol_id: &str,
    depth: u32,
) -> Result<(Vec<VizNode>, Vec<VizEdge>)> {
    use std::collections::{HashSet, VecDeque};

    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((symbol_id.to_string(), 0));
    visited.insert(symbol_id.to_string());

    while let Some((id, hop)) = queue.pop_front() {
        if hop >= depth {
            continue;
        }
        let esc = id.replace('\'', "\\'");

        // Outgoing CALLS
        let q = format!("MATCH (a:Symbol)-[:CALLS]->(b:Symbol) WHERE a.id = '{esc}' RETURN b.id");
        if let Ok(rows) = gq.raw_query(&q) {
            for row in &rows {
                if let Some(nid) = row.first() {
                    if visited.insert(nid.clone()) {
                        queue.push_back((nid.clone(), hop + 1));
                    }
                }
            }
        }
        // Incoming CALLS (callers)
        let q = format!("MATCH (a:Symbol)-[:CALLS]->(b:Symbol) WHERE b.id = '{esc}' RETURN a.id");
        if let Ok(rows) = gq.raw_query(&q) {
            for row in &rows {
                if let Some(nid) = row.first() {
                    if visited.insert(nid.clone()) {
                        queue.push_back((nid.clone(), hop + 1));
                    }
                }
            }
        }
        // INHERITS both directions
        let q = format!("MATCH (a:Symbol)-[:INHERITS]->(b:Symbol) WHERE a.id = '{esc}' OR b.id = '{esc}' RETURN a.id, b.id");
        if let Ok(rows) = gq.raw_query(&q) {
            for row in &rows {
                for nid in row {
                    if visited.insert(nid.clone()) {
                        queue.push_back((nid.clone(), hop + 1));
                    }
                }
            }
        }
    }

    // Fetch node details for all visited IDs
    let mut nodes = Vec::new();
    for id in &visited {
        let esc = id.replace('\'', "\\'");
        let q = format!(
            "MATCH (s:Symbol) WHERE s.id = '{esc}' RETURN s.id, s.name, s.kind, s.file, s.start_line, s.end_line"
        );
        if let Ok(rows) = gq.raw_query(&q) {
            for row in &rows {
                if row.len() >= 6 {
                    nodes.push(VizNode {
                        id: row[0].clone(),
                        label: row[1].clone(),
                        kind: row[2].clone(),
                        file: row[3].clone(),
                        start_line: row[4].clone(),
                        end_line: row[5].clone(),
                    });
                }
            }
        }
    }

    // Fetch only edges between visited nodes
    let mut edges = Vec::new();
    let id_set: HashSet<&str> = visited.iter().map(|s| s.as_str()).collect();

    let call_rows = gq.raw_query("MATCH (a:Symbol)-[:CALLS]->(b:Symbol) RETURN a.id, b.id")?;
    for row in &call_rows {
        if row.len() >= 2 && id_set.contains(row[0].as_str()) && id_set.contains(row[1].as_str()) {
            edges.push(VizEdge {
                from: row[0].clone(),
                to: row[1].clone(),
                rel_type: "CALLS".to_string(),
            });
        }
    }
    let inherit_rows =
        gq.raw_query("MATCH (a:Symbol)-[:INHERITS]->(b:Symbol) RETURN a.id, b.id")?;
    for row in &inherit_rows {
        if row.len() >= 2 && id_set.contains(row[0].as_str()) && id_set.contains(row[1].as_str()) {
            edges.push(VizEdge {
                from: row[0].clone(),
                to: row[1].clone(),
                rel_type: "INHERITS".to_string(),
            });
        }
    }

    Ok((nodes, edges))
}

/// Like `build_nodes_json` but marks the focus symbol with a larger size and distinct border.
fn build_nodes_json_with_focus(nodes: &[VizNode], focus_id: &str) -> String {
    let entries: Vec<String> = nodes
        .iter()
        .map(|n| {
            let color = match n.kind.as_str() {
                "Function" => "#4A90D9",
                "Class" => "#27AE60",
                "Method" => "#17A2B8",
                "Test" => "#E67E22",
                "Variable" | "Constant" => "#95A5A6",
                "Struct" | "Interface" | "Trait" => "#27AE60",
                "Enum" => "#16A085",
                "Module" => "#F39C12",
                "Section" => "#8E44AD",
                _ => "#BDC3C7",
            };
            if n.id == focus_id {
                format!(
                    "{{id:\"{}\",label:\"{}\",kind:\"{}\",file:\"{}\",startLine:\"{}\",endLine:\"{}\",color:\"{}\",size:30,borderWidth:4,borderColor:\"#FFD700\"}}",
                    json_escape(&n.id), json_escape(&n.label), json_escape(&n.kind),
                    json_escape(&n.file), json_escape(&n.start_line), json_escape(&n.end_line),
                    color,
                )
            } else {
                format!(
                    r#"{{id:"{}",label:"{}",kind:"{}",file:"{}",startLine:"{}",endLine:"{}",color:"{}"}}"#,
                    json_escape(&n.id), json_escape(&n.label), json_escape(&n.kind),
                    json_escape(&n.file), json_escape(&n.start_line), json_escape(&n.end_line),
                    color,
                )
            }
        })
        .collect();
    entries.join(",\n")
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn build_nodes_json(nodes: &[VizNode]) -> String {
    let entries: Vec<String> = nodes
        .iter()
        .map(|n| {
            let color = match n.kind.as_str() {
                "Function" => "#4A90D9",
                "Class" => "#27AE60",
                "Method" => "#17A2B8",
                "Test" => "#E67E22",
                "Variable" | "Constant" => "#95A5A6",
                "Struct" | "Interface" | "Trait" => "#27AE60",
                "Enum" => "#16A085",
                "Module" => "#F39C12",
                "Section" => "#8E44AD",
                _ => "#BDC3C7",
            };
            format!(
                r#"{{id:"{}",label:"{}",kind:"{}",file:"{}",startLine:"{}",endLine:"{}",color:"{}"}}"#,
                json_escape(&n.id),
                json_escape(&n.label),
                json_escape(&n.kind),
                json_escape(&n.file),
                json_escape(&n.start_line),
                json_escape(&n.end_line),
                color,
            )
        })
        .collect();

    entries.join(",\n")
}

fn build_edges_json(edges: &[VizEdge]) -> String {
    let entries: Vec<String> = edges
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let color = match e.rel_type.as_str() {
                "CALLS" => "#3498DB",
                "INHERITS" => "#E74C3C",
                "CONTAINS" => "#7F8C8D",
                _ => "#95A5A6",
            };
            format!(
                r#"{{id:"e{}",from:"{}",to:"{}",relType:"{}",color:"{}"}}"#,
                i,
                json_escape(&e.from),
                json_escape(&e.to),
                json_escape(&e.rel_type),
                color,
            )
        })
        .collect();

    entries.join(",\n")
}

const HTML_TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Infigraph - Graph Visualization</title>
<script src="https://unpkg.com/vis-network@9.1.6/standalone/umd/vis-network.min.js"></script>
<style>
  :root {
    --bg-primary: #1a1a2e;
    --bg-sidebar: #16213e;
    --bg-card: #0f3460;
    --bg-input: #1a1a3e;
    --accent: #e94560;
    --accent-hover: #ff6b81;
    --text-primary: #e0e0e0;
    --text-secondary: #8892a4;
    --text-muted: #5a6478;
    --border: #2a3a5c;
    --node-function: #4A90D9;
    --node-method: #17A2B8;
    --node-class: #27AE60;
    --node-test: #E67E22;
    --node-variable: #95A5A6;
    --node-section: #8E44AD;
    --node-module: #F39C12;
    --edge-calls: #3498DB;
    --edge-inherits: #E74C3C;
    --edge-contains: #7F8C8D;
    --radius: 6px;
    --sidebar-width: 280px;
    --detail-width: 320px;
  }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", sans-serif;
    background: var(--bg-primary);
    color: var(--text-primary);
    overflow: hidden;
    height: 100vh;
    display: flex;
  }

  /* ── Left Sidebar ── */
  #sidebar {
    width: var(--sidebar-width);
    min-width: var(--sidebar-width);
    background: var(--bg-sidebar);
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    height: 100vh;
    overflow: hidden;
  }
  .sidebar-header {
    padding: 20px 16px 16px;
    border-bottom: 1px solid var(--border);
  }
  .sidebar-header h1 {
    font-size: 18px;
    font-weight: 700;
    color: var(--accent);
    letter-spacing: 0.5px;
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .sidebar-header h1 .logo-icon {
    width: 22px; height: 22px;
    background: var(--accent);
    border-radius: 4px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font-size: 12px;
    color: #fff;
    font-weight: 800;
  }
  .stats-row {
    display: flex;
    gap: 6px;
    margin-top: 10px;
    flex-wrap: wrap;
  }
  .stat-badge {
    background: var(--bg-card);
    padding: 4px 10px;
    border-radius: 12px;
    font-size: 11px;
    color: var(--text-secondary);
    white-space: nowrap;
  }
  .stat-badge strong {
    color: var(--text-primary);
    font-weight: 600;
  }

  /* ── Search ── */
  .sidebar-section {
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
  }
  .sidebar-section-title {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 1.2px;
    color: var(--text-muted);
    margin-bottom: 8px;
    font-weight: 600;
  }
  #search-box {
    width: 100%;
    padding: 8px 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--bg-input);
    color: var(--text-primary);
    font-size: 13px;
    outline: none;
    transition: border-color 0.2s;
  }
  #search-box:focus { border-color: var(--accent); }
  #search-box::placeholder { color: var(--text-muted); }
  #search-count {
    font-size: 11px;
    color: var(--text-muted);
    margin-top: 6px;
  }

  /* ── Filter Checkboxes ── */
  .filter-grid {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
  }
  .filter-chip {
    display: flex;
    align-items: center;
    gap: 5px;
    padding: 3px 8px;
    border-radius: 4px;
    font-size: 11px;
    cursor: pointer;
    user-select: none;
    background: var(--bg-card);
    transition: opacity 0.2s;
  }
  .filter-chip:hover { opacity: 0.85; }
  .filter-chip input[type="checkbox"] { display: none; }
  .filter-chip .chip-dot {
    width: 8px; height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .filter-chip.unchecked { opacity: 0.35; }

  /* ── File Tree ── */
  #file-tree-section {
    flex: 1;
    overflow-y: auto;
    padding: 12px 16px;
    border-bottom: none;
  }
  #file-tree-section::-webkit-scrollbar { width: 4px; }
  #file-tree-section::-webkit-scrollbar-track { background: transparent; }
  #file-tree-section::-webkit-scrollbar-thumb { background: var(--border); border-radius: 2px; }
  .file-item {
    padding: 4px 8px;
    font-size: 12px;
    color: var(--text-secondary);
    cursor: pointer;
    border-radius: 4px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    transition: background 0.15s, color 0.15s;
  }
  .file-item:hover { background: var(--bg-card); color: var(--text-primary); }
  .file-item.active { background: var(--accent); color: #fff; }
  .file-item .file-count {
    float: right;
    font-size: 10px;
    color: var(--text-muted);
    background: var(--bg-primary);
    padding: 1px 6px;
    border-radius: 8px;
    margin-left: 4px;
  }
  .file-item.active .file-count { background: rgba(0,0,0,0.2); color: #fff; }

  /* ── Legend (bottom of sidebar) ── */
  .sidebar-footer {
    padding: 12px 16px;
    border-top: 1px solid var(--border);
    background: var(--bg-sidebar);
  }
  .legend-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 3px;
  }
  .legend-item {
    display: flex;
    align-items: center;
    gap: 5px;
    font-size: 10px;
    color: var(--text-secondary);
  }
  .legend-dot {
    width: 8px; height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .legend-line {
    width: 14px; height: 2px;
    flex-shrink: 0;
    border-radius: 1px;
  }

  /* ── Main Graph Area ── */
  #main {
    flex: 1;
    position: relative;
    display: flex;
    flex-direction: column;
  }
  #graph {
    flex: 1;
    width: 100%;
    height: 100%;
  }

  /* ── Toolbar ── */
  #toolbar {
    position: absolute;
    top: 12px;
    left: 12px;
    display: flex;
    gap: 4px;
    z-index: 5;
  }
  .tool-btn {
    width: 36px; height: 36px;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--bg-sidebar);
    color: var(--text-primary);
    font-size: 16px;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: background 0.15s, border-color 0.15s;
  }
  .tool-btn:hover { background: var(--bg-card); border-color: var(--accent); }
  .tool-btn.active { background: var(--accent); border-color: var(--accent); color: #fff; }
  .tool-btn[title]:hover::after {
    content: attr(title);
    position: absolute;
    top: 42px;
    left: 0;
    background: var(--bg-card);
    color: var(--text-primary);
    padding: 4px 8px;
    border-radius: 4px;
    font-size: 11px;
    white-space: nowrap;
    pointer-events: none;
  }

  /* ── Visible Node Count ── */
  #visible-count {
    position: absolute;
    bottom: 12px;
    left: 12px;
    font-size: 11px;
    color: var(--text-muted);
    background: var(--bg-sidebar);
    padding: 4px 10px;
    border-radius: 12px;
    border: 1px solid var(--border);
    z-index: 5;
  }

  /* ── Right Detail Panel ── */
  #detail-panel {
    position: absolute;
    top: 0;
    right: 0;
    width: var(--detail-width);
    height: 100%;
    background: var(--bg-sidebar);
    border-left: 1px solid var(--border);
    display: none;
    flex-direction: column;
    z-index: 10;
    overflow-y: auto;
  }
  #detail-panel::-webkit-scrollbar { width: 4px; }
  #detail-panel::-webkit-scrollbar-track { background: transparent; }
  #detail-panel::-webkit-scrollbar-thumb { background: var(--border); border-radius: 2px; }
  .detail-header {
    padding: 16px;
    border-bottom: 1px solid var(--border);
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
  }
  .detail-header h2 {
    font-size: 15px;
    font-weight: 600;
    color: var(--text-primary);
    word-break: break-all;
  }
  .detail-close {
    background: none;
    border: none;
    color: var(--text-muted);
    font-size: 20px;
    cursor: pointer;
    padding: 0 4px;
    line-height: 1;
    flex-shrink: 0;
  }
  .detail-close:hover { color: var(--accent); }
  .detail-body { padding: 0; }
  .detail-field {
    padding: 8px 16px;
    border-bottom: 1px solid rgba(42,58,92,0.5);
    display: flex;
    justify-content: space-between;
    align-items: baseline;
  }
  .detail-field-label {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.8px;
    color: var(--text-muted);
    flex-shrink: 0;
  }
  .detail-field-value {
    font-size: 13px;
    color: var(--text-primary);
    text-align: right;
    word-break: break-all;
  }
  .detail-kind-badge {
    display: inline-block;
    padding: 2px 8px;
    border-radius: 10px;
    font-size: 11px;
    font-weight: 600;
    color: #fff;
  }
  .detail-section {
    padding: 12px 16px;
    border-bottom: 1px solid rgba(42,58,92,0.5);
  }
  .detail-section-title {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 1.2px;
    color: var(--text-muted);
    margin-bottom: 6px;
    font-weight: 600;
  }
  .detail-list {
    list-style: none;
  }
  .detail-list li {
    padding: 4px 8px;
    font-size: 12px;
    color: var(--text-secondary);
    cursor: pointer;
    border-radius: 4px;
    transition: background 0.15s;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .detail-list li:hover { background: var(--bg-card); color: var(--accent); }
  .detail-list .empty-msg {
    color: var(--text-muted);
    font-style: italic;
    cursor: default;
    font-size: 11px;
  }
  .detail-list .empty-msg:hover { background: transparent; color: var(--text-muted); }
</style>
</head>
<body>

<!-- Left Sidebar -->
<div id="sidebar">
  <div class="sidebar-header">
    <h1><span class="logo-icon">C</span> Infigraph</h1>
    <div class="stats-row">
      <span class="stat-badge"><strong id="stat-nodes">0</strong> nodes</span>
      <span class="stat-badge"><strong id="stat-edges">0</strong> edges</span>
      <span class="stat-badge"><strong id="stat-files">0</strong> files</span>
    </div>
  </div>

  <!-- Search -->
  <div class="sidebar-section">
    <div class="sidebar-section-title">Search</div>
    <input id="search-box" type="text" placeholder="Filter nodes by name, kind, file...">
    <div id="search-count"></div>
  </div>

  <!-- Node Kind Filters -->
  <div class="sidebar-section">
    <div class="sidebar-section-title">Symbol Kinds</div>
    <div class="filter-grid" id="kind-filters"></div>
  </div>

  <!-- Edge Type Filters -->
  <div class="sidebar-section">
    <div class="sidebar-section-title">Edge Types</div>
    <div class="filter-grid" id="edge-filters"></div>
  </div>

  <!-- File Tree -->
  <div id="file-tree-section">
    <div class="sidebar-section-title">Files</div>
    <div id="file-tree"></div>
  </div>

  <!-- Legend -->
  <div class="sidebar-footer">
    <div class="sidebar-section-title">Legend</div>
    <div class="legend-grid">
      <span class="legend-item"><span class="legend-dot" style="background:var(--node-function)"></span>Function</span>
      <span class="legend-item"><span class="legend-dot" style="background:var(--node-method)"></span>Method</span>
      <span class="legend-item"><span class="legend-dot" style="background:var(--node-class)"></span>Class/Struct</span>
      <span class="legend-item"><span class="legend-dot" style="background:var(--node-test)"></span>Test</span>
      <span class="legend-item"><span class="legend-dot" style="background:var(--node-variable)"></span>Variable</span>
      <span class="legend-item"><span class="legend-dot" style="background:var(--node-section)"></span>Section</span>
      <span class="legend-item"><span class="legend-dot" style="background:var(--node-module)"></span>Module</span>
      <span class="legend-item"><span class="legend-line" style="background:var(--edge-calls)"></span>Calls</span>
      <span class="legend-item"><span class="legend-line" style="background:var(--edge-inherits)"></span>Inherits</span>
      <span class="legend-item"><span class="legend-line" style="background:var(--edge-contains);border-top:1px dashed var(--edge-contains);background:transparent"></span>Contains</span>
    </div>
  </div>
</div>

<!-- Main Area -->
<div id="main">
  <div id="graph"></div>

  <!-- Toolbar -->
  <div id="toolbar">
    <button class="tool-btn" id="btn-zoom-in" title="Zoom In">+</button>
    <button class="tool-btn" id="btn-zoom-out" title="Zoom Out">&minus;</button>
    <button class="tool-btn" id="btn-fit" title="Fit to Screen">&#x26F6;</button>
    <button class="tool-btn" id="btn-physics" title="Toggle Physics">&#x2725;</button>
  </div>

  <div id="visible-count"></div>

  <!-- Right Detail Panel -->
  <div id="detail-panel">
    <div class="detail-header">
      <h2 id="detail-name">—</h2>
      <button class="detail-close" id="detail-close">&times;</button>
    </div>
    <div class="detail-body">
      <div class="detail-field">
        <span class="detail-field-label">Kind</span>
        <span class="detail-field-value" id="detail-kind"></span>
      </div>
      <div class="detail-field">
        <span class="detail-field-label">File</span>
        <span class="detail-field-value" id="detail-file"></span>
      </div>
      <div class="detail-field">
        <span class="detail-field-label">Lines</span>
        <span class="detail-field-value" id="detail-lines"></span>
      </div>
      <div class="detail-field">
        <span class="detail-field-label">ID</span>
        <span class="detail-field-value" id="detail-id" style="font-size:11px"></span>
      </div>
      <div class="detail-section">
        <div class="detail-section-title">Callers (<span id="callers-count">0</span>)</div>
        <ul class="detail-list" id="callers-list"></ul>
      </div>
      <div class="detail-section">
        <div class="detail-section-title">Callees (<span id="callees-count">0</span>)</div>
        <ul class="detail-list" id="callees-list"></ul>
      </div>
    </div>
  </div>
</div>

<script>
(function() {
  "use strict";

  /* ── Raw Data (injected by Rust) ── */
  var nodesData = [
    /*__NODES_DATA__*/
  ];
  var edgesData = [
    /*__EDGES_DATA__*/
  ];

  /* ── Precompute connection counts for node sizing ── */
  var connCount = {};
  nodesData.forEach(function(n) { connCount[n.id] = 0; });
  edgesData.forEach(function(e) {
    if (connCount[e.from] !== undefined) connCount[e.from]++;
    if (connCount[e.to] !== undefined) connCount[e.to]++;
  });
  var maxConn = 1;
  for (var k in connCount) { if (connCount[k] > maxConn) maxConn = connCount[k]; }

  function nodeSize(id) {
    var c = connCount[id] || 0;
    return 6 + Math.round(14 * Math.sqrt(c / maxConn));
  }

  /* ── Collect unique kinds, files ── */
  var kindSet = {};
  var fileMap = {};  // file -> count
  nodesData.forEach(function(n) {
    kindSet[n.kind] = true;
    if (n.file) {
      fileMap[n.file] = (fileMap[n.file] || 0) + 1;
    }
  });
  var kinds = Object.keys(kindSet).sort();
  var files = Object.keys(fileMap).sort();

  /* ── Stats ── */
  document.getElementById("stat-nodes").textContent = nodesData.length;
  document.getElementById("stat-edges").textContent = edgesData.length;
  document.getElementById("stat-files").textContent = files.length;

  /* ── Kind color lookup ── */
  function kindColor(kind) {
    switch(kind) {
      case "Function": return "#4A90D9";
      case "Method": return "#17A2B8";
      case "Class": case "Struct": case "Interface": case "Trait": return "#27AE60";
      case "Test": return "#E67E22";
      case "Variable": case "Constant": return "#95A5A6";
      case "Section": return "#8E44AD";
      case "Module": return "#F39C12";
      case "Enum": return "#16A085";
      default: return "#BDC3C7";
    }
  }

  /* ── Build Kind Filter Chips ── */
  var activeKinds = {};
  kinds.forEach(function(k) { activeKinds[k] = true; });
  var kindFiltersEl = document.getElementById("kind-filters");

  kinds.forEach(function(k) {
    var chip = document.createElement("label");
    chip.className = "filter-chip";
    var cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = true;
    cb.addEventListener("change", function() {
      activeKinds[k] = cb.checked;
      chip.classList.toggle("unchecked", !cb.checked);
      applyFilters();
    });
    var dot = document.createElement("span");
    dot.className = "chip-dot";
    dot.style.background = kindColor(k);
    chip.appendChild(cb);
    chip.appendChild(dot);
    chip.appendChild(document.createTextNode(k));
    kindFiltersEl.appendChild(chip);
  });

  /* ── Build Edge Filter Chips ── */
  var edgeTypes = ["CALLS", "INHERITS", "CONTAINS"];
  var edgeColors = { CALLS: "#3498DB", INHERITS: "#E74C3C", CONTAINS: "#7F8C8D" };
  var activeEdgeTypes = { CALLS: true, INHERITS: true, CONTAINS: true };
  var edgeFiltersEl = document.getElementById("edge-filters");

  edgeTypes.forEach(function(t) {
    var chip = document.createElement("label");
    chip.className = "filter-chip";
    var cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = true;
    cb.addEventListener("change", function() {
      activeEdgeTypes[t] = cb.checked;
      chip.classList.toggle("unchecked", !cb.checked);
      applyFilters();
    });
    var dot = document.createElement("span");
    dot.className = "chip-dot";
    dot.style.background = edgeColors[t];
    chip.appendChild(cb);
    chip.appendChild(dot);
    chip.appendChild(document.createTextNode(t));
    edgeFiltersEl.appendChild(chip);
  });

  /* ── Build File Tree ── */
  var fileTreeEl = document.getElementById("file-tree");
  var activeFile = null;

  files.forEach(function(f) {
    var div = document.createElement("div");
    div.className = "file-item";
    var shortName = f.length > 35 ? "..." + f.slice(-32) : f;
    div.textContent = shortName;
    div.title = f;
    var badge = document.createElement("span");
    badge.className = "file-count";
    badge.textContent = fileMap[f];
    div.appendChild(badge);
    div.addEventListener("click", function() {
      if (activeFile === f) {
        activeFile = null;
        div.classList.remove("active");
        applyFilters();
      } else {
        var prev = fileTreeEl.querySelector(".active");
        if (prev) prev.classList.remove("active");
        activeFile = f;
        div.classList.add("active");
        applyFilters();
      }
    });
    fileTreeEl.appendChild(div);
  });

  /* ── Build vis.js DataSets ── */
  var nodes = new vis.DataSet(nodesData.map(function(n) {
    var c = n.color;
    return {
      id: n.id,
      label: n.label,
      color: { background: c, border: c, highlight: { background: "#e94560", border: "#e94560" }, hover: { background: c, border: "#e94560" } },
      font: { color: "#e0e0e0", size: 12, face: "-apple-system, BlinkMacSystemFont, sans-serif" },
      kind: n.kind,
      file: n.file,
      startLine: n.startLine,
      endLine: n.endLine,
      shape: "dot",
      size: nodeSize(n.id),
      borderWidth: 1.5,
      _origColor: c
    };
  }));

  var edges = new vis.DataSet(edgesData.map(function(e) {
    var dashes = e.relType === "CONTAINS";
    return {
      id: e.id,
      from: e.from,
      to: e.to,
      color: { color: e.color, highlight: "#e94560", hover: e.color, opacity: 0.7 },
      arrows: { to: { enabled: true, scaleFactor: 0.5 } },
      relType: e.relType,
      dashes: dashes,
      width: dashes ? 0.8 : 1.2,
      smooth: { type: "continuous", roundness: 0.2 },
      _origColor: e.color
    };
  }));

  /* ── Network Options ── */
  var container = document.getElementById("graph");
  var network = new vis.Network(container, { nodes: nodes, edges: edges }, {
    physics: {
      enabled: true,
      solver: "forceAtlas2Based",
      forceAtlas2Based: {
        gravitationalConstant: -40,
        centralGravity: 0.008,
        springLength: 100,
        springConstant: 0.04,
        damping: 0.4
      },
      stabilization: { iterations: 200, fit: true },
      maxVelocity: 50
    },
    interaction: {
      hover: true,
      tooltipDelay: 100,
      zoomView: true,
      dragView: true,
      multiselect: false,
      navigationButtons: false,
      keyboard: false
    },
    layout: {
      improvedLayout: nodesData.length < 400,
      randomSeed: 42
    },
    nodes: {
      borderWidth: 1.5,
      shadow: { enabled: true, color: "rgba(0,0,0,0.3)", size: 6, x: 2, y: 2 }
    },
    edges: {
      shadow: false,
      smooth: { type: "continuous", roundness: 0.2 }
    }
  });

  var physicsEnabled = true;

  /* ── Toolbar Buttons ── */
  document.getElementById("btn-zoom-in").addEventListener("click", function() {
    var scale = network.getScale();
    network.moveTo({ scale: scale * 1.3, animation: { duration: 300, easingFunction: "easeInOutQuad" } });
  });
  document.getElementById("btn-zoom-out").addEventListener("click", function() {
    var scale = network.getScale();
    network.moveTo({ scale: scale / 1.3, animation: { duration: 300, easingFunction: "easeInOutQuad" } });
  });
  document.getElementById("btn-fit").addEventListener("click", function() {
    network.fit({ animation: { duration: 500, easingFunction: "easeInOutQuad" } });
  });
  var physBtn = document.getElementById("btn-physics");
  physBtn.addEventListener("click", function() {
    physicsEnabled = !physicsEnabled;
    network.setOptions({ physics: { enabled: physicsEnabled } });
    physBtn.classList.toggle("active", physicsEnabled);
  });
  physBtn.classList.add("active");

  /* ── Search ── */
  var searchBox = document.getElementById("search-box");
  var searchCountEl = document.getElementById("search-count");

  searchBox.addEventListener("input", function() {
    applyFilters();
  });

  /* ── Filter Logic ── */
  function applyFilters() {
    var query = searchBox.value.toLowerCase().trim();
    var nodeUpdates = [];
    var visibleNodes = 0;

    nodesData.forEach(function(n) {
      var kindOk = activeKinds[n.kind] !== false;
      var fileOk = !activeFile || n.file === activeFile;
      var searchOk = !query || n.label.toLowerCase().indexOf(query) !== -1
                     || n.kind.toLowerCase().indexOf(query) !== -1
                     || n.file.toLowerCase().indexOf(query) !== -1
                     || n.id.toLowerCase().indexOf(query) !== -1;

      var visible = kindOk && fileOk && searchOk;
      if (visible) visibleNodes++;

      if (!visible) {
        nodeUpdates.push({
          id: n.id,
          hidden: true
        });
      } else if (query && searchOk) {
        nodeUpdates.push({
          id: n.id,
          hidden: false,
          color: { background: n.color, border: n.color, highlight: { background: "#e94560", border: "#e94560" }, hover: { background: n.color, border: "#e94560" } },
          font: { color: "#e0e0e0", size: 12 },
          size: nodeSize(n.id)
        });
      } else {
        nodeUpdates.push({
          id: n.id,
          hidden: false,
          color: { background: n.color, border: n.color, highlight: { background: "#e94560", border: "#e94560" }, hover: { background: n.color, border: "#e94560" } },
          font: { color: "#e0e0e0", size: 12 },
          size: nodeSize(n.id)
        });
      }
    });

    nodes.update(nodeUpdates);

    /* Filter edges by type */
    var edgeUpdates = [];
    edgesData.forEach(function(e) {
      var show = activeEdgeTypes[e.relType] !== false;
      edgeUpdates.push({ id: e.id, hidden: !show });
    });
    edges.update(edgeUpdates);

    /* Update counts */
    document.getElementById("visible-count").textContent = visibleNodes + " / " + nodesData.length + " nodes visible";
    if (query) {
      searchCountEl.textContent = visibleNodes + " matching";
    } else {
      searchCountEl.textContent = "";
    }
  }

  /* Initial visible count */
  document.getElementById("visible-count").textContent = nodesData.length + " / " + nodesData.length + " nodes visible";

  /* ── Build caller/callee indexes ── */
  var callersOf = {};  // nodeId -> [callerIds]
  var calleesOf = {};  // nodeId -> [calleeIds]
  nodesData.forEach(function(n) { callersOf[n.id] = []; calleesOf[n.id] = []; });
  edgesData.forEach(function(e) {
    if (e.relType === "CALLS") {
      if (calleesOf[e.from]) calleesOf[e.from].push(e.to);
      if (callersOf[e.to]) callersOf[e.to].push(e.from);
    }
  });

  /* Node label lookup */
  var nodeLabelMap = {};
  nodesData.forEach(function(n) { nodeLabelMap[n.id] = n.label; });

  /* ── Detail Panel ── */
  var detailPanel = document.getElementById("detail-panel");

  document.getElementById("detail-close").addEventListener("click", function() {
    detailPanel.style.display = "none";
    network.unselectAll();
  });

  function showDetail(nodeId) {
    var node = nodes.get(nodeId);
    if (!node) return;

    document.getElementById("detail-name").textContent = node.label;
    var kindBadge = document.createElement("span");
    kindBadge.className = "detail-kind-badge";
    kindBadge.style.background = kindColor(node.kind);
    kindBadge.textContent = node.kind;
    var kindEl = document.getElementById("detail-kind");
    kindEl.innerHTML = "";
    kindEl.appendChild(kindBadge);

    document.getElementById("detail-file").textContent = node.file || "—";
    document.getElementById("detail-lines").textContent = (node.startLine && node.endLine) ? node.startLine + " - " + node.endLine : "—";
    document.getElementById("detail-id").textContent = node.id;

    /* Callers */
    var callersList = document.getElementById("callers-list");
    callersList.innerHTML = "";
    var callers = callersOf[nodeId] || [];
    document.getElementById("callers-count").textContent = callers.length;
    if (callers.length === 0) {
      var li = document.createElement("li");
      li.className = "empty-msg";
      li.textContent = "No callers";
      callersList.appendChild(li);
    } else {
      callers.forEach(function(cid) {
        var li = document.createElement("li");
        li.textContent = nodeLabelMap[cid] || cid;
        li.title = cid;
        li.addEventListener("click", function() { navigateToNode(cid); });
        callersList.appendChild(li);
      });
    }

    /* Callees */
    var calleesList = document.getElementById("callees-list");
    calleesList.innerHTML = "";
    var callees = calleesOf[nodeId] || [];
    document.getElementById("callees-count").textContent = callees.length;
    if (callees.length === 0) {
      var li = document.createElement("li");
      li.className = "empty-msg";
      li.textContent = "No callees";
      calleesList.appendChild(li);
    } else {
      callees.forEach(function(cid) {
        var li = document.createElement("li");
        li.textContent = nodeLabelMap[cid] || cid;
        li.title = cid;
        li.addEventListener("click", function() { navigateToNode(cid); });
        calleesList.appendChild(li);
      });
    }

    detailPanel.style.display = "flex";
  }

  function navigateToNode(nodeId) {
    network.selectNodes([nodeId]);
    network.focus(nodeId, { scale: 1.2, animation: { duration: 400, easingFunction: "easeInOutQuad" } });
    showDetail(nodeId);
  }

  /* ── Network Events ── */
  network.on("click", function(params) {
    if (params.nodes.length > 0) {
      showDetail(params.nodes[0]);
      /* Highlight connected nodes */
      var selId = params.nodes[0];
      var connNodes = network.getConnectedNodes(selId);
      connNodes.push(selId);
      /* Dim non-connected nodes */
      var updates = [];
      nodesData.forEach(function(n) {
        if (connNodes.indexOf(n.id) !== -1) {
          updates.push({
            id: n.id,
            color: { background: n.color, border: n.id === selId ? "#e94560" : n.color, highlight: { background: "#e94560", border: "#e94560" }, hover: { background: n.color, border: "#e94560" } },
            font: { color: "#e0e0e0", size: n.id === selId ? 14 : 12 },
            opacity: 1.0,
            size: n.id === selId ? nodeSize(n.id) + 4 : nodeSize(n.id)
          });
        } else {
          updates.push({
            id: n.id,
            color: { background: "#2a2a3e", border: "#2a2a3e", highlight: { background: "#444", border: "#444" }, hover: { background: "#333", border: "#444" } },
            font: { color: "#444", size: 10 },
            opacity: 0.15,
            size: 4
          });
        }
      });
      nodes.update(updates);
    } else {
      detailPanel.style.display = "none";
      /* Restore all nodes */
      var updates = [];
      nodesData.forEach(function(n) {
        updates.push({
          id: n.id,
          color: { background: n.color, border: n.color, highlight: { background: "#e94560", border: "#e94560" }, hover: { background: n.color, border: "#e94560" } },
          font: { color: "#e0e0e0", size: 12 },
          opacity: 1.0,
          size: nodeSize(n.id)
        });
      });
      nodes.update(updates);
    }
  });

  /* Tooltip on hover */
  network.on("hoverNode", function(params) {
    container.title = params.node;
  });
  network.on("blurNode", function() {
    container.title = "";
  });

  /* Keyboard shortcuts */
  document.addEventListener("keydown", function(e) {
    if (e.key === "Escape") {
      detailPanel.style.display = "none";
      network.unselectAll();
      applyFilters();
    }
    if (e.key === "/" && document.activeElement !== searchBox) {
      e.preventDefault();
      searchBox.focus();
    }
  });
})();
</script>
</body>
</html>
"##;
