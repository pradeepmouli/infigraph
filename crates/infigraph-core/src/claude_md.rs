use std::path::Path;

use anyhow::Result;

const VERSION: u32 = 1;

/// Write/update project-level `.claude/CLAUDE.md` with infigraph instructions.
/// Uses sentinel markers for idempotent managed-block replacement.
pub fn ensure_project_claude_md(project_root: &Path) -> Result<()> {
    let claude_dir = project_root.join(".claude");
    let claude_md = claude_dir.join("CLAUDE.md");
    let begin_marker = format!("<!-- BEGIN INFIGRAPH v{} -->", VERSION);
    let end_marker = "<!-- END INFIGRAPH -->";

    let instructions = format!(
        r#"
{begin_marker}
## Infigraph — Code Intelligence (auto-generated)

This project is indexed by Infigraph. Use Infigraph MCP tools FIRST for all code tasks.
Fall back to grep/Read only if Infigraph returns nothing or for non-code files.

### Tool Preferences
1. **`search`** for ALL code search — hybrid BM25+vector+grep in one call
2. **`get_doc_context`** before editing any function — returns source+callers+callees
3. **`trace_callers`** / **`find_all_references`** before refactoring — never grep for callers
4. **`trace_callees`** / **`transitive_impact`** for blast radius
5. Read files directly only for non-code files or Edit tool line-number context

### Subagent Rules
Do NOT spawn these agent types for code tasks — they lack MCP access:
- **Explore** → use `search`, `search_code`, `search_symbols` directly
- **Plan** → use `get_architecture`, `get_skeleton`, `get_stats` directly
- **code-reviewer** → use `get_doc_context`, `get_code_snippet`, `review` directly

For tasks requiring a subagent, use **general-purpose** — it has full MCP/infigraph access.

### Verbose tools — delegate to subagent
`get_architecture`, `transitive_impact`, `detect_dead_code`, `detect_clusters`,
`detect_clones`, `export_graph`, `query_graph`, `trace_callers`/`trace_callees` (deep),
`group_query`, `group_index`
{end_marker}
"#
    );

    let existing = std::fs::read_to_string(&claude_md).unwrap_or_default();

    if existing.contains(&begin_marker) {
        return Ok(());
    }

    let new_content = if let Some(start) = existing.find("<!-- BEGIN INFIGRAPH") {
        if let Some(end_pos) = existing[start..].find(end_marker) {
            let end = start + end_pos + end_marker.len();
            let end = if existing[end..].starts_with('\n') {
                end + 1
            } else {
                end
            };
            format!("{}{}{}", &existing[..start], instructions, &existing[end..])
        } else {
            format!("{}\n{}", existing, instructions)
        }
    } else if existing.is_empty() {
        std::fs::create_dir_all(&claude_dir)?;
        instructions.to_string()
    } else {
        format!("{}\n{}", existing, instructions)
    };

    std::fs::create_dir_all(&claude_dir)?;
    std::fs::write(&claude_md, new_content)?;
    println!("  Updated project CLAUDE.md ({})", claude_md.display());
    Ok(())
}

/// Remove the managed infigraph block from project `.claude/CLAUDE.md`.
/// Returns true if a block was removed, false if nothing to do.
pub fn remove_project_claude_md(project_root: &Path) -> Result<bool> {
    let claude_md = project_root.join(".claude").join("CLAUDE.md");
    let end_marker = "<!-- END INFIGRAPH -->";

    let existing = match std::fs::read_to_string(&claude_md) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };

    let start = match existing.find("<!-- BEGIN INFIGRAPH") {
        Some(s) => s,
        None => return Ok(false),
    };

    let end = match existing[start..].find(end_marker) {
        Some(p) => {
            let e = start + p + end_marker.len();
            if existing[e..].starts_with('\n') {
                e + 1
            } else {
                e
            }
        }
        None => return Ok(false),
    };

    let new_content = format!("{}{}", &existing[..start], &existing[end..]);
    let trimmed = new_content.trim();
    if trimmed.is_empty() {
        std::fs::remove_file(&claude_md)?;
    } else {
        std::fs::write(&claude_md, format!("{}\n", trimmed))?;
    }
    Ok(true)
}
