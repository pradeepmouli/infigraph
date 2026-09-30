//! #32: the `infigraph-tool-routing` skill ships a reference of every MCP
//! tool, generated from the advertised tool list so it can never describe a
//! tool differently from what the server tells the agent.
//!
//! The renderer reads MCP `Tool` objects in their wire form (`name`, `title`,
//! `description`, `inputSchema`, `annotations`) and nothing specific to how
//! this server builds them. Moving to rmcp changes only [`advertised_tools`]:
//! `serde_json::to_value(router.list_all())` serializes to the same shape.
//!
//! Regenerate after changing a tool:
//! `INFIGRAPH_BLESS=1 cargo test -p infigraph-mcp --test tool_reference`

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::Value;

/// The tools as the server advertises them over `tools/list`.
fn advertised_tools() -> Vec<Value> {
    infigraph_mcp::build_tools_list()
}

/// Every tool belongs to exactly one section. A tool missing here fails the
/// test, so a new tool cannot ship undocumented.
const SECTIONS: &[(&str, &[&str])] = &[
    (
        "Find and read code",
        &[
            "search",
            "search_symbols",
            "search_code",
            "semantic_search",
            "get_symbols_in_file",
            "get_code_snippet",
            "symbol_context",
            "get_doc_context",
            "get_skeleton",
            "list_files",
        ],
    ),
    (
        "Callers, references and change impact",
        &[
            "trace_callers",
            "trace_callees",
            "find_all_references",
            "transitive_impact",
            "detect_changes",
            "semantic_diff",
            "get_type_hierarchy",
            "get_file_deps",
            "get_dependencies",
            "generate_sequence_diagram",
        ],
    ),
    (
        "Architecture, quality and review",
        &[
            "get_architecture",
            "get_stats",
            "get_graph_schema",
            "get_api_surface",
            "get_complexity",
            "get_test_coverage",
            "generate_test_context",
            "detect_dead_code",
            "detect_clusters",
            "detect_clones",
            "detect_cross_cutting",
            "detect_bridges",
            "review",
            "refactor",
            "git_summary",
        ],
    ),
    (
        "Security, routes and data flow",
        &[
            "detect_security_issues",
            "detect_taint_flows",
            "detect_interprocedural_taint",
            "detect_path_traversal",
            "detect_dynamic_urls",
            "detect_reflection",
            "detect_config_bindings",
            "detect_routes",
        ],
    ),
    (
        "Graph queries and export",
        &[
            "query_graph",
            "export_graph",
            "visualize",
            "visualize_symbol",
        ],
    ),
    (
        "Indexing and projects",
        &[
            "index_project",
            "list_projects",
            "delete_project",
            "list_languages",
            "scip_import",
            "index_manifests",
            "ingest_structured",
            "doctor",
        ],
    ),
    (
        "Watching for changes",
        &[
            "watch_project",
            "stop_watch",
            "restart_watch",
            "enable_watch",
            "disable_watch",
            "get_watch_status",
        ],
    ),
    (
        "Documents and Confluence",
        &[
            "index_docs",
            "search_docs",
            "reindex_docs",
            "clean_docs",
            "index_confluence",
            "index_confluence_pages",
            "watch_docs",
            "stop_watch_docs",
            "restart_watch_docs",
            "enable_watch_docs",
            "disable_watch_docs",
        ],
    ),
    (
        "Multi-repo groups",
        &[
            "group_list",
            "group_create",
            "group_add",
            "group_index",
            "group_build",
            "group_sync",
            "group_link",
            "group_link_docs",
            "group_query",
            "group_search",
            "group_search_docs",
            "group_contracts",
            "group_deps",
        ],
    ),
    (
        "Data pipelines",
        &[
            "pipeline_plugins",
            "pipeline_query",
            "pipeline_deps",
            "pipeline_impact",
            "pipeline_compliance",
        ],
    ),
    (
        "Sessions and memory",
        &[
            "get_latest_session",
            "save_session",
            "search_sessions",
            "purge_sessions",
            "memory_context",
            "consolidate_memory",
        ],
    ),
    (
        "Context compression",
        &["compress", "get_compression_stats"],
    ),
];

const HEADER: &str = "<!-- Generated from the MCP tools/list by crates/infigraph-mcp/tests/tool_reference.rs. Do not edit: run `INFIGRAPH_BLESS=1 cargo test -p infigraph-mcp --test tool_reference`. -->";

/// Where `infigraph install` picks the reference up from.
fn reference_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../infigraph-cli/resources/integrations/shared/skills/infigraph-tool-routing/references/tools.md")
}

