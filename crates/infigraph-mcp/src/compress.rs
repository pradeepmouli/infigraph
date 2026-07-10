use crate::session_context::CompressionLevel;
use serde_json::Value;

const MIN_TOKENS_TO_COMPRESS: usize = 100;

static BYPASS_TOOLS: &[&str] = &[
    "get_code_snippet",
    "detect_security_issues",
    "detect_taint_flows",
    "detect_interprocedural_taint",
    "detect_path_traversal",
    "compress",
];

pub fn compress_tool_output(raw: &str, tool_name: &str, args: &Value) -> String {
    let level = crate::session_context::get_compression_level();
    compress_tool_output_with_level(raw, tool_name, args, level)
}

pub fn compress_tool_output_with_level(
    raw: &str,
    tool_name: &str,
    args: &Value,
    level: CompressionLevel,
) -> String {
    if level == CompressionLevel::Off {
        return raw.to_string();
    }
    if should_bypass(tool_name, args, raw) {
        return raw.to_string();
    }
    let effective = effective_level(tool_name, level);
    if effective == CompressionLevel::Off {
        return raw.to_string();
    }
    match tool_name {
        "search" => compress_search(raw, args, effective),
        "get_doc_context" => compress_doc_context(raw, args, effective),
        "find_all_references" => compress_references(raw, args, effective),
        "get_architecture" => compress_architecture(raw, args, effective),
        "list_files" => compress_list_files(raw, args),
        "detect_dead_code" => compress_dead_code(raw, args),
        "get_api_surface" => compress_api_surface(raw, args, effective),
        "git_summary" => compress_git_summary(raw, args),
        _ => raw.to_string(),
    }
}

