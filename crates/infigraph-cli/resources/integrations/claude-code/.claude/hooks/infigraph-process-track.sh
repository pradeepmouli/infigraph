#!/usr/bin/env bash
# Infigraph process tracker — snapshots infigraph-mcp/infigraph-watch PIDs at
# session start so infigraph-process-cleanup.sh (SessionEnd) can kill only the
# ones THIS session spawned, never a daemon another concurrent session
# depends on (multiple sessions can share the same infigraph-mcp daemon).
input=$(cat)
session_id=$(echo "$input" | jq -r '.session_id // empty')
[ -n "$session_id" ] || exit 0

dir="${TMPDIR:-/tmp}/infigraph-proc-track"
mkdir -p "$dir" 2>/dev/null

ps aux 2>/dev/null | grep -E '/infigraph(-mcp)?[[:space:]].*(watch|--mcp)' | grep -v grep | awk '{print $2}' | sort > "$dir/$session_id.before" 2>/dev/null

exit 0
