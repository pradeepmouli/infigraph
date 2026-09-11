## Infigraph — which tool answers which question

| You want | Use |
|---|---|
| Code related to a name or an idea | `search` |
| Every occurrence of a string or pattern (e.g. all call sites before a rename) | `search` with `regex=true` |
| Constants, statics or fields defined in a file | `get_symbols_in_file` |
| A symbol's source, callers or callees | `get_code_snippet` / `get_doc_context` / `find_all_references` |
| Files matching a glob | `list_files` |
| Exact lines for an edit, or a non-code file | `Read` (pass `offset` for code) |

**Text.** `search` returns ranked symbols *and* every line containing the query under "Text matches", each naming the symbol it sits in; symbols holding a match rank first. With `regex=true` the query is a regex and every matching line is listed, not just the top `limit` — that is how to enumerate. `search` does not surface constants; `get_symbols_in_file` lists them with line numbers.

**When a tool is unavailable or errors,** tell the user — for example, to reconnect the infigraph MCP server (`/mcp` in Claude Code). Do not work around the enforcement hook: it blocks raw grep/find/Read on indexed code on purpose, and each block message names the tool to use instead.
