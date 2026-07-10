use infigraph_mcp::{
    compress::compress_tool_output_with_level, dispatch_tool, session_context::CompressionLevel,
};
use serde_json::json;

fn estimate_tokens(text: &str) -> usize {
    ((text.split_whitespace().count() as f64) * 1.4).ceil() as usize
}

struct EvalResult {
    id: &'static str,
    tool: &'static str,
    raw_tokens: usize,
    comp_tokens: usize,
}

impl EvalResult {
    fn savings_pct(&self) -> f64 {
        if self.raw_tokens == 0 {
            return 0.0;
        }
        (1.0 - self.comp_tokens as f64 / self.raw_tokens as f64) * 100.0
    }
}

fn run_task(id: &'static str, tool: &'static str, args: serde_json::Value) -> EvalResult {
    let raw = dispatch_tool(tool, &args).unwrap_or_else(|e| format!("Error: {e}"));
    let compressed = compress_tool_output_with_level(&raw, tool, &args, CompressionLevel::Summary);
    let raw_tokens = estimate_tokens(&raw);
    let comp_tokens = estimate_tokens(&compressed);
    EvalResult {
        id,
        tool,
        raw_tokens,
        comp_tokens,
    }
}

#[test]
fn phase2_compression_eval() {
    let path = env!("CARGO_MANIFEST_DIR");
    let project_root = std::path::Path::new(path)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_str()
        .unwrap();

    let stats = dispatch_tool("get_stats", &json!({"path": project_root}));
    if stats.is_err() || stats.as_ref().unwrap().contains("not initialized") {
        eprintln!("SKIP: project not indexed at {project_root}");
        return;
    }

    let p = project_root;

    let tasks: Vec<EvalResult> = vec![
        run_task(
            "S1",
            "search",
            json!({"path": p, "query": "authentication handler", "limit": 10}),
        ),
        run_task(
            "S2",
            "search",
            json!({"path": p, "query": "route definition detect_routes", "limit": 20}),
        ),
        run_task(
            "S3",
            "search",
            json!({"path": p, "query": "config load toml settings", "limit": 10}),
        ),
        run_task(
            "S4",
            "search",
            json!({"path": p, "query": "test", "file_pattern": "*test*", "limit": 20}),
        ),
        run_task(
            "S5",
            "search",
            json!({"path": p, "query": "error handling Result Err anyhow", "limit": 10}),
        ),
        run_task(
            "E1",
            "get_doc_context",
            json!({"path": p, "symbol_id": "crates/infigraph-mcp/src/lib.rs::dispatch_tool"}),
        ),
        run_task(
            "E3",
            "get_doc_context",
            json!({"path": p, "symbol_id": "crates/infigraph-mcp/src/tools/search.rs::tool_search"}),
        ),
        run_task(
            "E4",
            "get_doc_context",
            json!({"path": p, "symbol_id": "crates/infigraph-mcp/src/tools/graph.rs::tool_get_doc_context"}),
        ),
        run_task(
            "E5",
            "get_doc_context",
            json!({"path": p, "symbol_id": "crates/infigraph-mcp/src/tools/index.rs::tool_index_project"}),
        ),
        run_task(
            "R2",
            "get_doc_context",
            json!({"path": p, "symbol_id": "crates/infigraph-mcp/src/tools/graph.rs::tool_get_stats"}),
        ),
        run_task(
            "E2",
            "find_all_references",
            json!({"path": p, "symbol_id": "crates/infigraph-mcp/src/tools/search.rs::tool_search"}),
        ),
        run_task(
            "R3",
            "trace_callers",
            json!({"path": p, "symbol_id": "crates/infigraph-mcp/src/tools/graph.rs::tool_query_graph"}),
        ),
        run_task(
            "U1",
            "trace_callees",
            json!({"path": p, "symbol_id": "crates/infigraph-mcp/src/lib.rs::dispatch_tool"}),
        ),
        run_task("U4", "get_architecture", json!({"path": p})),
    ];

    println!();
    println!("======================================================================");
    println!("Phase 2 Compression Eval Results");
    println!("======================================================================");
    println!(
        "{:4} {:25} {:>8} {:>8} {:>8}",
        "ID", "Tool", "Raw", "Comp", "Savings"
    );
    println!("{}", "-".repeat(60));

    let mut by_tool: std::collections::HashMap<&str, (usize, usize, usize)> =
        std::collections::HashMap::new();

    for r in &tasks {
        println!(
            "{:4} {:25} {:8} {:8} {:7.1}%",
            r.id,
            r.tool,
            r.raw_tokens,
            r.comp_tokens,
            r.savings_pct()
        );
        let entry = by_tool.entry(r.tool).or_insert((0, 0, 0));
        entry.0 += r.raw_tokens;
        entry.1 += r.comp_tokens;
        entry.2 += 1;
    }

    println!("{}", "-".repeat(60));
    println!("\nPer-tool summary:");
    let mut total_raw = 0usize;
    let mut total_comp = 0usize;
    for (tool, (raw, comp, count)) in &by_tool {
        let savings = if *raw > 0 {
            (1.0 - *comp as f64 / *raw as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "  {:25} n={:2}  raw={:6}  comp={:6}  savings={:.1}%",
            tool, count, raw, comp, savings
        );
        total_raw += raw;
        total_comp += comp;
    }

    let total_savings = if total_raw > 0 {
        (1.0 - total_comp as f64 / total_raw as f64) * 100.0
    } else {
        0.0
    };
    println!("\n  TOTAL: raw={total_raw}  comp={total_comp}  savings={total_savings:.1}%");

    let search_stats = by_tool.get("search").unwrap_or(&(0, 0, 0));
    if search_stats.0 > 0 {
        let search_savings = (1.0 - search_stats.1 as f64 / search_stats.0 as f64) * 100.0;
        assert!(
            search_savings > 20.0,
            "search compression savings {search_savings:.1}% below 20% threshold"
        );
    }

    let doc_stats = by_tool.get("get_doc_context").unwrap_or(&(0, 0, 0));
    if doc_stats.0 > 0 {
        let doc_savings = (1.0 - doc_stats.1 as f64 / doc_stats.0 as f64) * 100.0;
        assert!(
            doc_savings > 30.0,
            "get_doc_context compression savings {doc_savings:.1}% below 30% threshold"
        );
    }
}
