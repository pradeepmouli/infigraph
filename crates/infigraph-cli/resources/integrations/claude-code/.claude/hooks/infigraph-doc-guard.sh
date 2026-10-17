#!/usr/bin/env bash
# PreToolUse hook for Edit/MultiEdit/Write: sends an edit of a glossary entry
# (a Markdown section headed by a symbol id) to doc_entry, which finds the exact
# lines and keeps the document index in step. Everything else passes.
#
# The decision is `infigraph doc-guard` (it reads the file; no store, no
# daemon). Only Markdown paths spawn it. Fails open on every error: a missing
# jq or infigraph, an unreadable input, a timeout -- the guard is a nudge, never
# a gate that can wedge an edit. It does not see the shell, so `sed -i` on a
# glossary is out of its reach.
input=$(cat)
file_path=$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty' 2>/dev/null)
case "$file_path" in
  *.md|*.MD|*.markdown) ;;
  *) exit 0 ;;
esac
command -v infigraph >/dev/null 2>&1 || exit 0
printf '%s' "$input" | infigraph doc-guard 2>/dev/null
exit 0