/// Cap compression level per tool based on eval quality results and runtime detail-rate.
/// Search degrades at Aggressive (top-3 drops important results).
/// If detail-request rate >30% for a tool, cap at Summary.
fn effective_level(tool_name: &str, level: CompressionLevel) -> CompressionLevel {
    let mut max_level = match tool_name {
        "search" => CompressionLevel::Summary,
        _ => CompressionLevel::Minimal,
    };
    if crate::session_context::should_reduce_compression(tool_name)
        && max_level > CompressionLevel::Summary
    {
        max_level = CompressionLevel::Summary;
    }
    if level > max_level {
        max_level
    } else {
        level
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

fn compress_search(raw: &str, _args: &Value, level: CompressionLevel) -> String {
    let mut lines = raw.lines().peekable();
    let header = match lines.next() {
        Some(h) if h.starts_with("Search:") => h,
        _ => return raw.to_string(),
    };

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
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with("grep:") || trimmed.starts_with('"') {
                continue;
            }
            symbol_lines.push(line.to_string());
        }
    }

    let max_symbols = match level {
        CompressionLevel::Off => usize::MAX,
        CompressionLevel::Summary => usize::MAX,
        CompressionLevel::Aggressive => 3,
        CompressionLevel::Minimal => 1,
    };

    let mut out = String::with_capacity(raw.len() / 2);
    out.push_str(header);
    out.push('\n');

    for (i, sl) in symbol_lines.iter().enumerate() {
        if i >= max_symbols {
            out.push_str(&format!(
                "  ... ({} more results)\n",
                symbol_lines.len() - max_symbols
            ));
            break;
        }
        out.push_str(sl);
        out.push('\n');
    }

    if level <= CompressionLevel::Summary && !text_section.is_empty() {
        out.push_str("\n---\nText matches:\n");
        out.push_str(&text_section);
    }

    if level <= CompressionLevel::Summary && !doc_section.is_empty() {
        out.push_str("\n---\nDocument matches:\n");
        for line in doc_section.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    if !watcher_warning.is_empty() {
        out.push_str(&watcher_warning);
    }

    out.push_str("\nUse search with detail=true for full source snippets and doc excerpts.");
    out
}

fn compress_doc_context(raw: &str, _args: &Value, level: CompressionLevel) -> String {
    if !raw.starts_with("=== ") {
        return raw.to_string();
    }

    let mut out = String::with_capacity(raw.len() / 3);
    let mut in_source = false;
    let mut source_first_line: Option<String> = None;
    let mut backtick_count = 0;
    let mut caller_count = 0;
    let mut callee_count = 0;
    let mut in_callers = false;
    let mut in_callees = false;

    let max_callers = match level {
        CompressionLevel::Off => usize::MAX,
        CompressionLevel::Summary => usize::MAX,
        CompressionLevel::Aggressive => 3,
        CompressionLevel::Minimal => 0,
    };

    for line in raw.lines() {
        if line == "Source:" {
            in_source = true;
            in_callers = false;
            in_callees = false;
            backtick_count = 0;
            continue;
        }
        if line.starts_with("Callers (") {
            in_source = false;
            in_callers = true;
            in_callees = false;
            caller_count = 0;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if line.starts_with("Callees (") {
            in_callers = false;
            in_callees = true;
            callee_count = 0;
            if caller_count > max_callers {
                out.push_str(&format!(
                    "  ... ({} more callers)\n",
                    caller_count - max_callers
                ));
            }
            out.push_str(line);
            out.push('\n');
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
                let trimmed = line.trim();
                if !trimmed.is_empty() {
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

        if in_callers {
            if !line.trim().is_empty() {
                caller_count += 1;
                if caller_count <= max_callers {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            continue;
        }

        if in_callees {
            if !line.trim().is_empty() {
                callee_count += 1;
                if callee_count <= max_callers {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    if in_callers && caller_count > max_callers {
        out.push_str(&format!(
            "  ... ({} more callers)\n",
            caller_count - max_callers
        ));
    }
    if in_callees && callee_count > max_callers {
        out.push_str(&format!(
            "  ... ({} more callees)\n",
            callee_count - max_callers
        ));
    }

    out
}

fn compress_references(raw: &str, _args: &Value, level: CompressionLevel) -> String {
    if !raw.starts_with("References to ") {
        return raw.to_string();
    }

    if level >= CompressionLevel::Minimal {
        let header = raw.lines().next().unwrap_or("");
        let file_count = raw
            .lines()
            .filter(|l| l.contains(" \u{2014} in "))
            .filter_map(|l| l.trim().split(':').next())
            .collect::<std::collections::HashSet<_>>()
            .len();
        return format!("{header}\n  ({file_count} files — use detail=true for locations)");
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

fn compress_architecture(raw: &str, _args: &Value, level: CompressionLevel) -> String {
    if !raw.contains("=== Language Breakdown ===") {
        return raw.to_string();
    }

    let (lang_limit, hotspot_limit, hub_limit) = match level {
        CompressionLevel::Off => (99, 99, 99),
        CompressionLevel::Summary => (5, 5, 5),
        CompressionLevel::Aggressive => (3, 3, 3),
        CompressionLevel::Minimal => (2, 0, 0),
    };

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
            "lang" => lang_limit,
            "kind" => 99,
            "hotspot" => hotspot_limit,
            "hub" => hub_limit,
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

fn compress_list_files(raw: &str, args: &Value) -> String {
    // Flat file list → directory tree with file counts per leaf dir
    // If glob was specified, show all files (user asked for specific subset)
    if args.get("glob").and_then(|v| v.as_str()).is_some() {
        return raw.to_string();
    }

    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 20 {
        return raw.to_string();
    }

    // Group by directory
    let mut dirs: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut root_files = 0usize;
    for line in &lines {
        let trimmed = line.trim();
        if let Some(pos) = trimmed.rfind('/') {
            let dir = &trimmed[..pos];
            *dirs.entry(dir).or_default() += 1;
        } else {
            root_files += 1;
        }
    }

    // Collapse child dirs into parents when parent has only one child subdir
    // Just show dir → count
    let mut out = String::with_capacity(raw.len() / 3);
    out.push_str(&format!("{} files total:\n", lines.len()));
    if root_files > 0 {
        out.push_str(&format!("  ./ ({root_files} files)\n"));
    }
    for (dir, count) in &dirs {
        out.push_str(&format!("  {dir}/ ({count} files)\n"));
    }
    out.push_str("\nUse list_files with glob pattern to see specific files.");
    out
}

fn compress_dead_code(raw: &str, _args: &Value) -> String {
    // Format: "Saved to ...\n(N lines, M bytes)\n\nPotentially dead code (K symbols):\n  Kind name (file)\n..."
    // Truncated at 4 items by the tool itself. Compress by grouping first 4 by file.
    if !raw.contains("Potentially dead code") {
        return raw.to_string();
    }

    let mut lines = raw.lines();
    let mut out = String::with_capacity(raw.len());

    // Keep header lines until "Potentially dead code"
    for line in &mut lines {
        out.push_str(line);
        out.push('\n');
        if line.starts_with("Potentially dead code") {
            break;
        }
    }

    // Group symbols by file
    let mut by_file: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // "Function name (file/path.rs)"
        if let Some(paren_start) = trimmed.rfind('(') {
            let file = trimmed[paren_start + 1..].trim_end_matches(')');
            let symbol = trimmed[..paren_start].trim();
            by_file
                .entry(file.to_string())
                .or_default()
                .push(symbol.to_string());
        } else {
            out.push_str("  ");
            out.push_str(trimmed);
            out.push('\n');
        }
    }

    for (file, symbols) in &by_file {
        if symbols.len() == 1 {
            out.push_str(&format!("  {} ({file})\n", symbols[0]));
        } else {
            out.push_str(&format!(
                "  {file} ({}x): {}\n",
                symbols.len(),
                symbols.join(", ")
            ));
        }
    }

    out.push_str("\nFull list saved to .infigraph/sessions/analysis/. Use detail=true for source.");
    out
}

fn compress_api_surface(raw: &str, _args: &Value, level: CompressionLevel) -> String {
    if !raw.starts_with("API Surface") {
        return raw.to_string();
    }

    if level >= CompressionLevel::Minimal {
        let header = raw.lines().next().unwrap_or("");
        let file_count = raw.lines().filter(|l| l.starts_with("## ")).count();
        return format!("{header}\n  ({file_count} files — use detail=true for symbols)");
    }

    let mut lines = raw.lines();
    let header = lines.next().unwrap();

    let mut out = String::with_capacity(raw.len() / 2);
    out.push_str(header);
    out.push('\n');

    let mut current_file = String::new();
    let mut symbols: Vec<String> = Vec::new();
    let mut routes: Vec<String> = Vec::new();

    let flush = |out: &mut String, file: &str, symbols: &[String], routes: &[String]| {
        if file.is_empty() {
            return;
        }
        if routes.is_empty() {
            out.push_str(&format!("  {file} ({} symbols)\n", symbols.len()));
        } else {
            out.push_str(&format!(
                "  {file} ({} symbols, {} routes)\n",
                symbols.len(),
                routes.len()
            ));
            for r in routes {
                out.push_str(&format!("    {r}\n"));
            }
        }
    };

    for line in lines {
        if let Some(heading) = line.strip_prefix("## ") {
            flush(&mut out, &current_file, &symbols, &routes);
            current_file = heading.to_string();
            symbols.clear();
            routes.clear();
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("[Route]") {
            routes.push(trimmed.to_string());
        }
        symbols.push(trimmed.to_string());
    }
    flush(&mut out, &current_file, &symbols, &routes);

    out
}

fn compress_git_summary(raw: &str, _args: &Value) -> String {
    // Format: "Git Summary — last N commits\n\n━━ hash date — author — message\n   Files changed: N\n     file\n   Symbols touched (N):\n     + Kind name (file:line)\n..."
    // Compress: keep header + commit lines, collapse symbol lists >5 to count
    if !raw.starts_with("Git Summary") {
        return raw.to_string();
    }

    let mut out = String::with_capacity(raw.len() / 2);
    let mut symbol_count = 0;
    let mut in_symbols = false;
    let max_symbols = 5;

    for line in raw.lines() {
        if line.starts_with("   Symbols touched") {
            in_symbols = true;
            symbol_count = 0;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_symbols {
            if line.starts_with("     ") {
                symbol_count += 1;
                if symbol_count <= max_symbols {
                    out.push_str(line);
                    out.push('\n');
                }
                continue;
            } else {
                if symbol_count > max_symbols {
                    out.push_str(&format!(
                        "     ... and {} more symbols\n",
                        symbol_count - max_symbols
                    ));
                }
                in_symbols = false;
            }
        }
        out.push_str(line);
        out.push('\n');
    }

    if in_symbols && symbol_count > max_symbols {
        out.push_str(&format!(
            "     ... and {} more symbols\n",
            symbol_count - max_symbols
        ));
    }

    out
}

// --- Generic content compression (for `compress` MCP tool) ---

#[derive(Debug, PartialEq)]
pub enum ContentType {
    Json,
    JsonArray,
    LogOutput,
    StackTrace,
    BuildOutput,
    FileTree,
    Table,
    PlainText,
}

pub fn classify_content(text: &str) -> ContentType {
    let first_lines: Vec<&str> = text.lines().take(20).collect();

    // Check log/build/stack BEFORE JSON — log lines often start with [INFO] etc.
    let log_markers = first_lines
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.contains("[INFO]")
                || t.contains("[WARN]")
                || t.contains("[ERROR]")
                || t.contains("[DEBUG]")
                || t.contains("INFO ")
                || t.contains("WARN ")
                || t.contains("ERROR ")
                || t.contains("DEBUG ")
        })
        .count();
    if log_markers >= 2 {
        return ContentType::LogOutput;
    }

    // Build output: "Compiling", "Checking", "Building", cargo/make patterns
    let build_markers = first_lines
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("Compiling ")
                || t.starts_with("Checking ")
                || t.starts_with("Building ")
                || t.starts_with("Linking ")
                || t.starts_with("Finished ")
                || t.starts_with("warning[")
                || t.starts_with("error[")
                || t.starts_with("warning:")
                || t.starts_with("error:")
        })
        .count();
    if build_markers >= 2 {
        return ContentType::BuildOutput;
    }

    // Stack trace: "at " + file:line patterns, "Traceback", "panic", "Exception"
    let stack_markers = first_lines
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("at ")
                || t.contains("    at ")
                || t.starts_with("Traceback")
                || t.contains("panic")
                || t.contains("Exception")
                || t.contains("Error:")
        })
        .count();
    if stack_markers >= 3 {
        return ContentType::StackTrace;
    }

    // File tree: box-drawing chars
    if first_lines
        .iter()
        .any(|l| l.contains("├──") || l.contains("└──"))
    {
        return ContentType::FileTree;
    }

    // Table: markdown table separators or tab-aligned columns
    if first_lines
        .iter()
        .any(|l| l.contains("| --- |") || l.contains("|---|") || l.contains("| -- |"))
    {
        return ContentType::Table;
    }

    // JSON — check AFTER specific formats (log lines start with [)
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        return ContentType::Json;
    }
    if trimmed.starts_with('[') {
        return ContentType::JsonArray;
    }

    ContentType::PlainText
}

pub fn compress_generic(text: &str) -> String {
    let content_type = classify_content(text);
    match content_type {
        ContentType::Json => compress_json(text),
        ContentType::JsonArray => compress_json(text),
        ContentType::LogOutput => compress_log(text),
        ContentType::StackTrace => compress_stack_trace(text),
        ContentType::BuildOutput => compress_build_output(text),
        ContentType::FileTree => compress_file_tree(text),
        ContentType::Table => compress_table(text),
        ContentType::PlainText => text.to_string(),
    }
}

fn compress_json(text: &str) -> String {
    let parsed: Result<Value, _> = serde_json::from_str(text.trim());
    let val = match parsed {
        Ok(v) => v,
        Err(_) => return text.to_string(),
    };

    match &val {
        Value::Array(arr) if arr.len() > 3 => {
            let schema = if let Some(first) = arr.first() {
                infer_json_schema(first)
            } else {
                "unknown".to_string()
            };
            let mut out = format!("JSON array ({} items), schema: {}\n", arr.len(), schema);
            out.push_str(&format!("Sample[0]: {}\n", truncate_json(&arr[0], 200)));
            out.push_str(&format!(
                "Sample[{}]: {}",
                arr.len() - 1,
                truncate_json(arr.last().unwrap(), 200)
            ));
            out
        }
        Value::Object(map) if text.len() > 500 => {
            let mut out = format!("JSON object ({} keys): ", map.len());
            let keys: Vec<&String> = map.keys().take(20).collect();
            out.push_str(
                &keys
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            if map.len() > 20 {
                out.push_str(&format!(", ... ({} more)", map.len() - 20));
            }
            out.push('\n');
            for (k, v) in map.iter().take(5) {
                out.push_str(&format!("  {k}: {}\n", truncate_json(v, 100)));
            }
            if map.len() > 5 {
                out.push_str(&format!("  ... ({} more keys)\n", map.len() - 5));
            }
            out
        }
        _ => text.to_string(),
    }
}

fn infer_json_schema(val: &Value) -> String {
    match val {
        Value::Object(map) => {
            let fields: Vec<String> = map
                .iter()
                .take(10)
                .map(|(k, v)| {
                    let t = match v {
                        Value::Number(_) => "num",
                        Value::String(_) => "str",
                        Value::Bool(_) => "bool",
                        Value::Array(_) => "array",
                        Value::Object(_) => "obj",
                        Value::Null => "null",
                    };
                    format!("{k}: {t}")
                })
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
        _ => "mixed".to_string(),
    }
}

fn truncate_json(val: &Value, max_len: usize) -> String {
    let s = serde_json::to_string(val).unwrap_or_default();
    if s.len() <= max_len {
        s
    } else {
        format!("{}...", &s[..max_len])
    }
}

fn compress_log(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 10 {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() / 3);
    let mut prev_pattern: Option<String> = None;
    let mut dup_count = 0usize;

    for line in &lines {
        let trimmed = line.trim();
        // Extract pattern: strip numbers, timestamps, IDs
        let pattern = trimmed
            .chars()
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect::<String>();

        let is_error = trimmed.contains("ERROR")
            || trimmed.contains("WARN")
            || trimmed.contains("error")
            || trimmed.contains("warning");

        if is_error {
            if dup_count > 0 {
                out.push_str(&format!("  ... ({dup_count} similar lines)\n"));
                dup_count = 0;
            }
            prev_pattern = None;
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if prev_pattern.as_deref() == Some(&pattern) {
            dup_count += 1;
        } else {
            if dup_count > 0 {
                out.push_str(&format!("  ... ({dup_count} similar lines)\n"));
            }
            dup_count = 0;
            prev_pattern = Some(pattern);
            out.push_str(line);
            out.push('\n');
        }
    }
    if dup_count > 0 {
        out.push_str(&format!("  ... ({dup_count} similar lines)\n"));
    }
    out
}

fn compress_stack_trace(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len() / 2);
    let mut framework_count = 0;

    for line in &lines {
        let trimmed = line.trim();
        let is_framework = trimmed.starts_with("at java.")
            || trimmed.starts_with("at sun.")
            || trimmed.starts_with("at org.springframework")
            || trimmed.starts_with("at io.netty")
            || trimmed.starts_with("at tokio::")
            || trimmed.starts_with("at std::")
            || trimmed.starts_with("at core::")
            || trimmed.contains("<internal>")
            || trimmed.contains("node_modules/")
            || trimmed.contains("site-packages/");

        if is_framework {
            framework_count += 1;
        } else {
            if framework_count > 0 {
                out.push_str(&format!("    ... ({framework_count} framework frames)\n"));
                framework_count = 0;
            }
            out.push_str(line);
            out.push('\n');
        }
    }
    if framework_count > 0 {
        out.push_str(&format!("    ... ({framework_count} framework frames)\n"));
    }
    out
}

fn compress_build_output(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len() / 3);
    let mut compile_count = 0usize;

    for line in &lines {
        let trimmed = line.trim();
        let is_compile_line = trimmed.starts_with("Compiling ")
            || trimmed.starts_with("Checking ")
            || trimmed.starts_with("Downloading ");

        if is_compile_line {
            compile_count += 1;
            continue;
        }

        if compile_count > 0 {
            out.push_str(&format!("({compile_count} compile/check steps)\n"));
            compile_count = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    if compile_count > 0 {
        out.push_str(&format!("({compile_count} compile/check steps)\n"));
    }
    out
}

fn compress_file_tree(text: &str) -> String {
    // Collapse deep subtrees: if a node has only leaf children, show "dir/ (N files)"
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 30 {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() / 2);
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let depth = line.chars().take_while(|c| *c == ' ' || *c == '│').count();
        let trimmed = line.trim_start_matches([' ', '│', '├', '└', '─']);
        let trimmed = trimmed.trim();

        if trimmed.ends_with('/') {
            // Directory — count immediate children
            let mut child_count = 0;
            let mut j = i + 1;
            while j < lines.len() {
                let child_depth = lines[j]
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '│')
                    .count();
                if child_depth <= depth {
                    break;
                }
                if child_depth == depth + 2 || child_depth == depth + 4 {
                    child_count += 1;
                }
                j += 1;
            }
            if child_count > 5 {
                out.push_str(line);
                out.push_str(&format!(" ({child_count} items)\n"));
                i = j;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
        i += 1;
    }
    out
}

fn compress_table(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 8 {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() / 3);
    // Keep header (first 2-3 lines including separator), first 3 data rows, last row
    let sep_idx = lines.iter().position(|l| l.contains("---")).unwrap_or(1);
    let header_end = sep_idx + 1;

    for line in lines.iter().take(header_end) {
        out.push_str(line);
        out.push('\n');
    }

    let data_lines: Vec<&&str> = lines[header_end..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if data_lines.len() <= 4 {
        for line in &data_lines {
            out.push_str(line);
            out.push('\n');
        }
    } else {
        for line in data_lines.iter().take(3) {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&format!("... ({} more rows)\n", data_lines.len() - 4));
        out.push_str(data_lines.last().unwrap());
        out.push('\n');
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

        let compressed = compress_search(raw, &json!({}), CompressionLevel::Summary);

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

        let compressed = compress_doc_context(raw, &json!({}), CompressionLevel::Summary);

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
        assert_eq!(
            compress_doc_context(raw, &json!({}), CompressionLevel::Summary),
            raw
        );
    }

    #[test]
    fn test_compress_search_passthrough_on_bad_format() {
        let raw = "something unexpected";
        assert_eq!(
            compress_search(raw, &json!({}), CompressionLevel::Summary),
            raw
        );
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

        let compressed = compress_references(raw, &json!({}), CompressionLevel::Summary);

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

        let compressed = compress_references(raw, &json!({}), CompressionLevel::Summary);

        // Single refs kept as-is (no grouping needed)
        assert!(compressed.contains("src/a.rs:10 — in bar"));
        assert!(compressed.contains("src/b.rs:20 — in baz"));
    }

    #[test]
    fn test_compress_references_passthrough_on_bad_format() {
        let raw = "not a references output";
        assert_eq!(
            compress_references(raw, &json!({}), CompressionLevel::Summary),
            raw
        );
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

        let compressed = compress_architecture(raw, &json!({}), CompressionLevel::Summary);

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
        assert_eq!(
            compress_architecture(raw, &json!({}), CompressionLevel::Summary),
            raw
        );
    }

    #[test]
    fn test_compress_list_files_collapses_dirs() {
        let mut raw = String::new();
        for i in 0..30 {
            raw.push_str(&format!("src/auth/file{i}.rs\n"));
        }
        raw.push_str("src/routes/handler.rs\n");
        raw.push_str("Cargo.toml\n");

        let compressed = compress_list_files(&raw, &json!({}));

        assert!(compressed.contains("32 files total:"));
        assert!(compressed.contains("src/auth/ (30 files)"));
        assert!(compressed.contains("src/routes/ (1 files)"));
        assert!(compressed.contains("./ (1 files)"));
        assert!(compressed.contains("glob pattern"));
        // Individual files not listed
        assert!(!compressed.contains("file15.rs"));
    }

    #[test]
    fn test_compress_list_files_passthrough_small() {
        let raw = "src/a.rs\nsrc/b.rs\n";
        assert_eq!(compress_list_files(raw, &json!({})), raw);
    }

    #[test]
    fn test_compress_list_files_passthrough_with_glob() {
        let mut raw = String::new();
        for i in 0..30 {
            raw.push_str(&format!("src/file{i}.rs\n"));
        }
        let compressed = compress_list_files(&raw, &json!({"glob": "*.rs"}));
        assert_eq!(compressed, raw);
    }

    #[test]
    fn test_compress_dead_code_groups_by_file() {
        let raw = "Saved to /tmp/dead.md\n(100 lines, 5000 bytes)\n\nPotentially dead code (4 symbols):\n  Function foo (src/a.rs)\n  Function bar (src/a.rs)\n  Function baz (src/b.rs)\n  Function qux (src/c.rs)\n";

        let compressed = compress_dead_code(raw, &json!({}));

        assert!(compressed.contains("Potentially dead code (4 symbols):"));
        assert!(compressed.contains("src/a.rs (2x): Function foo, Function bar"));
        assert!(compressed.contains("Function baz (src/b.rs)"));
        assert!(compressed.contains("Function qux (src/c.rs)"));
    }

    #[test]
    fn test_compress_api_surface_collapses_symbols_keeps_routes() {
        let raw = "API Surface (8 symbols):\n\n## src/lib.rs\n  [Class] Foo (L1)\n  [Method] bar (L5)\n  [Method] baz (L10)\n## src/routes.rs\n  [Route] GET /users (L3) — route GET /users\n  [Route] POST /users (L8) — route POST /users\n";

        let compressed = compress_api_surface(raw, &json!({}), CompressionLevel::Summary);

        assert!(compressed.contains("API Surface (8 symbols):"));
        assert!(compressed.contains("src/lib.rs (3 symbols)"));
        assert!(!compressed.contains("[Class] Foo"));
        assert!(compressed.contains("src/routes.rs (2 symbols, 2 routes)"));
        assert!(compressed.contains("[Route] GET /users"));
    }

    #[test]
    fn test_compress_git_summary_truncates_symbols() {
        let mut raw = "Git Summary — last 1 commits\n\n━━ abc123 2026-07-10 — User — Big commit\n   Files changed: 1\n     src/lib.rs\n   Symbols touched (10):\n".to_string();
        for i in 0..10 {
            raw.push_str(&format!("     + Function fn{i} (src/lib.rs:{i})\n"));
        }

        let compressed = compress_git_summary(&raw, &json!({}));

        assert!(compressed.contains("Symbols touched (10):"));
        assert!(compressed.contains("Function fn0"));
        assert!(compressed.contains("Function fn4"));
        assert!(!compressed.contains("Function fn5"));
        assert!(compressed.contains("... and 5 more symbols"));
    }

    #[test]
    fn test_compress_git_summary_passthrough_small() {
        let raw = "Git Summary — last 1 commits\n\n━━ abc 2026-07-10 — User — Fix\n   Files changed: 1\n     src/a.rs\n   Symbols touched (1):\n     + Function foo (src/a.rs:1)\n";
        let compressed = compress_git_summary(raw, &json!({}));
        assert!(compressed.contains("Function foo"));
        assert!(!compressed.contains("... and"));
    }

    // --- Generic compressor tests ---

    #[test]
    fn test_classify_json_object() {
        assert_eq!(classify_content(r#"{"key": "val"}"#), ContentType::Json);
    }

    #[test]
    fn test_classify_json_array() {
        assert_eq!(classify_content(r#"[1, 2, 3]"#), ContentType::JsonArray);
    }

    #[test]
    fn test_classify_log_output() {
        let log = "[INFO] Starting server\n[INFO] Listening on :8080\n[ERROR] Connection failed\n";
        assert_eq!(classify_content(log), ContentType::LogOutput);
    }

    #[test]
    fn test_classify_build_output() {
        let build = "Compiling serde v1.0\nCompiling tokio v1.0\nChecking myapp v0.1\nerror[E0308]: type mismatch\n";
        assert_eq!(classify_content(build), ContentType::BuildOutput);
    }

    #[test]
    fn test_classify_stack_trace() {
        let trace = "Error: NullPointerException\n    at com.app.Auth.login(Auth.java:45)\n    at java.lang.Thread.run(Thread.java:748)\n    at com.app.Main.main(Main.java:10)\n";
        assert_eq!(classify_content(trace), ContentType::StackTrace);
    }

    #[test]
    fn test_classify_file_tree() {
        let tree = "src/\n├── auth/\n│   ├── login.rs\n│   └── logout.rs\n└── main.rs\n";
        assert_eq!(classify_content(tree), ContentType::FileTree);
    }

    #[test]
    fn test_classify_table() {
        let table = "| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |\n";
        assert_eq!(classify_content(table), ContentType::Table);
    }

    #[test]
    fn test_classify_plain_text() {
        assert_eq!(classify_content("Hello world"), ContentType::PlainText);
    }

    #[test]
    fn test_compress_json_array() {
        let arr: Vec<serde_json::Value> = (0..50)
            .map(|i| json!({"id": i, "name": format!("item{i}"), "active": true}))
            .collect();
        let text = serde_json::to_string_pretty(&arr).unwrap();

        let compressed = compress_json(&text);

        assert!(compressed.contains("JSON array (50 items)"));
        assert!(compressed.contains("id: num"));
        assert!(compressed.contains("name: str"));
        assert!(compressed.contains("Sample[0]"));
        assert!(compressed.contains("Sample[49]"));
    }

    #[test]
    fn test_compress_log_dedup() {
        let mut log = String::new();
        for i in 0..20 {
            log.push_str(&format!("[INFO] Processing item {i}/20\n"));
        }
        log.push_str("[ERROR] Failed on item 15\n");

        let compressed = compress_log(&log);

        assert!(compressed.contains("[INFO] Processing item 0/20"));
        assert!(compressed.contains("similar lines"));
        assert!(compressed.contains("[ERROR] Failed on item 15"));
    }

    #[test]
    fn test_compress_build_output_collapses() {
        let mut build = String::new();
        for i in 0..20 {
            build.push_str(&format!("Compiling crate{i} v1.0\n"));
        }
        build.push_str("warning: unused variable `x` (src/lib.rs:23)\n");
        build.push_str("error[E0308]: type mismatch (src/main.rs:10)\n");
        build.push_str("Finished dev profile\n");

        let compressed = compress_build_output(&build);

        assert!(compressed.contains("(20 compile/check steps)"));
        assert!(compressed.contains("warning: unused variable"));
        assert!(compressed.contains("error[E0308]"));
        assert!(compressed.contains("Finished dev profile"));
        assert!(!compressed.contains("Compiling crate5"));
    }

    #[test]
    fn test_compress_stack_trace_collapses_framework() {
        let trace = "Error: connection timeout\n    at com.app.Db.query(Db.java:45)\n    at java.lang.Thread.run(Thread.java:748)\n    at sun.reflect.Invoke(Invoke.java:20)\n    at com.app.Main.main(Main.java:10)\n";

        let compressed = compress_stack_trace(trace);

        assert!(compressed.contains("com.app.Db.query"));
        assert!(compressed.contains("com.app.Main.main"));
        assert!(compressed.contains("2 framework frames"));
        assert!(!compressed.contains("java.lang.Thread"));
    }

    #[test]
    fn test_compress_table_truncates() {
        let mut table = "| Name | Score |\n| --- | --- |\n".to_string();
        for i in 0..20 {
            table.push_str(&format!("| user{i} | {i} |\n", i = i));
        }

        let compressed = compress_table(&table);

        assert!(compressed.contains("| Name | Score |"));
        assert!(compressed.contains("| user0 | 0 |"));
        assert!(compressed.contains("| user2 | 2 |"));
        assert!(compressed.contains("more rows"));
        assert!(compressed.contains("| user19 | 19 |"));
        assert!(!compressed.contains("| user10 |"));
    }

    // --- Phase 6: Level-aware compression tests ---

    #[test]
    fn test_search_aggressive_limits_results() {
        let raw = "Search: 'foo' (5 symbol results, 0 text matches)\n\n0.95  Function a (f.rs:L1-5)\n0.90  Function b (f.rs:L6-10)\n0.85  Function c (f.rs:L11-15)\n0.80  Function d (f.rs:L16-20)\n0.75  Function e (f.rs:L21-25)\n";

        let compressed = compress_search(raw, &json!({}), CompressionLevel::Aggressive);
        assert!(compressed.contains("Function a"));
        assert!(compressed.contains("Function c"));
        assert!(!compressed.contains("Function d"));
        assert!(compressed.contains("2 more results"));
    }

    #[test]
    fn test_search_minimal_one_result() {
        let raw = "Search: 'foo' (3 symbol results, 2 text matches)\n\n0.95  Function a (f.rs:L1-5)\n0.90  Function b (f.rs:L6-10)\n0.85  Function c (f.rs:L11-15)\n\n---\nText matches:\nf.rs:1: let foo = 1;\nf.rs:2: let bar = foo;\n";

        let compressed = compress_search(raw, &json!({}), CompressionLevel::Minimal);
        assert!(compressed.contains("Function a"));
        assert!(!compressed.contains("Function b"));
        assert!(compressed.contains("2 more results"));
        assert!(!compressed.contains("Text matches:"));
    }

    #[test]
    fn test_doc_context_aggressive_truncates_callers() {
        let raw = "=== Function login ===\nFile: src/lib.rs:1-10\n\nSource:\n```\n  1  pub fn login() {}\n```\n\nCallers (5):\n  a::x\n  b::y\n  c::z\n  d::w\n  e::v\n\nCallees (4):\n  f::a\n  f::b\n  f::c\n  f::d\n";

        let compressed = compress_doc_context(raw, &json!({}), CompressionLevel::Aggressive);
        assert!(compressed.contains("a::x"));
        assert!(compressed.contains("c::z"));
        assert!(!compressed.contains("d::w"));
        assert!(compressed.contains("2 more callers"));
        assert!(compressed.contains("f::c"));
        assert!(!compressed.contains("f::d"));
        assert!(compressed.contains("1 more callees"));
    }

    #[test]
    fn test_doc_context_minimal_no_callers() {
        let raw = "=== Function login ===\nFile: src/lib.rs:1-10\n\nSource:\n```\n  1  pub fn login() {}\n```\n\nCallers (3):\n  a::x\n  b::y\n  c::z\n\nCallees (2):\n  f::a\n  f::b\n";

        let compressed = compress_doc_context(raw, &json!({}), CompressionLevel::Minimal);
        assert!(!compressed.contains("a::x"));
        assert!(compressed.contains("3 more callers"));
        assert!(compressed.contains("2 more callees"));
    }

    #[test]
    fn test_references_minimal_count_only() {
        let raw = "References to 'foo' (4 total):\n\n  src/a.rs:1 \u{2014} in bar\n  src/a.rs:5 \u{2014} in baz\n  src/b.rs:10 \u{2014} in qux\n  src/c.rs:20 \u{2014} in quux\n";

        let compressed = compress_references(raw, &json!({}), CompressionLevel::Minimal);
        assert!(compressed.contains("References to 'foo' (4 total):"));
        assert!(compressed.contains("3 files"));
        assert!(!compressed.contains("bar"));
    }

    #[test]
    fn test_architecture_aggressive_fewer_items() {
        let raw = "\
=== Language Breakdown ===
                  rust: 201 files
              markdown: 24 files
                  toml: 16 files
                  json: 10 files
                python: 8 files
                  bash: 6 files

=== Symbols by Kind ===
              Function: 1146

=== Hotspot Files (most symbols) ===
   1. src/a.rs       220 symbols
   2. src/b.rs       85 symbols
   3. src/c.rs       83 symbols
   4. src/d.rs       77 symbols
   5. src/e.rs       72 symbols
";

        let compressed = compress_architecture(raw, &json!({}), CompressionLevel::Aggressive);
        assert!(compressed.contains("toml: 16 files"));
        assert!(!compressed.contains("json: 10 files"));
        assert!(compressed.contains("src/c.rs"));
        assert!(!compressed.contains("src/d.rs"));
    }

    #[test]
    fn test_off_level_passthrough() {
        let raw = "x ".repeat(200);
        let result =
            compress_tool_output_with_level(&raw, "search", &json!({}), CompressionLevel::Off);
        assert_eq!(result, raw);
    }

    #[test]
    fn test_effective_level_caps_search() {
        assert_eq!(
            effective_level("search", CompressionLevel::Aggressive),
            CompressionLevel::Summary
        );
        assert_eq!(
            effective_level("search", CompressionLevel::Minimal),
            CompressionLevel::Summary
        );
        assert_eq!(
            effective_level("search", CompressionLevel::Summary),
            CompressionLevel::Summary
        );
        assert_eq!(
            effective_level("get_doc_context", CompressionLevel::Aggressive),
            CompressionLevel::Aggressive
        );
        assert_eq!(
            effective_level("get_doc_context", CompressionLevel::Minimal),
            CompressionLevel::Minimal
        );
    }

    #[test]
    fn test_api_surface_minimal_count_only() {
        let raw = "API Surface (5 symbols):\n\n## src/lib.rs\n  [Class] Foo (L1)\n  [Method] bar (L5)\n## src/routes.rs\n  [Route] GET /users (L3)\n";

        let compressed = compress_api_surface(raw, &json!({}), CompressionLevel::Minimal);
        assert!(compressed.contains("API Surface (5 symbols):"));
        assert!(compressed.contains("2 files"));
        assert!(!compressed.contains("[Class]"));
    }
}
