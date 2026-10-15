#!/usr/bin/env bash
# Infigraph SessionStart hook — session continuity on startup/resume/compaction
# stdin: JSON {session_id, cwd, source, ...}
# source: "startup" | "resume" | "clear" | "compact"
#
# MCP-liveness note: reads (get_latest_session) don't need a tool-call fallback
# at all -- this hook already reads .infigraph/sessions/*.json directly via
# inject_session_summary, so that content is injected up front regardless of
# whether the MCP tool would work. Only writes (save_session) genuinely need
# a try-then-fallback recipe, since only the LLM can compose new content.

input=$(cat)
cwd=$(echo "$input" | jq -r '.cwd // empty')

# Only enforce in Infigraph-indexed projects
[ -d "$cwd/.infigraph" ] || exit 0

source_type=$(echo "$input" | jq -r '.source // "startup"')
session_id=$(echo "$input" | jq -r '.session_id // empty')

# Clear saved sentinel on new session (must save before /clear)
if [ -n "$session_id" ]; then
  rm -f "${TMPDIR:-/tmp}/infigraph-sessions/$session_id.saved" 2>/dev/null
fi

# Restore and save guidance share one maintained implementation.
source "$(dirname "${BASH_SOURCE[0]}")/infigraph-session-lib.sh"

case "$source_type" in
  compact)
    backup=$(ls -t "$cwd"/.infigraph/sessions/unsaved-transcript-*.md 2>/dev/null | head -1)
    session_ctx=$(inject_session_summary)
    [ -z "$session_ctx" ] && session_ctx="No prior sessions found."
    fallback=$(save_fallback_recipe)
    if [ -n "$backup" ]; then
      full="INFIGRAPH SESSION SAVE (COMPACTION): Context was just compacted. $session_ctx (that's what's already on disk from before this compaction -- no tool call needed to see it). A pre-compaction transcript backup exists at $backup. Read this file (contains recent exchanges as clean markdown), extract key context (summary, decisions, pending tasks, constraints, assumptions, blockers, files touched) not yet captured above, then persist it: $fallback After saving (via either path), delete the backup file with Bash rm. Then call mcp__infigraph__get_latest_session to reload saved context. Do NOT skip this."
    else
      full="INFIGRAPH SESSION SAVE (COMPACTION): Context was just compacted. $session_ctx (that's what's already on disk from before this compaction -- no tool call needed to see it). Pre-compaction decisions and context not yet captured above are at risk of being lost -- capture them now. $fallback Then call mcp__infigraph__get_latest_session to reload saved context. Do NOT skip this."
    fi
    escaped=$(printf '%s' "$full" | json_escape)
    cat <<ENDJSON
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"$escaped"}}
ENDJSON
    ;;
  startup|resume)
    session_ctx=$(inject_session_summary)
    if [ -n "$session_ctx" ]; then
      escaped=$(printf '%s' "$session_ctx" | json_escape)
      cat <<ENDJSON
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"INFIGRAPH SESSION RESTORE: $escaped You MUST print a visible summary of prior session context to the user (summary + pending tasks) so they can see session continuity is working."}}
ENDJSON
    else
      cat <<'ENDJSON'
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"INFIGRAPH SESSION RESTORE: No prior sessions found. Fresh start."}}
ENDJSON
    fi
    ;;
  clear)
    # Reset exchange counters on /clear
    session_id=$(echo "$input" | jq -r '.session_id // empty')
    if [ -n "$session_id" ]; then
      echo "0" > "${TMPDIR:-/tmp}/infigraph-sessions/$session_id.count" 2>/dev/null
      echo "0" > "${TMPDIR:-/tmp}/claude-clear-suggest/$session_id.count" 2>/dev/null
    fi
    # Reset test-context sentinel on clear
    rm -f "$cwd/.infigraph/.test-context-called" 2>/dev/null
    rm -f "$cwd/.infigraph/.search-fallback-allowed" 2>/dev/null
    backup=$(ls -t "$cwd"/.infigraph/sessions/unsaved-transcript-*.md 2>/dev/null | head -1)
    session_ctx=$(inject_session_summary)
    if [ -n "$backup" ]; then
      fallback=$(save_fallback_recipe)
      full="INFIGRAPH CONTEXT RESET: Context was cleared. $session_ctx You MUST print a visible summary of prior session context to the user (summary + pending tasks) so they can see session continuity is working. A pre-clear transcript backup exists at $backup. Read this file (contains recent exchanges as clean markdown), extract key context (summary, decisions, pending tasks, files touched), then persist it: $fallback After saving (via either path), delete the backup file with Bash rm. Do NOT proceed with user work until this recovery is complete."
      escaped=$(printf '%s' "$full" | json_escape)
      cat <<ENDJSON
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"$escaped"}}
ENDJSON
    else
      escaped=$(printf '%s' "$session_ctx" | json_escape)
      cat <<ENDJSON
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"INFIGRAPH CONTEXT RESET: Context was cleared. $escaped You MUST print a visible summary of prior session context to the user (summary + pending tasks) so they can see session continuity is working."}}
ENDJSON
    fi
    ;;
esac

exit 0
