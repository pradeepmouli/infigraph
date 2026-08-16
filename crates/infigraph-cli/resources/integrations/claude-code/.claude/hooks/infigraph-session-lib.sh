#!/usr/bin/env bash
# Shared read-only helpers for infigraph session-continuity hooks.
# Source this after setting `cwd` from the hook's stdin JSON.

# Read latest session JSON and extract key fields inline.
inject_session_summary() {
  local sessions_dir="$cwd/.infigraph/sessions"
  local latest=$(ls -t "$sessions_dir"/session_*.json 2>/dev/null | head -1)
  [ -z "$latest" ] && return
  local summary pending decisions constraints blockers
  summary=$(jq -r '.summary // empty' "$latest" 2>/dev/null)
  pending=$(jq -r '.pending_tasks // empty' "$latest" 2>/dev/null)
  decisions=$(jq -r '.decisions // empty' "$latest" 2>/dev/null)
  constraints=$(jq -r '.constraints // empty' "$latest" 2>/dev/null)
  blockers=$(jq -r '.blockers // empty' "$latest" 2>/dev/null)
  local session_label=$(basename "$latest" .json)
  local out="PRIOR SESSION ($session_label):"
  [ -n "$summary" ] && out="$out Summary: $summary."
  [ -n "$pending" ] && out="$out Pending: $pending."
  [ -n "$decisions" ] && out="$out Decisions: $decisions."
  [ -n "$constraints" ] && out="$out Constraints: $constraints."
  [ -n "$blockers" ] && out="$out Blockers: $blockers."
  local narrative="$sessions_dir/${session_label}.md"
  [ -f "$narrative" ] && out="$out Narrative log: $narrative (read if more context needed)."
  echo "$out"
}

# pgrep can only rule the MCP tool OUT (no process = definitely unreachable),
# never confirm it's IN (a running process could be stale/orphaned/another
# session's) — so it only changes the framing below. The actual gate against
# wasted work is always a cheap availability check done before composing any
# narrative, regardless of what pgrep found.
save_fallback_recipe() {
  local hint
  if pgrep -f "infigraph-mcp" >/dev/null 2>&1; then
    hint="An infigraph-mcp process is running, but that does NOT prove it's connected to this session (could be stale, orphaned, or serving a different session)."
  else
    hint="No infigraph-mcp process detected at all -- mcp__infigraph__save_session is essentially guaranteed to fail."
  fi
  echo "$hint Regardless of that: before writing ANY summary/narrative content, do a cheap availability check first (ToolSearch for mcp__infigraph__save_session, or attempt the call once with minimal args) -- do NOT compose the full narrative until you've confirmed the tool actually responds. FALLBACK once that check shows the tool is unavailable or it errors: write directly to a file at \$cwd/.infigraph/sessions/session_<YYYY-MM-DD>.json (today's UTC date). It's a JSON object with these string fields: id (value 'session_<YYYY-MM-DD>'), name (empty string unless this is a named session), summary, pending_tasks, decisions, files_touched (comma-separated), constraints, assumptions, blockers -- plus these numeric fields: created_at, updated_at, last_accessed (all unix epoch seconds), and confidence (float). If a file for today already exists, merge into it: decisions appends onto the existing value separated by ' | ', files_touched unions the new files in, everything else replaces the old value, created_at is kept from the existing file, confidence becomes the max of 0.9 and the existing confidence. Also append the narrative to the companion session_<YYYY-MM-DD>.md file as a new section (blank line, '## Save @ HH:MM UTC', blank line, narrative text). Then write '0' to \${TMPDIR:-/tmp}/infigraph-sessions/\$session_id.count and '1' to \${TMPDIR:-/tmp}/infigraph-sessions/\$session_id.saved so /clear keeps working. Only use this fallback once the check has actually shown the tool is unreachable -- never skip the check itself."
}

json_escape() {
  python3 -c 'import sys,json; print(json.dumps(sys.stdin.read().strip())[1:-1])'
}
