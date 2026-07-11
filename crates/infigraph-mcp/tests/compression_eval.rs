use infigraph_mcp::{
    compress::{classify_content, compress_generic, compress_tool_output_with_level},
    dispatch_tool,
    session_context::{apply_seen_dedup, CompressionLevel},
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

#[test]
fn phase3_dedup_eval() {
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

    // Enable dedup for this test
    std::env::set_var("INFIGRAPH_DEDUP", "1");

    let p = project_root;

    // Phase 3 eval: measure dedup savings by calling same tools twice
    // First call returns compressed output; second call should return "(seen)" placeholder
    struct DedupResult {
        id: &'static str,
        tool: &'static str,
        first_tokens: usize,
        second_tokens: usize,
        second_is_placeholder: bool,
    }

    let test_cases: Vec<(&str, &str, serde_json::Value)> = vec![
        (
            "D1",
            "search",
            json!({"path": p, "query": "dedup eval test symbol", "limit": 10}),
        ),
        (
            "D2",
            "get_doc_context",
            json!({"path": p, "symbol_id": "crates/infigraph-mcp/src/lib.rs::dispatch_tool"}),
        ),
        (
            "D3",
            "find_all_references",
            json!({"path": p, "symbol_id": "crates/infigraph-mcp/src/tools/search.rs::tool_search"}),
        ),
        ("D4", "get_architecture", json!({"path": p})),
    ];

    let mut results: Vec<DedupResult> = Vec::new();

    for (id, tool, args) in &test_cases {
        let raw = dispatch_tool(tool, args).unwrap_or_else(|e| format!("Error: {e}"));
        let compressed =
            compress_tool_output_with_level(&raw, tool, args, CompressionLevel::Summary);

        // First call through dedup
        let first = apply_seen_dedup(&compressed, tool, args);
        let first_tokens = estimate_tokens(&first);

        // Second call with same content — should get placeholder
        let second = apply_seen_dedup(&compressed, tool, args);
        let second_tokens = estimate_tokens(&second);
        let is_placeholder = second.contains("(seen");

        results.push(DedupResult {
            id,
            tool,
            first_tokens,
            second_tokens,
            second_is_placeholder: is_placeholder,
        });
    }

    println!();
    println!("======================================================================");
    println!("Phase 3 Dedup Eval Results");
    println!("======================================================================");
    println!(
        "{:4} {:25} {:>8} {:>8} {:>8} {:>10}",
        "ID", "Tool", "1st", "2nd", "Savings", "Deduped?"
    );
    println!("{}", "-".repeat(70));

    let mut total_first = 0usize;
    let mut total_second = 0usize;
    let mut dedup_count = 0usize;

    for r in &results {
        let savings = if r.first_tokens > 0 {
            (1.0 - r.second_tokens as f64 / r.first_tokens as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "{:4} {:25} {:8} {:8} {:7.1}% {:>10}",
            r.id,
            r.tool,
            r.first_tokens,
            r.second_tokens,
            savings,
            if r.second_is_placeholder { "YES" } else { "no" }
        );
        total_first += r.first_tokens;
        total_second += r.second_tokens;
        if r.second_is_placeholder {
            dedup_count += 1;
        }
    }

    println!("{}", "-".repeat(70));
    let total_savings = if total_first > 0 {
        (1.0 - total_second as f64 / total_first as f64) * 100.0
    } else {
        0.0
    };
    println!(
        "TOTAL: first={total_first}  second={total_second}  savings={total_savings:.1}%  deduped={dedup_count}/{}",
        results.len()
    );

    // Quality gate: dedup should work on outputs large enough to compress
    // Small outputs (<50 tokens) are exempt from dedup
    let dedup_eligible: Vec<&DedupResult> =
        results.iter().filter(|r| r.first_tokens >= 50).collect();
    let dedup_rate = if !dedup_eligible.is_empty() {
        dedup_eligible
            .iter()
            .filter(|r| r.second_is_placeholder)
            .count() as f64
            / dedup_eligible.len() as f64
    } else {
        0.0
    };
    println!("Dedup rate (eligible): {:.0}%", dedup_rate * 100.0);

    assert!(
        dedup_rate >= 0.5,
        "Dedup rate {:.0}% below 50% threshold on eligible outputs",
        dedup_rate * 100.0
    );
}

#[test]
fn phase4_generic_compressor_eval() {
    use infigraph_mcp::compress::ContentType;

    struct GenericResult {
        id: &'static str,
        content_type: ContentType,
        raw_tokens: usize,
        comp_tokens: usize,
    }

    let json_array = format!(
        "[{}]",
        (0..50)
            .map(|i| format!(
                r#"{{"id":{},"name":"user{}","email":"user{}@example.com","score":{}}}"#,
                i,
                i,
                i,
                i * 10
            ))
            .collect::<Vec<_>>()
            .join(",")
    );

    let log_output = (0..100)
        .map(|i| {
            if i == 42 {
                "[ERROR] Connection refused at db:5432".to_string()
            } else if i == 75 {
                "[WARN] Slow query detected (3.2s)".to_string()
            } else {
                format!("[INFO] Processing request {} OK", i)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let stack_trace = "java.lang.NullPointerException: null\n\tat com.app.UserService.getUser(UserService.java:42)\n\tat com.app.Controller.handleRequest(Controller.java:88)\n\tat org.springframework.web.servlet.DispatcherServlet.doDispatch(DispatcherServlet.java:1067)\n\tat org.springframework.web.servlet.DispatcherServlet.doService(DispatcherServlet.java:963)\n\tat org.springframework.web.servlet.FrameworkServlet.processRequest(FrameworkServlet.java:1006)\n\tat javax.servlet.http.HttpServlet.service(HttpServlet.java:750)\n\tat org.apache.catalina.core.ApplicationFilterChain.doFilter(ApplicationFilterChain.java:166)\n\tat org.apache.tomcat.websocket.server.WsFilter.doFilter(WsFilter.java:53)\n\tat com.app.Main.main(Main.java:15)\n".to_string();

    let build_output = (0..30)
        .map(|i| format!("Compiling dep-{} v1.{}.0", i, i))
        .chain(std::iter::once(
            "warning: unused variable `x` in src/lib.rs:23".to_string(),
        ))
        .chain(std::iter::once(
            "error[E0308]: mismatched types in src/main.rs:10".to_string(),
        ))
        .chain(std::iter::once("Finished dev profile".to_string()))
        .collect::<Vec<_>>()
        .join("\n");

    let prose = "Authentication uses JWT tokens for stateless session management. The token contains claims about the user identity and permissions. Sessions are stored server-side in Redis for fast lookup. Cookies carry the session identifier between requests. HTTP headers include authorization bearer tokens. Middleware validates tokens before passing requests to handlers. The routing layer maps URLs to controller functions. Caching reduces database load by storing frequent queries. Logging captures request and response metadata for debugging. Metrics track latency percentiles and error rates. Rate limiting prevents abuse by throttling excessive requests. Circuit breakers protect downstream services from cascading failures. Load balancers distribute traffic across multiple instances. Health checks verify service readiness and liveness. Graceful shutdown drains in-flight requests before terminating. Database migrations run during deployment using a versioned schema approach. Connection pooling minimizes the overhead of establishing new database connections. Query optimization involves analyzing execution plans and adding appropriate indexes.";

    let test_cases: Vec<(&str, String)> = vec![
        ("G1-json", json_array),
        ("G2-log", log_output),
        ("G3-stack", stack_trace),
        ("G4-build", build_output),
        ("G5-prose", prose.to_string()),
    ];

    let mut results: Vec<GenericResult> = Vec::new();

    for (id, content) in &test_cases {
        let content_type = classify_content(content);
        let compressed = compress_generic(content);
        let raw_tokens = estimate_tokens(content);
        let comp_tokens = estimate_tokens(&compressed);

        results.push(GenericResult {
            id,
            content_type,
            raw_tokens,
            comp_tokens,
        });
    }

    println!();
    println!("======================================================================");
    println!("Phase 4 Generic Compressor Eval Results");
    println!("======================================================================");
    println!(
        "{:10} {:15} {:>8} {:>8} {:>8}",
        "ID", "Type", "Raw", "Comp", "Savings"
    );
    println!("{}", "-".repeat(55));

    let mut total_raw = 0usize;
    let mut total_comp = 0usize;

    for r in &results {
        let savings = if r.raw_tokens > 0 {
            (1.0 - r.comp_tokens as f64 / r.raw_tokens as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "{:10} {:15?} {:8} {:8} {:7.1}%",
            r.id, r.content_type, r.raw_tokens, r.comp_tokens, savings
        );
        total_raw += r.raw_tokens;
        total_comp += r.comp_tokens;
    }

    println!("{}", "-".repeat(55));
    let total_savings = if total_raw > 0 {
        (1.0 - total_comp as f64 / total_raw as f64) * 100.0
    } else {
        0.0
    };
    println!("TOTAL: raw={total_raw}  comp={total_comp}  savings={total_savings:.1}%");

    // Quality checks
    // JSON: raw is compact (no whitespace) so word-count underestimates tokens.
    // Check by character length instead — compressed should be shorter than raw.
    let json_compressed_len = compress_generic(&test_cases[0].1).len();
    assert!(
        json_compressed_len < test_cases[0].1.len(),
        "JSON compressor should reduce character length: {} vs {}",
        json_compressed_len,
        test_cases[0].1.len()
    );

    let log_result = &results[1];
    assert!(
        log_result.comp_tokens < log_result.raw_tokens / 2,
        "Log compressor should achieve >50% savings on 100 lines"
    );

    let prose_result = &results[4];
    assert!(
        prose_result.comp_tokens < prose_result.raw_tokens,
        "Prose compressor should reduce tokens"
    );

    assert!(
        total_savings > 30.0,
        "Overall generic compression savings {total_savings:.1}% below 30% threshold"
    );
}

#[test]
fn kompress_integration() {
    use infigraph_mcp::compress::compress_generic;

    let home = std::env::var("HOME").unwrap_or_default();
    let model_path =
        std::path::Path::new(&home).join(".infigraph/models/kompress-small/model.onnx");
    if !model_path.exists() {
        eprintln!("kompress model not downloaded, skipping integration test");
        return;
    }

    std::env::set_var("INFIGRAPH_ML_COMPRESSION", "kompress");

    let long_prose = format!(
        "{} {} {}",
        "Authentication uses JWT tokens for stateless session management. \
        The token contains claims about the user identity and permissions. \
        Sessions are stored server-side in Redis for fast lookup. \
        Cookies carry the session identifier between requests. \
        HTTP headers include authorization bearer tokens. \
        Middleware validates tokens before passing requests to handlers. \
        The routing layer maps URLs to controller functions. \
        Caching reduces database load by storing frequent queries. \
        Logging captures request and response metadata for debugging. \
        Metrics track latency percentiles and error rates.",
        "Rate limiting prevents abuse by throttling excessive requests. \
        Circuit breakers protect downstream services from cascading failures. \
        Load balancers distribute traffic across multiple instances. \
        Health checks verify service readiness and liveness. \
        Graceful shutdown drains in-flight requests before terminating. \
        Database migrations run during deployment using versioned schemas. \
        Connection pooling minimizes overhead of establishing new connections. \
        Query optimization involves analyzing execution plans and indexes. \
        Replication ensures data durability across geographic regions. \
        Backup procedures run nightly with point-in-time recovery capability.",
        "Schema validation prevents malformed data from entering the system. \
        Input sanitization guards against injection attacks and XSS. \
        Output encoding ensures safe rendering in client applications. \
        Content security policies restrict resource loading origins. \
        Cross-origin resource sharing headers control API access patterns. \
        Transport layer security encrypts data in transit between services. \
        Certificate management automates renewal and rotation of TLS certs. \
        Secret management stores credentials in encrypted vaults with audit. \
        Access control lists define fine-grained permissions per resource. \
        Role-based authorization maps users to permission sets via groups.",
    );

    let compressed = compress_generic(&long_prose);
    let raw_tokens = estimate_tokens(&long_prose);
    let comp_tokens = estimate_tokens(&compressed);
    let savings = (1.0 - comp_tokens as f64 / raw_tokens as f64) * 100.0;

    eprintln!(
        "kompress: {} -> {} tokens ({:.1}% savings)",
        raw_tokens, comp_tokens, savings
    );
    eprintln!("output: {}", &compressed[..compressed.len().min(300)]);

    assert!(comp_tokens < raw_tokens, "kompress should compress");
    assert!(
        compressed.contains("Authentication") || compressed.contains("authentication"),
        "should preserve key content"
    );

    std::env::set_var("INFIGRAPH_ML_COMPRESSION", "extractive");
}
