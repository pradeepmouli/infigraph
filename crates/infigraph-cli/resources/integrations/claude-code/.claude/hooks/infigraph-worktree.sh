#!/usr/bin/env bash
# Infigraph PostToolUse hook -- git worktree lifecycle.
# Fires after every Bash tool call; only acts when the command looks like a
# `git worktree add|remove|prune` invocation that actually succeeded (exit 0).
# Rather than parse the triggering command's own arguments (which may use a
# default worktree name, a relative path, or --force flags in any order), it
# re-runs `git worktree list --porcelain` before and after and diffs the two
# -- the authoritative source of what actually changed, regardless of how the
# command was phrased.
input=$(cat)

tool=$(echo "$input" | jq -r '.tool_name // empty')
[ "$tool" = "Bash" ] || exit 0

cmd=$(echo "$input" | jq -r '.tool_input.command // empty')
echo "$cmd" | grep -qE '(^|\s)git\s+worktree\s+(add|remove|prune)(\s|$)' || exit 0

exit_code=$(echo "$input" | jq -r '.tool_response.exitCode // 0')
[ "$exit_code" = "0" ] || exit 0

cwd=$(echo "$input" | jq -r '.cwd // empty')
[ -n "$cwd" ] || exit 0

command -v infigraph >/dev/null 2>&1 || exit 0
git -C "$cwd" rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0

infigraph worktree reconcile >/dev/null 2>&1 &
disown
exit 0
