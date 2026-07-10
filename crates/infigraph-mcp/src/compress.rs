use serde_json::Value;

const MIN_TOKENS_TO_COMPRESS: usize = 100;

static BYPASS_TOOLS: &[&str] = &[
    "get_code_snippet",
    "detect_security_issues",
    "detect_taint_flows",
    "detect_interprocedural_taint",
    "detect_path_traversal",
];

pub fn compress_tool_output(raw: &str, tool_name: &str, args: &Value) -> String {
    if should_bypass(tool_name, args, raw) {
        return raw.to_string();
    }
    match tool_name {
        "search" => compress_search(raw, args),
        "get_doc_context" => compress_doc_context(raw, args),
        "find_all_references" => compress_references(raw, args),
        "get_architecture" => compress_architecture(raw, args),
        _ => raw.to_string(),
    }
}

fn should_bypass(tool_name: &str, args: &Value, raw: &str) -> bool {
    if BYPASS_TOOLS.contains(&tool_name) {
        return true;
    }
    if args
        .get("detail")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    if args
        .get("for_edit")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    let word_count = raw.split_whitespace().count();
    let est_tokens = ((word_count as f64) * 1.4).ceil() as usize;
    if est_tokens < MIN_TOKENS_TO_COMPRESS {
        return true;
    }
    if raw.starts_with("Error:") || raw.starts_with("No ") {
        return true;
    }
    false
}

fn compress_search(raw: &str, _args: &Value) -> String {
    // Parse header: "Search: 'query' (N symbol results, M text matches)"
    let mut lines = raw.lines().peekable();
    let header = match lines.next() {
        Some(h) if h.starts_with("Search:") => h,
        _ => return raw.to_string(),
    };

    // Skip blank line after header
    if lines.peek().is_some_and(|l| l.is_empty()) {
        lines.next();
    }

    let mut symbol_lines: Vec<String> = Vec::new();
    let mut text_section = String::new();
    let mut doc_section = String::new();
    let mut watcher_warning = String::new();
    let mut in_text = false;
    let mut in_docs = false;

    for line in lines {
        if line == "---" {
            if in_text || in_docs {
                // second/third separator
            }
            in_text = false;
            in_docs = false;
            continue;
        }
        if line == "Text matches:" {
            in_text = true;
            continue;
        }
        if line == "Document matches:" {
            in_text = false;
            in_docs = true;
            continue;
        }
        if line.starts_with("✓ Auto-started") || line.starts_with("⚠ No file watcher") {
            watcher_warning = format!("\n{line}");
            continue;
        }

        if in_text {
            text_section.push_str(line);
            text_section.push('\n');
        } else if in_docs {
            doc_section.push_str(line);
            doc_section.push('\n');
        } else {
            // Symbol result block: score line, optional docstring, optional grep
            // Score lines start with a digit (e.g. "0.950  Function ...")
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with("grep:") || trimmed.starts_with('"') {
                // Skip docstring previews and grep context in summary mode
                continue;
            }
            // This is a score line — keep it as-is (already compact)
            symbol_lines.push(line.to_string());
        }
    }

    let mut out = String::with_capacity(raw.len() / 2);
    out.push_str(header);
    out.push('\n');

    for sl in &symbol_lines {
        out.push_str(sl);
        out.push('\n');
    }

    if !text_section.is_empty() {
        out.push_str("\n---\nText matches:\n");
        out.push_str(&text_section);
    }

    if !doc_section.is_empty() {
        out.push_str("\n---\nDocument matches:\n");
        // Compress doc matches: keep only [file] heading (score) lines, drop snippets
        for line in doc_section.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                // e.g. "  [docs/PLAN.md] Task 2.4 (score: 0.84)"
                out.push_str(line);
                out.push('\n');
            }
            // Skip snippet lines (indented text content)
        }
    }

    if !watcher_warning.is_empty() {
        out.push_str(&watcher_warning);
    }

    out.push_str("\nUse search with detail=true for full source snippets and doc excerpts.");
    out
}

