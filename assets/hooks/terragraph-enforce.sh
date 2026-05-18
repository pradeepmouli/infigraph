#!/usr/bin/env bash
# Infigraph PreToolUse enforcement hook
# Warns when raw search/file tools are used in Infigraph-indexed projects.
# stdin: JSON {tool_name, tool_input, cwd}
# exit 0 = allow (with warning on stderr)

input=$(cat)
tool=$(echo "$input" | jq -r '.tool_name // empty')
cwd=$(echo "$input" | jq -r '.cwd // empty')

# Guard: only enforce in projects with a .infigraph directory
[ -d "$cwd/.infigraph" ] || exit 0

case "$tool" in
  Grep)
    echo "WARNING: Prefer mcp__infigraph__search (unified search) over Grep. Infigraph is indexed for this project." >&2
    ;;
  Glob)
    echo "WARNING: Prefer mcp__infigraph__list_files over Glob. Infigraph is indexed for this project." >&2
    ;;
  Bash)
    cmd=$(echo "$input" | jq -r '.tool_input.command // empty')
    if echo "$cmd" | grep -qE '(^|\s|/)(grep|egrep|fgrep|rg|ripgrep|ag|ack)(\s|$)'; then
      echo "WARNING: Prefer mcp__infigraph__search over grep/rg. Infigraph is indexed for this project." >&2
    fi
    if echo "$cmd" | grep -qE '(^|\s)find\s.*-name\s'; then
      echo "WARNING: Prefer mcp__infigraph__list_files over find. Infigraph is indexed for this project." >&2
    fi
    ;;
esac

exit 0
