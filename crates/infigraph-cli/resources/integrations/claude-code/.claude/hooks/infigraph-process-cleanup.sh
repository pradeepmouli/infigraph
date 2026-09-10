#!/usr/bin/env bash
# Infigraph process cleanup — kills only infigraph-mcp/infigraph-watch PIDs
# that are a direct child, or the worker one hop below (supervisor->worker),
# of THIS session's own `claude` process (recorded by
# infigraph-process-track.sh at SessionStart). Never touches a process
# spawned by a different session's `claude`, even one running in the same
# terminal/editor/login shell.
#
# History: an earlier version diffed a machine-wide PID snapshot by wall
# clock only ("kill anything new since my session started"), with no
# ownership check -- any infigraph-mcp process ANY other session spawned
# after this session's start looked "new" too, so closing one session could
# kill a completely unrelated session's live MCP server as collateral
# damage (observed 2026-08-28: another session's SessionEnd killed this
# session's own primary infigraph-mcp supervisor mid-conversation). A second
# attempt recorded a session's FULL ancestor chain instead -- also wrong,
# since two sessions in the same terminal app/login shell share ancestors
# above the `claude` process itself, reproducing the same bug one level up.
input=$(cat)
session_id=$(echo "$input" | jq -r '.session_id // empty')
[ -n "$session_id" ] || exit 0

dir="${TMPDIR:-/tmp}/infigraph-proc-track"
marker="$dir/$session_id.claude_pid"
[ -f "$marker" ] || exit 0
claude_pid=$(cat "$marker" 2>/dev/null)
[ -n "$claude_pid" ] || { rm -f "$marker"; exit 0; }

ps aux 2>/dev/null | grep -E '/infigraph(-mcp)?[[:space:]].*(watch|--mcp)' | grep -v grep | awk '{print $2}' | while read -r candidate_pid; do
    ppid=$(ps -o ppid= -p "$candidate_pid" 2>/dev/null | tr -d ' ')
    [ -n "$ppid" ] || continue
    if [ "$ppid" = "$claude_pid" ]; then
        # Direct child (the supervisor).
        kill -9 "$candidate_pid" 2>/dev/null
        continue
    fi
    gppid=$(ps -o ppid= -p "$ppid" 2>/dev/null | tr -d ' ')
    if [ -n "$gppid" ] && [ "$gppid" = "$claude_pid" ]; then
        # One hop below the supervisor (the worker).
        kill -9 "$candidate_pid" 2>/dev/null
    fi
done

rm -f "$marker"
exit 0
