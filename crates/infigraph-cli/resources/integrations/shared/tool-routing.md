## Infigraph — which tool answers which question

| You want | Use |
|---|---|
| Code related to a name or an idea | `search` |
| Every occurrence of a string or pattern (e.g. all call sites before a rename) | `search` with `regex=true` |
| Constants, statics or fields defined in a file | `get_symbols_in_file` |
| A symbol's source, callers or callees | `get_code_snippet` / `get_doc_context` / `find_all_references` |
| Files matching a glob | `list_files` with `glob` (e.g. `glob="src/**/*.rs"`) |
| Exact lines for an edit | `Read` with `offset` |
| A file that is not indexed (config, lockfile, log) | `Read`. Markdown **is** indexed: pass `offset`, or use `search` |

**Text.** `search` returns ranked symbols *and* every line containing the query under "Text matches", each naming the symbol it sits in; symbols holding a match rank first. With `regex=true` the query is a regex and every matching line is listed, not just the top `limit` — that is how to enumerate. `search` does not surface constants; `get_symbols_in_file` lists them with line numbers.

## Infigraph CLI — project lifecycle (run with Bash)

MCP tools answer questions about code; these commands manage the index and its processes. Run them from the project root.

| You want | Run |
|---|---|
| A new git worktree, indexed | `git worktree add -b <branch> <path> <base>`, then `infigraph worktree init <path>` (clones the main checkout's index, reindexes only what differs) |
| To remove a worktree | `infigraph worktree teardown <path>` **first** (stops its daemon via a file inside it), then `git worktree remove <path>`; `infigraph worktree reconcile` fixes the registry after the fact |
| To catch up the index | `infigraph index` (incremental) |
| A graph that is corrupt, wedged, or refused as too large | `infigraph rebuild` (builds fresh, swaps it in); if the growth was legitimate, `infigraph restamp-baseline` |
| To diagnose | `infigraph doctor` (`--global` for every project); `infigraph verify` checks one index offline |
| To see or stop Infigraph processes | `infigraph ps`; `infigraph kill <pid>` (`--force` for SIGKILL; refuses non-Infigraph pids) |
| To stop or restart this project's daemon | `infigraph daemon-stop` / `infigraph daemon-restart` |
| To undo a bad write or a rebuild | `infigraph restore` lists restore points; `infigraph restore <id>` restores one (the current state is kept first) |
| To drop registry entries for projects that are gone | `infigraph gc` |

Everything else: `infigraph --help`, `infigraph <command> --help`. Most analysis commands (`callers`, `impact`, `search`, …) duplicate MCP tools; prefer the tools.

**When a tool is unavailable or errors,** tell the user — for example, to reconnect the infigraph MCP server (`/mcp` in Claude Code). Do not work around the enforcement hook: it blocks raw grep/find/Read on indexed code on purpose, and each block message names the tool to use instead.
