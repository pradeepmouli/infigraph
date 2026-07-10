use std::collections::HashSet;

use serde_json::json;

#[test]
fn advertised_tools_match_mcp_tool_names() {
    let tools_json = infigraph_mcp::build_tools_list();
    let advertised: HashSet<String> = tools_json
        .iter()
        .filter_map(|t| t.get("name")?.as_str().map(String::from))
        .collect();

    let canonical: HashSet<&str> = infigraph_mcp::MCP_TOOL_NAMES.iter().copied().collect();

    let missing_from_canonical: Vec<_> = advertised
        .iter()
        .filter(|name| !canonical.contains(name.as_str()))
        .collect();
    let missing_from_advertised: Vec<_> = canonical
        .iter()
        .filter(|name| !advertised.contains(**name))
        .collect();

    assert!(
        missing_from_canonical.is_empty(),
        "Tools in build_tools_list() but not in MCP_TOOL_NAMES: {missing_from_canonical:?}"
    );
    assert!(
        missing_from_advertised.is_empty(),
        "Tools in MCP_TOOL_NAMES but not in build_tools_list(): {missing_from_advertised:?}"
    );
}

#[test]
fn dispatch_handles_all_mcp_tool_names() {
    for tool_name in infigraph_mcp::MCP_TOOL_NAMES {
        let result = infigraph_mcp::dispatch_tool(tool_name, &json!({}));
        if let Err(e) = result {
            let msg = e.to_string();
            assert!(
                !msg.contains("Unknown tool"),
                "dispatch_tool has no handler for '{tool_name}': {msg}"
            );
        }
    }
}

#[test]
fn mcp_tool_names_has_no_duplicates() {
    let mut seen = HashSet::new();
    for name in infigraph_mcp::MCP_TOOL_NAMES {
        assert!(
            seen.insert(name),
            "Duplicate tool name in MCP_TOOL_NAMES: {name}"
        );
    }
}

#[test]
fn every_mcp_tool_has_cli_or_is_mcp_only() {
    let cli_mapped: HashSet<&str> = infigraph_mcp::MCP_TO_CLI_MAP
        .iter()
        .map(|(mcp, _)| *mcp)
        .collect();
    let mcp_only: HashSet<&str> = infigraph_mcp::MCP_ONLY_TOOLS.iter().copied().collect();

    let mut uncovered = Vec::new();
    for tool in infigraph_mcp::MCP_TOOL_NAMES {
        if !cli_mapped.contains(tool) && !mcp_only.contains(tool) {
            uncovered.push(*tool);
        }
    }

    assert!(
        uncovered.is_empty(),
        "MCP tools with no CLI mapping and not in MCP_ONLY_TOOLS: {uncovered:?}. \
         Either add a CLI command (update MCP_TO_CLI_MAP) or mark as MCP-only (update MCP_ONLY_TOOLS)."
    );
}

#[test]
fn mcp_only_tools_are_valid_mcp_tool_names() {
    let all: HashSet<&str> = infigraph_mcp::MCP_TOOL_NAMES.iter().copied().collect();
    for tool in infigraph_mcp::MCP_ONLY_TOOLS {
        assert!(
            all.contains(tool),
            "MCP_ONLY_TOOLS contains '{tool}' which is not in MCP_TOOL_NAMES"
        );
    }
}

#[test]
fn cli_map_entries_are_valid_mcp_tool_names() {
    let all: HashSet<&str> = infigraph_mcp::MCP_TOOL_NAMES.iter().copied().collect();
    for (mcp, cli) in infigraph_mcp::MCP_TO_CLI_MAP {
        assert!(
            all.contains(mcp),
            "MCP_TO_CLI_MAP has '{mcp}' → '{cli}' but '{mcp}' is not in MCP_TOOL_NAMES"
        );
    }
}

#[test]
fn tool_definitions_are_byte_stable() {
    let first = serde_json::to_string(&infigraph_mcp::build_tools_list()).unwrap();
    let second = serde_json::to_string(&infigraph_mcp::build_tools_list()).unwrap();
    assert_eq!(
        first, second,
        "build_tools_list() produces different bytes across calls — this breaks LLM prefix caching"
    );
}

#[test]
fn tool_schema_token_budget() {
    let tools = serde_json::to_string(&infigraph_mcp::build_tools_list()).unwrap();
    let word_count = tools.split_whitespace().count();
    let est_tokens = ((word_count as f64) * 1.4).ceil() as usize;
    assert!(
        est_tokens < 10_000,
        "Tool schemas are {est_tokens} estimated tokens — exceeds 10k budget. Trim descriptions."
    );
}

#[test]
fn no_tool_is_both_cli_mapped_and_mcp_only() {
    let cli_mapped: HashSet<&str> = infigraph_mcp::MCP_TO_CLI_MAP
        .iter()
        .map(|(mcp, _)| *mcp)
        .collect();
    let mcp_only: HashSet<&str> = infigraph_mcp::MCP_ONLY_TOOLS.iter().copied().collect();

    let overlap: Vec<_> = cli_mapped.intersection(&mcp_only).collect();
    assert!(
        overlap.is_empty(),
        "Tools in both MCP_TO_CLI_MAP and MCP_ONLY_TOOLS (pick one): {overlap:?}"
    );
}
