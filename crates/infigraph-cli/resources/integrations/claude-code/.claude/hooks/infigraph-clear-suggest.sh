#!/usr/bin/env bash
# Infigraph UserPromptSubmit hook — nudge /clear when real context usage is high.
# Reads actual usage from the statusline hook's per-session snapshot (statusline-command.sh
# writes context_window.{used_percentage,context_window_size,total_input_tokens} on every
# render, far more often than prompts are submitted); falls back to a transcript-size
# estimate only when no fresh snapshot exists. Priority: own event's context_window (not
# present on UserPromptSubmit as of this writing, checked defensively in case that changes)
# -> fresh (<=10min) statusline snapshot -> transcript-size estimate against this account's
# real window size (1,000,000 tokens -- NOT the 200k default; using 200k here previously
# produced nonsensical 191%/310%/387%-style readings). Fires at THRESHOLD_PCT (default 70),
# then suppresses repeats for REPEAT_TURNS turns (default 5) while still over threshold.

input=$(cat)
session_id=$(echo "$input" | jq -r '.session_id // empty')
[ -z "$session_id" ] && exit 0

# Teammate/subagent relays (SendMessage-delivered turns, e.g. idle notifications and status
# pings from background agents) arrive as UserPromptSubmit events just like real human input,
# but aren't a "user exchange" in the sense this hook means -- exclude them so the counter
# tracks actual turns with the human, not agent-orchestration chatter.
prompt=$(echo "$input" | jq -r '.prompt // empty')
case "$prompt" in
  *"<teammate-message"*) exit 0 ;;
esac

THRESHOLD_PCT="${INFIGRAPH_CLEAR_SUGGEST_THRESHOLD_PCT:-70}"
REPEAT_TURNS="${INFIGRAPH_CLEAR_SUGGEST_REPEAT_TURNS:-5}"
WINDOW_TOKENS=1000000

counter_dir="${TMPDIR:-/tmp}/claude-clear-suggest"
mkdir -p "$counter_dir" 2>/dev/null
counter_file="$counter_dir/$session_id.count"
fired_file="$counter_dir/$session_id.last_fired_turn"

count=0
[ -f "$counter_file" ] && count=$(cat "$counter_file" 2>/dev/null || echo 0)
count=$((count + 1))
echo "$count" > "$counter_file"

# 1) Own event's context_window field -- UserPromptSubmit doesn't carry this today
# (confirmed by watching it fire live), but check first in case that ever changes.
used_pct=$(echo "$input" | jq -r '.context_window.used_percentage // empty')

# 2) Fresh statusline snapshot -- the accurate common case. statusline-command.sh
# writes this atomically (temp+rename) on every render.
if [ -z "$used_pct" ]; then
  snapshot="$counter_dir/$session_id.statusline_usage.json"
  if [ -f "$snapshot" ]; then
    snap_ts=$(jq -r '.ts // empty' "$snapshot" 2>/dev/null)
    if [ -n "$snap_ts" ]; then
      now=$(date +%s)
      snap_ts_int=$(printf '%.0f' "$snap_ts" 2>/dev/null || echo 0)
      if [ "$snap_ts_int" -gt 0 ] && [ $((now - snap_ts_int)) -lt 600 ]; then
        used_pct=$(jq -r '.used_percentage // empty' "$snapshot" 2>/dev/null)
      fi
    fi
  fi
fi

# 3) Last resort: estimate from the transcript's raw size. ~4 bytes/token is a
# standard rough estimate for English/code text.
if [ -z "$used_pct" ]; then
  transcript_path=$(echo "$input" | jq -r '.transcript_path // empty')
  if [ -n "$transcript_path" ] && [ -f "$transcript_path" ]; then
    bytes=$(wc -c < "$transcript_path" 2>/dev/null | tr -d ' ')
    [ -n "$bytes" ] && used_pct=$(awk -v b="$bytes" -v w="$WINDOW_TOKENS" 'BEGIN{printf "%.0f", (b/4/w)*100}')
  fi
fi

[ -z "$used_pct" ] && exit 0
used_pct_int=$(printf '%.0f' "$used_pct" 2>/dev/null || echo 0)
[ "$used_pct_int" -ge "$THRESHOLD_PCT" ] || exit 0

last_fired=0
[ -f "$fired_file" ] && last_fired=$(cat "$fired_file" 2>/dev/null || echo 0)
# A /clear resets the turn counter (infigraph-session-start.sh's clear case) but not
# this file -- detect the rollback and treat it as no prior fire, rather than
# accidentally suppressing every nudge post-/clear until count climbs back past a
# stale high-water mark.
[ "$last_fired" -gt "$count" ] && last_fired=0
if [ "$last_fired" -gt 0 ] && [ $((count - last_fired)) -lt "$REPEAT_TURNS" ]; then
  exit 0
fi
echo "$count" > "$fired_file"

cat <<'ENDJSON'
{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"MANDATORY VISIBLE ACTION — CONTEXT CLEANUP: You MUST print this message verbatim to the user RIGHT NOW before doing anything else:\n\n---\n🧹 **Context getting long** — save session and type `/clear` to reset.\n---\n\nDo NOT skip this message. Do NOT silently absorb it. The user MUST see it in the chat output. After printing, continue with the user's request."}}
ENDJSON

exit 0
