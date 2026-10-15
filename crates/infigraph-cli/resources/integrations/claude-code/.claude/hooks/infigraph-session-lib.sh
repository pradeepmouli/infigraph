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

# Shared write guidance. One real save, not an empty availability probe.
save_fallback_recipe() {
  echo 'Use save_session once if available; do not probe with an empty save or retry an unavailable tool. If unavailable, merge compact state into $cwd/.infigraph/sessions/session_<UTC YYYY-MM-DD>.json: string fields id, name, summary, pending_tasks, decisions, files_touched, constraints, assumptions, blockers; numeric created_at, updated_at, last_accessed (epoch seconds), confidence. Preserve omitted fields and created_at; append only new decisions with " | ", union files_touched, replace supplied snapshot fields, set updated_at/last_accessed to now and confidence to at least 0.9. Append only genuinely new narrative to the companion .md when needed. After a successful tool or fallback save, reset ${TMPDIR:-/tmp}/infigraph-sessions/$session_id.count to 0 and .saved to 1. Do not mark a failed save successful; report failure.'
}

json_escape() {
  python3 -c 'import sys,json; print(json.dumps(sys.stdin.read().strip())[1:-1])'
}
