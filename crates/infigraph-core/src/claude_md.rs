use std::path::Path;

use anyhow::Result;

/// Bump whenever the block's text changes: a project whose file already holds
/// this version's marker is left alone, so an unbumped edit never reaches it.
/// A bumped one reaches a project when it is next indexed or its daemon next
/// starts.
const VERSION: u32 = 4;

/// Write/update project-level `.claude/CLAUDE.md` with infigraph instructions.
/// Uses sentinel markers for idempotent managed-block replacement.
///
/// The block is a pointer, not the rules themselves: `infigraph install` puts
/// the rules in the user's global `~/.claude/CLAUDE.md` and the routing in the
/// `infigraph-tool-routing` skill, and Claude Code loads every ancestor's
/// CLAUDE.md, so a full copy here was paid for again in every session (#207).
///
/// Only a project store gets one ([`crate::project::is_project_store`]). A
/// daemon started in a directory that holds projects, or in `$HOME` -- whose
/// `.claude/CLAUDE.md` *is* the global file -- used to write the block there
/// before anything decided the directory was not a project.
pub fn ensure_project_claude_md(project_root: &Path) -> Result<()> {
    if !crate::project::is_project_store(project_root) {
        return Ok(());
    }
    let claude_dir = project_root.join(".claude");
    let claude_md = claude_dir.join("CLAUDE.md");
    let begin_marker = format!("<!-- BEGIN INFIGRAPH v{} -->", VERSION);
    let end_marker = "<!-- END INFIGRAPH -->";

    let instructions = format!(
        r#"
{begin_marker}
## Infigraph (auto-generated)

This project is indexed by Infigraph. Use its MCP tools first for code tasks: the
`infigraph-tool-routing` skill (installed by `infigraph install`) says which tool answers
which question and how to set up a worktree. If an Infigraph tool is unavailable or errors,
tell the user (e.g. to reconnect the MCP server) rather than working around the enforcement hook.
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
            // Exactly one blank line before the block (none at the top of the
            // file), however many the old one had: `instructions` brings its
            // own leading newline, so keeping the old gap grew it every bump.
            let before = existing[..start].trim_end_matches('\n');
            let block = if before.is_empty() {
                instructions.trim_start_matches('\n').to_string()
            } else {
                format!("{before}\n{instructions}")
            };
            format!("{block}{}", &existing[end..])
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory `is_project_store` accepts: `.infigraph/graph` and no
    /// `registry.json`.
    fn project() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();
        std::fs::write(tmp.path().join(".infigraph").join("graph"), b"").unwrap();
        tmp
    }

    fn block_of(root: &Path) -> Option<String> {
        std::fs::read_to_string(root.join(".claude").join("CLAUDE.md")).ok()
    }

    #[test]
    fn a_project_gets_the_block_pointing_at_the_skill() {
        let root = project();
        ensure_project_claude_md(root.path()).unwrap();
        let text = block_of(root.path()).expect("block written");
        assert!(text.contains(&format!("<!-- BEGIN INFIGRAPH v{VERSION} -->")));
        assert!(text.contains("`infigraph-tool-routing` skill"));
    }

    #[test]
    fn a_directory_without_a_graph_is_left_alone() {
        // A container of projects, or any directory a daemon was merely
        // started in: no graph, so not a project.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();
        ensure_project_claude_md(tmp.path()).unwrap();
        assert_eq!(block_of(tmp.path()), None);
    }

    #[test]
    fn the_global_store_is_left_alone() {
        // `$HOME` holds the user's global store, and `$HOME/.claude/CLAUDE.md`
        // is the user's global instructions file.
        let home = project();
        std::fs::write(home.path().join(".infigraph").join("registry.json"), b"{}").unwrap();
        ensure_project_claude_md(home.path()).unwrap();
        assert_eq!(block_of(home.path()), None);
    }

    /// Each replacement used to keep the blank lines before the old block and
    /// add one of its own, so every version bump grew the gap by a line.
    #[test]
    fn replacing_a_block_does_not_grow_the_blank_lines_before_it() {
        let root = project();
        let dir = root.path().join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        let stale = "<!-- BEGIN INFIGRAPH v1 -->\nold\n<!-- END INFIGRAPH -->\n";
        std::fs::write(dir.join("CLAUDE.md"), format!("\n\n\n{stale}")).unwrap();
        ensure_project_claude_md(root.path()).unwrap();
        assert!(block_of(root.path())
            .unwrap()
            .starts_with("<!-- BEGIN INFIGRAPH v"));

        std::fs::write(dir.join("CLAUDE.md"), format!("# Mine\n\n\n\n{stale}")).unwrap();
        ensure_project_claude_md(root.path()).unwrap();
        assert!(block_of(root.path())
            .unwrap()
            .starts_with("# Mine\n\n<!-- BEGIN INFIGRAPH v"));
    }

    #[test]
    fn an_older_block_is_replaced_and_the_rest_kept() {
        let root = project();
        let dir = root.path().join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("CLAUDE.md"),
            "# Mine\n\n<!-- BEGIN INFIGRAPH v2 -->\nold\n<!-- END INFIGRAPH -->\n\n## After\n",
        )
        .unwrap();
        ensure_project_claude_md(root.path()).unwrap();
        let text = block_of(root.path()).unwrap();
        assert!(text.starts_with("# Mine\n"));
        assert!(text.contains("## After"));
        assert!(!text.contains("old\n"));
        assert_eq!(text.matches("<!-- BEGIN INFIGRAPH").count(), 1);
    }
}
