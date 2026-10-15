#!/usr/bin/env bash
# Infigraph UserPromptSubmit hook — session save reminder
# Counts user exchanges per Claude session. Every 10th exchange, emits a
# reminder to call save_session. Resets when the PostToolUse reset hook fires.
# stdin: JSON {prompt, session_id, cwd, ...}
#
# Compact reminders share recovery guidance with the SessionStart hook.

input=$(cat)
cwd=$(echo "$input" | jq -r '.cwd // empty')

# Only enforce in Infigraph-indexed projects
[ -d "$cwd/.infigraph" ] || exit 0

session_id=$(echo "$input" | jq -r '.session_id // empty')
[ -z "$session_id" ] && exit 0

# Teammate/subagent relays (SendMessage-delivered turns, e.g. idle
# notifications and status pings from background agents) arrive as
# UserPromptSubmit events just like real human input, but aren't a "user
# exchange" in the sense this hook means -- exclude them so the counter
# tracks actual turns with the human, not agent-orchestration chatter.
prompt=$(echo "$input" | jq -r '.prompt // empty')
case "$prompt" in
  *"<teammate-message"*) exit 0 ;;
esac

counter_dir="${TMPDIR:-/tmp}/infigraph-sessions"
mkdir -p "$counter_dir" 2>/dev/null
counter_file="$counter_dir/$session_id.count"

count=0
[ -f "$counter_file" ] && count=$(cat "$counter_file" 2>/dev/null || echo 0)
count=$((count + 1))
echo "$count" > "$counter_file"

if [ $((count % 10)) -eq 0 ]; then
  # Reuse the same compact policy and fallback as compaction recovery.
  source "$(dirname "${BASH_SOURCE[0]}")/infigraph-session-lib.sh"
  full_context="SESSION SAVE: If anything changed, save a compact current summary and pending-work snapshot. Append only new decisions; omit narrative unless new reasoning needs it (at most 100 words). Do not repeat saved history or announce routine saves. $(save_fallback_recipe)"

  json_escaped=$(printf '%s' "$full_context" | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read())[1:-1])')

  cat <<ENDJSON
{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"$json_escaped"}}
ENDJSON
fi

exit 0
