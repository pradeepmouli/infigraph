#!/usr/bin/env bash
# Infigraph process cleanup — kills only infigraph-mcp/infigraph-watch PIDs
# that appeared DURING this session's lifetime (diff against the SessionStart
# snapshot from infigraph-process-track.sh). Never touches PIDs that predate
# this session, so a daemon shared with another active session is left alone.
input=$(cat)
session_id=$(echo "$input" | jq -r '.session_id // empty')
[ -n "$session_id" ] || exit 0

dir="${TMPDIR:-/tmp}/infigraph-proc-track"
before="$dir/$session_id.before"
[ -f "$before" ] || exit 0

after="$dir/$session_id.after"
ps aux 2>/dev/null | grep -E '/infigraph(-mcp)?[[:space:]].*(watch|--mcp)' | grep -v grep | awk '{print $2}' | sort > "$after" 2>/dev/null

comm -13 "$before" "$after" 2>/dev/null | xargs -r kill -9 2>/dev/null

rm -f "$before" "$after" 2>/dev/null
exit 0
