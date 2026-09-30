## Infigraph — Primary Code Intelligence

Infigraph MCP is indexed. Use Infigraph tools FIRST for all code tasks. Read non-code files directly. If an Infigraph tool is unavailable or errors, tell the user rather than working around the enforcement hook.

### Rules
1. **`search`** for ALL code search — ranked symbols plus every line containing the text, in one call; **`regex=true`** lists every occurrence (e.g. all call sites) rather than the top `limit`. Constants: `get_symbols_in_file`.
2. **`get_doc_context`** before editing any function; **`find_all_references`** / **`transitive_impact`** before refactoring. Never grep for callers or trace call chains by hand.
3. Read files directly only for non-code files (configs, docs, manifests) or Edit tool line-number context.
4. Which tool answers which question, every tool's parameters, subagent and verbose-tool rules: the `infigraph-tool-routing` skill (inlined below where skills aren't supported).

### Session Continuity — MANDATORY
- **On session start:** MUST call `get_latest_session` to resume prior context
- **After context compaction:** if you see "continued from a previous conversation" or a compaction summary, IMMEDIATELY call `save_session` with whatever context survived before doing anything else
- **MUST call `save_session` IMMEDIATELY (before responding to the user)** when ANY of these occur. No session-end signal exists — if you don't save now, context is lost forever:
  1. **Finding** — root cause identified, discovered a bug, learned how something works
  2. **Milestone** — bug fixed and verified, feature committed, test passing, build green
  3. **Decision** — chose an approach, ruled something out, changed strategy
  4. **Task done** — any pending task from a prior session is completed
  5. **Periodic** — if you have NOT called `save_session` in the last 5 exchanges with the user, call it NOW regardless of whether anything dramatic happened. This is a hard rule, not a suggestion.
- Do NOT defer saves ("I'll save later"). Do NOT batch them. Do NOT wait for user to ask.
- "Later" does not exist — context compaction or session end can happen at any moment.
- **Before `/clear`:** ALWAYS call `save_session` first — `/clear` wipes context and LM2 can only restore what was persisted. Unsaved reasoning, decisions, and in-flight work will be lost.
- Same-day saves merge: summary/pending_tasks overwrite, decisions append, files_touched union
- **Narrative dumps:** On every `save_session`, include `narrative` field with full session story — what was explored, found, reasoned, decided, and why. Chronological prose, not terse bullets. Written to `.infigraph/sessions/session_YYYY-MM-DD.md` and embedded for semantic search. On session start, if `get_latest_session` shows a narrative log path, read it when structured fields aren't enough context.

### Session Field Guide
- **decisions** — structured format: `Goal: X. Decision: Y. Why: Z. Invalidates-if: W.`
- **constraints** — things that failed: `Tried: X. Failed because: Y. Do not retry unless: Z.`
- **assumptions** — what current approach depends on: `Assumes: X. If X changes: Y.`
- **blockers** — stuck items needing human input or external dependency
- **narrative** — full session story: explorations, findings, reasoning, code changes, decisions in chronological order. Write as prose, not structured fields.
