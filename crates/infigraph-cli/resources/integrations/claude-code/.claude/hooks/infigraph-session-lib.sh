#!/usr/bin/env bash
# Shared read-only helpers for infigraph session-continuity hooks.
# Source this after setting `cwd` from the hook's stdin JSON.

# The short key of a client session id, the way the server names a record
# (`graph::session_key`): its first eight alphanumerics, lowercased. Empty when
# there is none. Callers set `session_id` from the hook's stdin JSON.
session_key() {
  printf '%s' "$1" | tr -cd '[:alnum:]' | cut -c1-8 | tr '[:upper:]' '[:lower:]'
}

# The newest unsaved-transcript backup written within $1 minutes, any session's.
# A cleared session's id is not the one that starts after it, so /clear cannot
# name its own backup; the clock does.
recent_backup() {
  local newest
  newest=$(ls -t "$cwd"/.infigraph/sessions/unsaved-transcript-*.md 2>/dev/null | head -1)
  [ -n "$newest" ] && [ -n "$(find "$newest" -mmin "-$1" 2>/dev/null)" ] && echo "$newest"
}

# This session's own backup: the hook names it by the session's key.
own_backup() {
  local key
  key=$(session_key "$session_id")
  if [ -n "$key" ]; then
    ls -t "$cwd"/.infigraph/sessions/unsaved-transcript-*-"$key".md 2>/dev/null | head -1
  else
    ls -t "$cwd"/.infigraph/sessions/unsaved-transcript-*.md 2>/dev/null | head -1
  fi
}

# Read this session's latest record and extract key fields inline. Another
# session's record is still shown when this one has none, and said to be.
inject_session_summary() {
  local sessions_dir="$cwd/.infigraph/sessions"
  local key note=""
  key=$(session_key "$session_id")
  local latest=""
  if [ -n "$key" ]; then
    latest=$(ls -t "$sessions_dir"/session_*_"$key".json 2>/dev/null | head -1)
    if [ -z "$latest" ]; then
      latest=$(ls -t "$sessions_dir"/session_*.json 2>/dev/null | head -1)
      note=" NOT this session's record: it is another session's or from before sessions were told apart, and this session has none yet."
    fi
  else
    latest=$(ls -t "$sessions_dir"/session_*.json 2>/dev/null | head -1)
    note=" Caller unknown: this may be another session's record."
  fi
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
  echo "$out$note"
}

# Shared write guidance. One real save, not an empty availability probe.
save_fallback_recipe() {
  local recipe key
  recipe='Use save_session once if available; do not probe with an empty save or retry an unavailable tool. If unavailable, merge compact state into $cwd/.infigraph/sessions/session_<UTC YYYY-MM-DD>.json: string fields id, name, summary, pending_tasks, decisions, files_touched, constraints, assumptions, blockers; integer created_at, updated_at, last_accessed (whole epoch seconds, no decimals: int(time.time()), not time.time()), numeric confidence. Preserve omitted fields and created_at; append only new decisions with " | ", union files_touched, replace supplied snapshot fields, set updated_at/last_accessed to now and confidence to at least 0.9. Append only genuinely new narrative to the companion .md when needed. After a successful tool or fallback save, reset ${TMPDIR:-/tmp}/infigraph-sessions/$session_id.count to 0 and .saved to 1. Do not mark a failed save successful; report failure.'
  key=$(session_key "$session_id")
  if [ -n "$key" ]; then
    # The record is this session's: its file carries the key, the tool is told who is calling.
    recipe="${recipe/session_<UTC YYYY-MM-DD>.json/session_<UTC YYYY-MM-DD>_${key}.json}"
    recipe="${recipe//\$session_id/$session_id} Pass session=$session_id to save_session."
  fi
  echo "$recipe"
}

json_escape() {
  python3 -c 'import sys,json; print(json.dumps(sys.stdin.read().strip())[1:-1])'
}
