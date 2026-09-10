#!/usr/bin/env bash
# Infigraph process tracker — records the PID of THIS session's own `claude`
# process at session start, so infigraph-process-cleanup.sh (SessionEnd) can
# kill only infigraph-mcp/infigraph-watch processes that are direct (or
# one-hop, supervisor->worker) children of that exact process, never a
# daemon another concurrent session depends on.
#
# Earlier versions either (a) diffed a machine-wide PID snapshot by wall
# clock, or (b) recorded a session's FULL ancestor chain -- both wrong: (a)
# has no ownership check at all, (b) climbs past the per-session `claude`
# process into shared ancestors (the same terminal app/login shell can be
# the ancestor of MULTIPLE unrelated sessions' `claude` processes), which
# reproduces the exact same cross-session kill bug one level higher. Only
# the single `claude` PID itself is a safe, precise anchor: infigraph-mcp is
# always its DIRECT child (verified empirically), never several hops removed
# through shared terminal/login/editor infrastructure.
input=$(cat)
session_id=$(echo "$input" | jq -r '.session_id // empty')
[ -n "$session_id" ] || exit 0

dir="${TMPDIR:-/tmp}/infigraph-proc-track"
mkdir -p "$dir" 2>/dev/null

# Walk up from this hook's own parent looking for the process literally
# named `claude` -- the actual per-session CLI process, whether this hook
# runs as its direct child or through a short-lived `sh -c` wrapper. Stop
# and record ONLY that one PID; never record anything above it.
pid=$PPID
depth=0
claude_pid=""
while [ -n "$pid" ] && [ "$pid" != "0" ] && [ "$pid" != "1" ] && [ "$depth" -lt 16 ]; do
    if [ "$(ps -o comm= -p "$pid" 2>/dev/null)" = "claude" ]; then
        claude_pid="$pid"
        break
    fi
    pid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')
    depth=$((depth + 1))
done

# If it can't be found, don't write anything -- cleanup bails out safely
# (no file => no kills) rather than guess with a broader, riskier match.
if [ -n "$claude_pid" ]; then
    echo "$claude_pid" > "$dir/$session_id.claude_pid"
fi

exit 0
