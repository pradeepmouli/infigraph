#!/usr/bin/env bash
# Infigraph PreToolUse + PostToolUse hook -- git worktree lifecycle.
# Registered for both events on Bash; only acts when the command looks like a
# `git worktree add|remove|prune` invocation. Rather than parse the command's
# own arguments (a default worktree name, a relative path, --force flags in
# any order), it snapshots `git worktree list --porcelain` before the command
# (PreToolUse, keyed by the call's tool_use_id) and diffs it against the list
# after (PostToolUse) -- the authoritative record of what actually changed:
#   - each added worktree:   `infigraph worktree init <path>`, but only when
#     the main worktree is an Infigraph project (has .infigraph/), so this
#     never starts indexing an arbitrary repo;
#   - each removed worktree: `infigraph worktree teardown <path>`, which
#     reaches the daemon over its socket, so it works after the directory is
#     gone;
#   - then `infigraph worktree reconcile`, as before.
# Everything runs detached (init indexes, which takes a while); output goes to
# <main>/.infigraph/worktree-hook.log when that directory exists. Without a
# snapshot (PreToolUse did not run) it falls back to reconcile alone. Never
# fails or blocks the tool call: every path exits 0.
input=$(cat)

tool=$(echo "$input" | jq -r '.tool_name // empty')
[ "$tool" = "Bash" ] || exit 0

cmd=$(echo "$input" | jq -r '.tool_input.command // empty')
echo "$cmd" | grep -qE '(^|\s)git\s+worktree\s+(add|remove|prune)(\s|$)' || exit 0

cwd=$(echo "$input" | jq -r '.cwd // empty')
[ -n "$cwd" ] || exit 0

event=$(echo "$input" | jq -r '.hook_event_name // empty')
id=$(echo "$input" | jq -r '.tool_use_id // empty' | tr -cd 'A-Za-z0-9_-')
snap_dir="${TMPDIR:-/tmp}/infigraph-worktree-hook"
snap=""
[ -n "$id" ] && snap="$snap_dir/$id.list"

list_worktrees() {
  git -C "$cwd" worktree list --porcelain 2>/dev/null | sed -n 's/^worktree //p'
}

if [ "$event" = "PreToolUse" ]; then
  [ -n "$snap" ] || exit 0
  git -C "$cwd" rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0
  mkdir -p "$snap_dir" 2>/dev/null || exit 0
  # A call that failed never reaches PostToolUse, so its snapshot is never
  # consumed; sweep those after an hour.
  find "$snap_dir" -name '*.list' -mmin +60 -delete 2>/dev/null
  list_worktrees >"$snap" 2>/dev/null || rm -f "$snap"
  exit 0
fi

before=""
if [ -n "$snap" ] && [ -f "$snap" ]; then
  before=$(cat "$snap")
  rm -f "$snap"
  have_snapshot=1
else
  have_snapshot=0
  exit_code=$(echo "$input" | jq -r '.tool_response.exitCode // 0')
  [ "$exit_code" = "0" ] || exit 0
fi

command -v infigraph >/dev/null 2>&1 || exit 0
git -C "$cwd" rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0

after=$(list_worktrees)
main=$(printf '%s\n' "$after" | head -n 1)

added=""
removed=""
if [ "$have_snapshot" = 1 ]; then
  # Lines in one list but not the other; paths never contain newlines here.
  added=$(printf '%s\n' "$after" | grep -vxF -f <(printf '%s\n' "$before") | grep -v '^$')
  removed=$(printf '%s\n' "$before" | grep -vxF -f <(printf '%s\n' "$after") | grep -v '^$')
  # Never index a repo that is not an Infigraph project.
  [ -n "$main" ] && [ -d "$main/.infigraph" ] || added=""
fi

log=/dev/null
[ -n "$main" ] && [ -d "$main/.infigraph" ] && log="$main/.infigraph/worktree-hook.log"

# One detached, sequential chain, because every step rewrites the registry.
# Reconcile runs last, so it no longer reports a just-initialised worktree as
# unindexed.
(
  cd "$cwd" || exit 0
  printf '%s\n' "$removed" | while IFS= read -r p; do
    [ -n "$p" ] && infigraph worktree teardown "$p"
  done
  printf '%s\n' "$added" | while IFS= read -r p; do
    [ -n "$p" ] && infigraph worktree init "$p"
  done
  infigraph worktree reconcile
) </dev/null >>"$log" 2>&1 &
disown
exit 0
