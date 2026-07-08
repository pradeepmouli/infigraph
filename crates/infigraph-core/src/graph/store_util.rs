use kuzu::Connection;

/// Escape single quotes and control characters for Kuzu string literals.
pub(crate) fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', " ")
        .replace('\r', "")
        .replace('\t', " ")
}

/// Convert a path to forward-slash form (needed on Windows for Kuzu COPY FROM).
pub(crate) fn fwd_slash_path(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Batch-insert edges via UNWIND in chunks of 500.
pub(crate) fn unwind_edges_from_pairs(
    conn: &Connection,
    pairs: &[(&str, &str)],
    rel_type: &str,
    src_label: &str,
    dst_label: &str,
) {
    const CHUNK: usize = 500;
    for chunk in pairs.chunks(CHUNK) {
        let pair_list: Vec<String> = chunk
            .iter()
            .map(|(a, b)| format!("{{a: '{}', b: '{}'}}", escape(a), escape(b)))
            .collect();
        let _ = conn.query(&format!(
            "UNWIND [{}] AS p MATCH (a:{src_label}), (b:{dst_label}) WHERE a.id = p.a AND b.id = p.b CREATE (a)-[:{rel_type}]->(b)",
            pair_list.join(", ")
        ));
    }
}

/// Classify a file path into a category for search ranking.
/// Returns one of: "impl", "test", "config", "docs".
pub fn classify_file(file: &str) -> &'static str {
    let fl = file.to_ascii_lowercase();
    if fl.ends_with("-lock.yaml")
        || fl.ends_with(".lock")
        || fl.contains("pnpm-lock")
        || fl.contains("package-lock")
        || fl.contains("yarn.lock")
    {
        return "config";
    }
    if fl.ends_with(".md") || fl.contains("/docs/") || fl.contains("/doc/") {
        return "docs";
    }
    if fl.ends_with(".yaml") || fl.ends_with(".yml") || fl.ends_with(".json") {
        if fl.contains("test")
            || fl.contains("eval")
            || fl.contains("golden")
            || fl.contains("dataset")
            || fl.contains("fixture")
        {
            return "test";
        }
        return "config";
    }
    if fl.contains("/test/")
        || fl.contains("/tests/")
        || fl.contains("/__tests__/")
        || fl.contains("/__mocks__/")
        || fl.starts_with("test_")
        || fl.contains("/test_")
        || fl.contains(".test.")
        || fl.contains(".spec.")
        || fl.contains("/e2e/")
        || fl.contains("/fixtures/")
        || fl.contains("/testdata/")
    {
        return "test";
    }
    "impl"
}
