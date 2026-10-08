#!/usr/bin/env bash
# Infigraph UserPromptSubmit hook — nudge /clear when real context usage is high.
# Reads actual usage from the statusline hook's per-session snapshot (statusline-command.sh
# writes context_window.{used_percentage,context_window_size,total_input_tokens} on every
# render). Priority: own event's context_window (not present on UserPromptSubmit as of this
# writing, checked defensively in case that changes) -> a fresh (<=10min) snapshot -> a
# transcript-size estimate, used ONLY when no snapshot was ever written for this session.
#
# A snapshot older than 10 minutes means "unknown", not "fall back to the transcript":
# the statusline renders only while the UI is active, so it goes stale across every long
# wait between prompts, and the transcript file keeps the whole pre-/compact history, so its
# size reads as far more than the live context (a session at 17% read as 173% and nudged on
# every fifth turn). Stale therefore stays silent. The estimate that remains counts from the
# last compaction boundary: that record's postTokens plus the bytes written since, at ~4
# bytes/token, against this account's real window (1,000,000 tokens, not the 200k default).
# Fires at THRESHOLD_PCT (default 70), then suppresses repeats for REPEAT_TURNS turns
# (default 5) while still over threshold.

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

# 2) Statusline snapshot -- the accurate common case. statusline-command.sh writes it
# atomically (temp+rename) on every render. A stale one is "unknown": exit, do not guess.
snapshot="$counter_dir/$session_id.statusline_usage.json"
if [ -z "$used_pct" ] && [ -f "$snapshot" ]; then
  snap_ts=$(jq -r '.ts // empty' "$snapshot" 2>/dev/null)
  snap_ts_int=$(printf '%.0f' "${snap_ts:-0}" 2>/dev/null || echo 0)
  [ $(( $(date +%s) - snap_ts_int )) -lt 600 ] || exit 0
  used_pct=$(jq -r '.used_percentage // empty' "$snapshot" 2>/dev/null)
fi

# 3) No snapshot has ever been written (statusline not installed): estimate from the
# transcript, counting from the last compaction boundary so history that /compact already
# dropped is not counted again.
if [ -z "$used_pct" ] && [ ! -f "$snapshot" ]; then
  transcript_path=$(echo "$input" | jq -r '.transcript_path // empty')
  if [ -n "$transcript_path" ] && [ -f "$transcript_path" ]; then
    boundary=$(awk '/"subtype":"compact_boundary"/{n=NR} END{print n+0}' "$transcript_path")
    base_tokens=0
    if [ "$boundary" -gt 0 ]; then
      base_tokens=$(sed -n "${boundary}p" "$transcript_path" | jq -r '.compactMetadata.postTokens // 0' 2>/dev/null)
      bytes=$(tail -n +"$boundary" "$transcript_path" | wc -c | tr -d ' ')
    else
      bytes=$(wc -c < "$transcript_path" 2>/dev/null | tr -d ' ')
    fi
    [ -n "$bytes" ] && used_pct=$(awk -v b="$bytes" -v t="${base_tokens:-0}" -v w="$WINDOW_TOKENS" 'BEGIN{printf "%.0f", ((t + b/4)/w)*100}')
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
