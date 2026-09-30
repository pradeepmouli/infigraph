
## Working with Infigraph

### Before you change code
- **Before editing a function:** `get_doc_context` returns its source, callers and callees in one call.
- **Before refactoring:** `find_all_references`, then `transitive_impact` for the blast radius. Never grep for callers, and never trace call chains by hand.
- **Git changes:** `detect_changes` maps a diff to the symbols it touches and what depends on them.

### Workflows
- **Find code:** `search`, then `get_code_snippet` or `symbol_context` for one symbol's detail.
- **Onboarding:** `list_projects` (don't re-index what is already indexed), then `index_project`, `get_architecture`, `get_stats`.
- **Multi-repo:** `group_create`, `group_add` for each repo, `group_index`, `group_sync`, `group_link`.

### Subagents in an Infigraph-indexed project
Agent types without MCP access fall back to grep and glob. Do not spawn them for code tasks:
- **Explore:** use `search` (with `regex=true` to enumerate) and `get_symbols_in_file` directly.
- **Plan:** use `get_architecture`, `get_skeleton` and `get_stats` directly.
- **code-reviewer:** use `get_doc_context`, `get_code_snippet` and `review` directly.

When a task needs a subagent, use **general-purpose**: it has full MCP access.

### Verbose tools: delegate to a subagent
`get_architecture`, `transitive_impact`, `detect_dead_code`, `detect_clusters`, `detect_clones`, `export_graph`, `query_graph`, deep `trace_callers`/`trace_callees`, `group_query` and `group_index` return long output. Every other tool is safe to call inline.

**Reindex:** the `infigraph-reindex` skill (`/infigraph-reindex [path]` where slash commands exist) runs inline, not in a subagent, to save tokens.
