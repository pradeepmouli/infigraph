## Infigraph — Primary Code Intelligence

Infigraph MCP is indexed. Use Infigraph tools FIRST for all code tasks. Read non-code files directly. If an Infigraph tool is unavailable or errors, tell the user rather than working around the enforcement hook.

### Rules
1. **`search`** for ALL code search — ranked symbols plus every line containing the text, in one call; **`regex=true`** lists every occurrence (e.g. all call sites) rather than the top `limit`. Constants: `get_symbols_in_file`.
2. **`get_doc_context`** before editing any function; **`find_all_references`** / **`transitive_impact`** before refactoring. Never grep for callers or trace call chains by hand.
3. Read files directly only for non-code files (configs, docs, manifests) or Edit tool line-number context.
4. Which tool answers which question, every tool's parameters, subagent and verbose-tool rules: the `infigraph-tool-routing` skill (inlined below where skills aren't supported).

### Session Continuity
- This concise policy supersedes older generated wording that requests full-session narratives or redundant checkpoints.
- On session start, call `get_latest_session`; read its narrative only if the compact fields are insufficient.
- Save meaningful new findings, decisions, completed work, or blockers before responding. One save may cover several simultaneous triggers.
- After compaction or before `/clear`, preserve unsaved context. Do not resave history already on disk.
- Periodic backstop: after 10 user exchanges without a save, save a compact checkpoint if anything changed. Avoid no-change duplicate saves.
- Keep routine saves around 100–250 tokens, expanding only for essential recovery details. Save silently unless there is a failure.
- `summary` and `pending_tasks` are concise current-state snapshots, because they replace prior values. Preserve unresolved work.
- `decisions` appends: send only new decisions, never repeat the accumulated list. `files_touched` unions: send only newly touched files.
- Snapshot fields are replaced even when omitted: resend brief still-active `constraints`, `assumptions`, and `blockers` when they must survive. Do not duplicate resolved history.
- `narrative` is optional: at most 100 words of new reasoning or context not captured by structured fields. Never retell the full session or duplicate prior narrative.
- Use one real save rather than an empty availability probe. If the tool is unavailable, follow the hook's file fallback and do not claim success unless persistence succeeds.
