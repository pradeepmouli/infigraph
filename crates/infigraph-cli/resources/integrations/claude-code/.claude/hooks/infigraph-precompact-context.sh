#!/usr/bin/env bash
# Infigraph PreCompact hook — injects the last saved session (verbatim, read
# from disk) into context right before compaction runs, so the compaction
# summary can lean on it instead of re-deriving the same context from
# scratch. Read-only: this hook does NOT trigger a save. Freshness of the
# saved data is enforced earlier, while the model can still act on it (the
# UserPromptSubmit 25-exchange nudge + CLAUDE.md's mandatory save triggers) —
# a PreCompact hook has no model turn to force a tool call with.
# stdin: JSON {session_id, cwd, trigger}  (trigger: "manual" | "auto")

input=$(cat)
cwd=$(echo "$input" | jq -r '.cwd // empty')

# Only enforce in Infigraph-indexed projects
[ -d "$cwd/.infigraph" ] || exit 0

# shellcheck source=infigraph-session-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/infigraph-session-lib.sh"

session_ctx=$(inject_session_summary)
[ -z "$session_ctx" ] && exit 0

escaped=$(printf '%s' "$session_ctx" | json_escape)
cat <<ENDJSON
{"hookSpecificOutput":{"hookEventName":"PreCompact","additionalContext":"BEFORE YOU SUMMARIZE: a saved session already exists on disk — use it as context so the compaction summary can reference it and focus on what's new, rather than re-deriving everything from scratch. $escaped"}}
ENDJSON