fn property_type(schema: &Value) -> String {
    match (schema.get("type"), schema.get("enum")) {
        (_, Some(Value::Array(values))) => values
            .iter()
            .map(|v| {
                v.as_str()
                    .map_or_else(|| v.to_string(), |s| format!("`{s}`"))
            })
            .collect::<Vec<_>>()
            .join(" \\| "),
        (Some(Value::String(t)), _) if t == "array" => {
            let item = schema
                .get("items")
                .and_then(|i| i.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("any");
            format!("{item}[]")
        }
        (Some(Value::String(t)), _) => t.clone(),
        _ => "any".to_string(),
    }
}

/// A parameter description that recurs, word for word, across many tools is
/// printed once at the top instead of under every tool.
fn shared_descriptions(tools: &[Value]) -> BTreeMap<String, String> {
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    for tool in tools {
        let props = tool["inputSchema"]["properties"].as_object();
        for (name, schema) in props.into_iter().flatten() {
            if let Some(d) = schema.get("description").and_then(Value::as_str) {
                *seen.entry((name.clone(), d.to_string())).or_default() += 1;
            }
        }
    }
    seen.into_iter()
        .filter(|(_, n)| *n >= 5)
        .map(|((name, d), _)| (name, d))
        .collect()
}

fn render_tool(out: &mut String, tool: &Value, shared: &BTreeMap<String, String>) {
    let name = tool["name"].as_str().unwrap_or_default();
    out.push_str(&format!("### `{name}`\n\n"));
    let annotations = &tool["annotations"];
    let title = tool
        .get("title")
        .or_else(|| annotations.get("title"))
        .and_then(Value::as_str);
    let mut hints = Vec::new();
    for (key, label) in [
        ("readOnlyHint", "read-only"),
        ("destructiveHint", "destructive"),
        ("idempotentHint", "idempotent"),
    ] {
        if annotations.get(key).and_then(Value::as_bool) == Some(true) {
            hints.push(label);
        }
    }
    if title.is_some() || !hints.is_empty() {
        let mut line = title.map(|t| format!("**{t}**")).unwrap_or_default();
        if !hints.is_empty() {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&format!("({})", hints.join(", ")));
        }
        out.push_str(&format!("{line}\n\n"));
    }
    if let Some(d) = tool.get("description").and_then(Value::as_str) {
        out.push_str(&format!("{}\n\n", d.trim()));
    }
    let schema = &tool["inputSchema"];
    let required: BTreeSet<&str> = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let Some(props) = schema["properties"].as_object().filter(|p| !p.is_empty()) else {
        out.push_str("No parameters.\n\n");
        return;
    };
    out.push_str(
        "| Parameter | Type | Required | Default | Description |\n|---|---|---|---|---|\n",
    );
    let mut names: Vec<&String> = props.keys().collect();
    names.sort_by_key(|n| (!required.contains(n.as_str()), n.as_str()));
    for pname in names {
        let p = &props[pname];
        let description = match p.get("description").and_then(Value::as_str) {
            Some(d) if shared.get(pname).map(String::as_str) == Some(d) => {
                "see Common parameters".to_string()
            }
            Some(d) => d.replace('|', "\\|").replace('\n', " "),
            None => String::new(),
        };
        let default = p
            .get("default")
            .map(|v| format!("`{v}`"))
            .unwrap_or_default();
        let req = if required.contains(pname.as_str()) {
            "yes"
        } else {
            ""
        };
        out.push_str(&format!(
            "| `{pname}` | {} | {req} | {default} | {description} |\n",
            property_type(p)
        ));
    }
    out.push('\n');
}

/// The reference, or every way the section table disagrees with the
/// advertised tools.
fn render(tools: &[Value]) -> Result<String, Vec<String>> {
    let by_name: BTreeMap<&str, &Value> = tools
        .iter()
        .filter_map(|t| Some((t["name"].as_str()?, t)))
        .collect();
    let mut problems = Vec::new();
    let mut placed = BTreeSet::new();
    for (section, names) in SECTIONS {
        for name in *names {
            if !by_name.contains_key(name) {
                problems.push(format!(
                    "`{name}` is listed under \"{section}\" but no longer advertised"
                ));
            }
            if !placed.insert(*name) {
                problems.push(format!("`{name}` is listed in more than one section"));
            }
        }
    }
    for name in by_name.keys() {
        if !placed.contains(name) {
            problems.push(format!(
                "`{name}` is advertised but belongs to no section in SECTIONS"
            ));
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }

    let shared = shared_descriptions(tools);
    let mut out = format!(
        "{HEADER}\n\n# Infigraph MCP tools\n\nEvery tool the Infigraph MCP server advertises ({} in all), grouped by task. Read only the section you need.\n\n",
        tools.len()
    );
    for (section, names) in SECTIONS {
        let anchor = section.to_lowercase().replace(' ', "-").replace(',', "");
        out.push_str(&format!(
            "- [{section}](#{anchor}) — {}\n",
            names.join(", ")
        ));
    }
    if !shared.is_empty() {
        out.push_str("\n## Common parameters\n\n");
        for (name, d) in &shared {
            out.push_str(&format!("- `{name}`: {d}\n"));
        }
    }
    for (section, names) in SECTIONS {
        out.push_str(&format!("\n## {section}\n\n"));
        for name in *names {
            render_tool(&mut out, by_name[name], &shared);
        }
    }
    Ok(format!("{}\n", out.trim_end()))
}

#[test]
fn the_skill_reference_matches_the_advertised_tools() {
    let rendered = match render(&advertised_tools()) {
        Ok(r) => r,
        Err(problems) => panic!(
            "SECTIONS in tests/tool_reference.rs is out of date:\n- {}",
            problems.join("\n- ")
        ),
    };
    let path = reference_path();
    if std::env::var_os("INFIGRAPH_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        on_disk == rendered,
        "{} is stale. Run `INFIGRAPH_BLESS=1 cargo test -p infigraph-mcp --test tool_reference` and commit the result.",
        path.display()
    );
}

#[test]
fn a_tool_in_no_section_is_reported_by_name() {
    let mut tools = advertised_tools();
    tools.push(serde_json::json!({"name": "brand_new_tool", "description": "x", "inputSchema": {"type": "object"}}));
    let problems = render(&tools).unwrap_err();
    assert!(
        problems.iter().any(|p| p.contains("brand_new_tool")),
        "{problems:?}"
    );
}

#[test]
fn rmcp_style_annotations_are_rendered() {
    let tool = serde_json::json!({
        "name": "search",
        "title": "Unified search",
        "description": "d",
        "inputSchema": {"type": "object", "properties": {}},
        "annotations": {"readOnlyHint": true}
    });
    let mut out = String::new();
    render_tool(&mut out, &tool, &BTreeMap::new());
    assert!(out.contains("**Unified search** (read-only)"), "{out}");
    assert!(out.contains("No parameters."), "{out}");
}