fn compress_doc_context(raw: &str, _args: &Value) -> String {
    // Format: === Kind name ===\nFile: ...\nDoc: ...\nComplexity: ...\n\nSource:\n```\n...\n```\n\nCallers (N):\n...\n\nCallees (N):\n...
    // Summary: drop Source block, keep signature line from source if available
    if !raw.starts_with("=== ") {
        return raw.to_string();
    }

    let mut out = String::with_capacity(raw.len() / 3);
    let mut in_source = false;
    let mut source_first_line: Option<String> = None;
    let mut backtick_count = 0;

    for line in raw.lines() {
        if line == "Source:" {
            in_source = true;
            backtick_count = 0;
            continue;
        }
        if in_source {
            if line == "```" {
                backtick_count += 1;
                if backtick_count >= 2 {
                    in_source = false;
                    if let Some(sig) = &source_first_line {
                        out.push_str(&format!("Signature: {}\n", sig.trim()));
                    }
                    out.push_str("(source omitted — use get_doc_context with detail=true or get_code_snippet)\n");
                }
                continue;
            }
            if backtick_count == 1 && source_first_line.is_none() {
                // First line of source — extract signature (strip line number prefix)
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    // Strip leading line number: "  23  pub fn login(...)"
                    let sig = if let Some(pos) = trimmed.find("  ") {
                        let after = trimmed[pos..].trim();
                        if after.is_empty() {
                            trimmed
                        } else {
                            after
                        }
                    } else {
                        trimmed
                    };
                    source_first_line = Some(sig.to_string());
                }
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    out
}

fn compress_references(raw: &str, _args: &Value) -> String {
    // Format: "References to 'X' (N total):\n\n  file:line — in func\n..."
    // Summary: group by file, show count per file instead of listing every line
    if !raw.starts_with("References to ") {
        return raw.to_string();
    }

    let mut lines = raw.lines();
    let header = lines.next().unwrap();

    // Skip blank line
    lines.next();

    // Group references by file
    let mut by_file: Vec<(&str, Vec<(&str, &str)>)> = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // "file:line — in func"
        if let Some(dash_pos) = trimmed.find(" \u{2014} in ") {
            let loc = &trimmed[..dash_pos];
            let separator = " \u{2014} in ";
            let func = &trimmed[dash_pos + separator.len()..];
            let file = loc.rsplit_once(':').map(|(f, _)| f).unwrap_or(loc);
            if by_file.last().is_none_or(|(f, _)| *f != file) {
                by_file.push((file, Vec::new()));
            }
            by_file.last_mut().unwrap().1.push((loc, func));
        }
    }

    let mut out = String::with_capacity(raw.len() / 2);
    out.push_str(header);
    out.push('\n');
    for (file, refs) in &by_file {
        if refs.len() == 1 {
            out.push_str(&format!("  {} — in {}\n", refs[0].0, refs[0].1));
        } else {
            // Deduplicate function names
            let mut funcs: Vec<&str> = refs.iter().map(|(_, f)| *f).collect();
            funcs.dedup();
            let lines_str: Vec<&str> = refs
                .iter()
                .map(|(loc, _)| loc.rsplit_once(':').map(|(_, l)| l).unwrap_or("?"))
                .collect();
            out.push_str(&format!(
                "  {} ({}x): L{} — {}\n",
                file,
                refs.len(),
                lines_str.join(","),
                funcs.join(", ")
            ));
        }
    }
    out.push_str("\nUse find_all_references with detail=true for calling context.");
    out
}

fn compress_architecture(raw: &str, _args: &Value) -> String {
    // Summary: keep language breakdown (top 5), symbols by kind, hotspots (top 5),
    // hubs (top 5), truncate entry points to count only
    if !raw.contains("=== Language Breakdown ===") {
        return raw.to_string();
    }

    let mut out = String::with_capacity(raw.len() / 2);
    let mut section = "";
    let mut section_count = 0;
    let mut entry_point_count = 0;
    let mut in_entry_points = false;

    for line in raw.lines() {
        if line.starts_with("=== ") {
            if in_entry_points && entry_point_count > 0 {
                out.push_str(&format!(
                    "  ... and {} total entry points\n",
                    entry_point_count
                ));
            }
            in_entry_points = line.contains("Entry Points");
            section = if line.contains("Language") {
                "lang"
            } else if line.contains("Symbols") {
                "kind"
            } else if line.contains("Hotspot") {
                "hotspot"
            } else if line.contains("Hub") {
                "hub"
            } else {
                "other"
            };
            section_count = 0;
            entry_point_count = 0;
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if in_entry_points {
            if !line.trim().is_empty() {
                entry_point_count += 1;
            }
            continue;
        }

        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }

        section_count += 1;
        let limit = match section {
            "lang" => 5,
            "kind" => 99, // keep all kinds — small
            "hotspot" => 5,
            "hub" => 5,
            _ => 99,
        };

        if section_count <= limit {
            out.push_str(line);
            out.push('\n');
        } else if section_count == limit + 1 {
            out.push_str("  ... (truncated)\n");
        }
    }

    if in_entry_points && entry_point_count > 0 {
        out.push_str(&format!(
            "  {} entry points (use get_architecture with detail=true to list)\n",
            entry_point_count
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_bypass_small_output() {
        assert!(should_bypass("search", &json!({}), "No results"));
    }

    #[test]
    fn test_bypass_detail_true() {
        assert!(should_bypass(
            "search",
            &json!({"detail": true}),
            "x ".repeat(200).as_str()
        ));
    }

    #[test]
    fn test_bypass_security_tool() {
        assert!(should_bypass(
            "detect_security_issues",
            &json!({}),
            "x ".repeat(200).as_str()
        ));
    }

    #[test]
    fn test_compress_search_strips_docstrings_and_grep() {
        let raw = r#"Search: 'auth login' (3 symbol results, 1 text matches)

0.950  Function login (crates/auth/src/lib.rs:L23-45)
       "Authenticate a user with username and password"
       grep: crates/auth/src/lib.rs:23: pub fn login(username: &str) {
0.870  Function verify_token (crates/auth/src/lib.rs:L47-55)
0.820  Test test_login (crates/auth/tests/auth_test.rs:L10-30)

---
Text matches:
crates/auth/src/lib.rs:23: pub fn login(username: &str) {

---
Document matches:
  [docs/AUTH.md] Authentication flow (score: 0.84)
    The login flow starts with...
  [docs/API.md] POST /login (score: 0.72)
    Handles user authentication

⚠ No file watcher running — results may be stale. Run `infigraph watch` or re-index to refresh."#;

        let compressed = compress_search(raw, &json!({}));

        // Should keep score lines
        assert!(compressed.contains("0.950  Function login"));
        assert!(compressed.contains("0.870  Function verify_token"));
        assert!(compressed.contains("0.820  Test test_login"));
        // Should strip docstrings and grep
        assert!(!compressed.contains("Authenticate a user"));
        assert!(!compressed.contains("grep:"));
        // Should keep text matches
        assert!(compressed.contains("Text matches:"));
        // Should keep doc file references but strip snippets
        assert!(compressed.contains("[docs/AUTH.md]"));
        assert!(!compressed.contains("The login flow starts"));
        // Should have detail hint
        assert!(compressed.contains("detail=true"));
        // Should preserve watcher warning
        assert!(compressed.contains("⚠ No file watcher"));
    }

    #[test]
    fn test_compress_doc_context_strips_source() {
        let raw = r#"=== Function login ===
File:  crates/auth/src/lib.rs:23-45
Doc:   Authenticate a user
Complexity: 8

Source:
```
  23  pub fn login(username: &str, password: &str) -> Result<Token> {
  24      let user = find_user(username)?;
  25      verify_password(user, password)?;
  26      create_token(user)
  27  }
```

Callers (3):
  crates/routes/auth.rs::login_handler
  crates/tests/auth_test.rs::test_login
  crates/tests/auth_test.rs::test_login_fail

Callees (3):
  crates/auth/src/lib.rs::find_user
  crates/auth/src/lib.rs::verify_password
  crates/auth/src/lib.rs::create_token
"#;

        let compressed = compress_doc_context(raw, &json!({}));

        // Should keep header, doc, complexity
        assert!(compressed.contains("=== Function login ==="));
        assert!(compressed.contains("File:  crates/auth/src/lib.rs:23-45"));
        assert!(compressed.contains("Complexity: 8"));
        // Should extract signature
        assert!(
            compressed.contains("pub fn login(username: &str, password: &str) -> Result<Token>")
        );
        // Should strip source body
        assert!(!compressed.contains("find_user(username)"));
        assert!(!compressed.contains("verify_password(user"));
        assert!(!compressed.contains("create_token(user)"));
        // Should keep callers/callees
        assert!(compressed.contains("Callers (3):"));
        assert!(compressed.contains("login_handler"));
        assert!(compressed.contains("Callees (3):"));
        assert!(compressed.contains("find_user"));
        // Should have detail hint
        assert!(compressed.contains("detail=true"));
    }

    #[test]
    fn test_compress_doc_context_passthrough_on_bad_format() {
        let raw = "not a doc context output";
        assert_eq!(compress_doc_context(raw, &json!({})), raw);
    }

    #[test]
    fn test_compress_search_passthrough_on_bad_format() {
        let raw = "something unexpected";
        assert_eq!(compress_search(raw, &json!({})), raw);
    }

    #[test]
    fn test_no_compression_on_error() {
        let raw = "Error: missing 'query'";
        let result = compress_tool_output(raw, "search", &json!({}));
        assert_eq!(result, raw);
    }

    #[test]
    fn test_compress_references_groups_by_file() {
        let raw = "References to 'src/auth.rs::login' (5 total):\n\n  src/routes/auth.rs:12 — in login_handler\n  src/routes/auth.rs:34 — in logout_handler\n  src/tests/auth_test.rs:10 — in test_login\n  src/tests/auth_test.rs:25 — in test_login_fail\n  src/tests/auth_test.rs:40 — in test_login_expired\n";

        let compressed = compress_references(raw, &json!({}));

        // Header preserved
        assert!(compressed.contains("References to 'src/auth.rs::login' (5 total):"));
        // Grouped by file with count
        assert!(compressed.contains("src/routes/auth.rs (2x)"));
        assert!(compressed.contains("src/tests/auth_test.rs (3x)"));
        // Detail hint
        assert!(compressed.contains("detail=true"));
    }

    #[test]
    fn test_compress_references_single_ref_per_file() {
        let raw = "References to 'lib.rs::foo' (2 total):\n\n  src/a.rs:10 — in bar\n  src/b.rs:20 — in baz\n";

        let compressed = compress_references(raw, &json!({}));

        // Single refs kept as-is (no grouping needed)
        assert!(compressed.contains("src/a.rs:10 — in bar"));
        assert!(compressed.contains("src/b.rs:20 — in baz"));
    }

    #[test]
    fn test_compress_references_passthrough_on_bad_format() {
        let raw = "not a references output";
        assert_eq!(compress_references(raw, &json!({})), raw);
    }

    #[test]
    fn test_compress_architecture_truncates_sections() {
        let raw = "\
=== Language Breakdown ===
                  rust: 201 files
              markdown: 24 files
                  toml: 16 files
                  json: 10 files
                python: 8 files
                  bash: 6 files
            typescript: 4 files

=== Symbols by Kind ===
              Function: 1146
                  Test: 950

=== Hotspot Files (most symbols) ===
   1. src/a.rs       220 symbols
   2. src/b.rs       85 symbols
   3. src/c.rs       83 symbols
   4. src/d.rs       77 symbols
   5. src/e.rs       72 symbols
   6. src/f.rs       71 symbols
   7. src/g.rs       67 symbols

=== Hub Functions (most callers) ===
   1. iter       src/lib.rs   834 callers
   2. push_str   src/sync.rs  514 callers
   3. split      src/ext.rs   129 callers
   4. next       src/lib.rs   120 callers
   5. lock       src/js.rs    101 callers
   6. bundled    src/lang.rs   84 callers

=== Entry Points (call others, never called) ===
  Function main    src/bin/a.rs
  Function main    src/bin/b.rs
  Function main    src/bin/c.rs
  Function setup   src/test.rs
";

        let compressed = compress_architecture(raw, &json!({}));

        // Languages: top 5 kept, rest truncated
        assert!(compressed.contains("rust: 201 files"));
        assert!(compressed.contains("python: 8 files"));
        assert!(!compressed.contains("bash: 6 files"));
        assert!(compressed.contains("(truncated)"));
        // Symbols by kind: all kept
        assert!(compressed.contains("Function: 1146"));
        assert!(compressed.contains("Test: 950"));
        // Hotspots: top 5 kept
        assert!(compressed.contains("src/e.rs"));
        assert!(!compressed.contains("src/f.rs"));
        // Hubs: top 5 kept
        assert!(compressed.contains("lock"));
        assert!(!compressed.contains("bundled"));
        // Entry points: collapsed to count
        assert!(compressed.contains("4 entry points"));
        assert!(!compressed.contains("Function main"));
    }

    #[test]
    fn test_compress_architecture_passthrough_on_bad_format() {
        let raw = "not architecture output";
        assert_eq!(compress_architecture(raw, &json!({})), raw);
    }
}
