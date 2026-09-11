use std::path::Path;

use anyhow::Result;

/// Bump whenever the block's text changes: a project whose file already holds
/// this version's marker is left alone, so an unbumped edit never reaches it.
const VERSION: u32 = 3;

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
Read non-code files directly. If an Infigraph tool is unavailable or errors, tell the user
(e.g. to reconnect the MCP server) rather than working around the enforcement hook.

### Tool Preferences
1. **`search`** for ALL code search — ranked symbols plus every line containing the text; **`regex=true`** lists every occurrence (e.g. all call sites). Constants: `get_symbols_in_file`. Full routing: the `infigraph-tool-routing` skill
2. **`get_doc_context`** before editing any function — returns source+callers+callees
3. **`trace_callers`** / **`find_all_references`** before refactoring — never grep for callers
4. **`trace_callees`** / **`transitive_impact`** for blast radius
5. Read files directly only for non-code files or Edit tool line-number context

### Subagent Rules
Do NOT spawn these agent types for code tasks — they lack MCP access:
- **Explore** → use `search` (with `regex=true` to enumerate) and `get_symbols_in_file` directly
- **Plan** → use `get_architecture`, `get_skeleton`, `get_stats` directly
- **code-reviewer** → use `get_doc_context`, `get_code_snippet`, `review` directly

For tasks requiring a subagent, use **general-purpose** — it has full MCP/infigraph access.

### Verbose tools — delegate to subagent
`get_architecture`, `transitive_impact`, `detect_dead_code`, `detect_clusters`,
`detect_clones`, `export_graph`, `query_graph`, `trace_callers`/`trace_callees` (deep),
`group_query`, `group_index`

### Context Compression
Tool outputs are automatically compressed to save context window budget.
- Compression scales with session length (Off → Summary → Aggressive → Minimal)
- `search` results are capped at Summary level to preserve result quality
- Security tools (`detect_security_issues`, `detect_taint_flows`, etc.) are never compressed
- `get_code_snippet` passes through uncompressed for edit accuracy
- No action needed — compression is transparent and automatic
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
